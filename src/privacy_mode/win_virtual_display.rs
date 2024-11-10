use super::{PrivacyMode, PrivacyModeState, INVALID_PRIVACY_MODE_CONN_ID, NO_PHYSICAL_DISPLAYS};
use crate::{platform::windows::reg_display_settings, virtual_display_manager};
use hbb_common::{allow_err, bail, config::Config, log, ResultType};
use std::{
    io::Error,
    ops::{Deref, DerefMut},
    thread,
    time::Duration,
};
use virtual_display::MonitorMode;
use winapi::{
    shared::{
        minwindef::{DWORD, FALSE},
        ntdef::{NULL, WCHAR},
    },
    um::{
        wingdi::{
            DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP,
            DISPLAY_DEVICE_MIRRORING_DRIVER, DISPLAY_DEVICE_PRIMARY_DEVICE, DM_POSITION,
        },
        winuser::{
            ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsExW,
            EnumDisplaySettingsW, CDS_NORESET, CDS_RESET, CDS_SET_PRIMARY, CDS_UPDATEREGISTRY,
            DISP_CHANGE_FAILED, DISP_CHANGE_SUCCESSFUL, EDD_GET_DEVICE_INTERFACE_NAME,
            ENUM_CURRENT_SETTINGS, ENUM_REGISTRY_SETTINGS,
        },
    },
};

pub(super) const PRIVACY_MODE_IMPL: &str = super::PRIVACY_MODE_IMPL_WIN_VIRTUAL_DISPLAY;

const CONFIG_KEY_REG_RECOVERY: &str = "reg_recovery";

struct Display {
    dm: DEVMODEW,
    name: [WCHAR; 32],
    primary: bool,
}

pub struct PrivacyModeImpl {
    impl_key: String,
    conn_id: i32,
    displays: Vec<Display>,
    virtual_displays: Vec<Display>,
    virtual_displays_added: Vec<u32>,
}

struct TurnOnGuard<'a> {
    privacy_mode: &'a mut PrivacyModeImpl,
    succeeded: bool,
}

impl<'a> Deref for TurnOnGuard<'a> {
    type Target = PrivacyModeImpl;

    fn deref(&self) -> &Self::Target {
        self.privacy_mode
    }
}

impl<'a> DerefMut for TurnOnGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.privacy_mode
    }
}

impl<'a> Drop for TurnOnGuard<'a> {
    fn drop(&mut self) {
        if !self.succeeded {
            self.privacy_mode
                .turn_off_privacy(INVALID_PRIVACY_MODE_CONN_ID, None)
                .ok();
        }
    }
}

impl PrivacyModeImpl {
    pub fn new(impl_key: &str) -> Self {
        Self {
            impl_key: impl_key.to_owned(),
            conn_id: INVALID_PRIVACY_MODE_CONN_ID,
            displays: Vec::new(),
            virtual_displays: Vec::new(),
            virtual_displays_added: Vec::new(),
        }
    }

    // mainly from https://github.com/rustdesk-org/rustdesk/blob/44c3a52ca8502cf53b58b59db130611778d34dbe/libs/scrap/src/dxgi/mod.rs#L365
    fn set_displays(&mut self) {
        self.displays.clear();
        self.virtual_displays.clear();

        let mut i: DWORD = 0;
        loop {
            #[allow(invalid_value)]
            let mut dd: DISPLAY_DEVICEW = unsafe { std::mem::MaybeUninit::uninit().assume_init() };
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as _;
            let ok = unsafe { EnumDisplayDevicesW(std::ptr::null(), i, &mut dd as _, 0) };
            if ok == FALSE {
                break;
            }
            i += 1;
            if 0 == (dd.StateFlags & DISPLAY_DEVICE_ACTIVE)
                || (dd.StateFlags & DISPLAY_DEVICE_MIRRORING_DRIVER) > 0
            {
                continue;
            }
            #[allow(invalid_value)]
            let mut dm: DEVMODEW = unsafe { std::mem::MaybeUninit::uninit().assume_init() };
            dm.dmSize = std::mem::size_of::<DEVMODEW>() as _;
            dm.dmDriverExtra = 0;
            unsafe {
                if FALSE
                    == EnumDisplaySettingsExW(
                        dd.DeviceName.as_ptr(),
                        ENUM_CURRENT_SETTINGS,
                        &mut dm as _,
                        0,
                    )
                {
                    if FALSE
                        == EnumDisplaySettingsExW(
                            dd.DeviceName.as_ptr(),
                            ENUM_REGISTRY_SETTINGS,
                            &mut dm as _,
                            0,
                        )
                    {
                        continue;
                    }
                }
            }

            let primary = (dd.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE) > 0;
            let display = Display {
                dm,
                name: dd.DeviceName,
                primary,
            };

            let ds = virtual_display_manager::get_cur_device_string();
            if let Ok(s) = String::from_utf16(&dd.DeviceString) {
                if s.len() >= ds.len() && &s[..ds.len()] == ds {
                    self.virtual_displays.push(display);
                    continue;
                }
            }
            self.displays.push(display);
        }
    }

    fn restore_plug_out_monitor(&mut self) {
        let _ = virtual_display_manager::plug_out_monitor_indices(
            &self.virtual_displays_added,
            true,
            false,
        );
        self.virtual_displays_added.clear();
    }

    #[inline]
    fn change_display_settings_ex_err_msg(rc: i32) -> String {
        if rc != DISP_CHANGE_FAILED {
            format!("ret: {}", rc)
        } else {
            format!(
                "ret: {}, last error: {:?}",
                rc,
                std::io::Error::last_os_error()
            )
        }
    }

    fn set_primary_display(&mut self) -> ResultType<()> {
        let display = &self.virtual_displays[0];

        #[allow(invalid_value)]
        let mut new_primary_dm: DEVMODEW = unsafe { std::mem::MaybeUninit::uninit().assume_init() };
        new_primary_dm.dmSize = std::mem::size_of::<DEVMODEW>() as _;
        new_primary_dm.dmDriverExtra = 0;
        unsafe {
            if FALSE
                == EnumDisplaySettingsW(
                    display.name.as_ptr(),
                    ENUM_CURRENT_SETTINGS,
                    &mut new_primary_dm,
                )
            {
                bail!(
                    "Failed EnumDisplaySettingsW, device name: {:?}, error: {}",
                    std::string::String::from_utf16(&display.name),
                    Error::last_os_error()
                );
            }

            let mut i: DWORD = 0;
            loop {
                let mut flags = CDS_UPDATEREGISTRY | CDS_NORESET;
                #[allow(invalid_value)]
                let mut dd: DISPLAY_DEVICEW = std::mem::MaybeUninit::uninit().assume_init();
                dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as _;
                if FALSE
                    == EnumDisplayDevicesW(NULL as _, i, &mut dd, EDD_GET_DEVICE_INTERFACE_NAME)
                {
                    break;
                }
                i += 1;
                if (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) == 0 {
                    continue;
                }

                if dd.DeviceName == display.name {
                    flags |= CDS_SET_PRIMARY;
                }

                #[allow(invalid_value)]
                let mut dm: DEVMODEW = std::mem::MaybeUninit::uninit().assume_init();
                dm.dmSize = std::mem::size_of::<DEVMODEW>() as _;
                dm.dmDriverExtra = 0;
                if FALSE
                    == EnumDisplaySettingsW(dd.DeviceName.as_ptr(), ENUM_CURRENT_SETTINGS, &mut dm)
                {
                    bail!(
                        "Failed EnumDisplaySettingsW, device name: {:?}, error: {}",
                        std::string::String::from_utf16(&dd.DeviceName),
                        Error::last_os_error()
                    );
                }

                dm.u1.s2_mut().dmPosition.x -= new_primary_dm.u1.s2().dmPosition.x;
                dm.u1.s2_mut().dmPosition.y -= new_primary_dm.u1.s2().dmPosition.y;
                dm.dmFields |= DM_POSITION;
                dm.dmPelsWidth = 1920;
                dm.dmPelsHeight = 1080;
                let rc = ChangeDisplaySettingsExW(
                    dd.DeviceName.as_ptr(),
                    &mut dm,
                    NULL as _,
                    flags,
                    NULL,
                );
                if rc != DISP_CHANGE_SUCCESSFUL {
                    let err = Self::change_display_settings_ex_err_msg(rc);
                    log::error!(
                        "Failed ChangeDisplaySettingsEx, device name: {:?}, flags: {}, {}",
                        std::string::String::from_utf16(&dd.DeviceName),
                        flags,
                        &err
                    );
                    bail!("Failed ChangeDisplaySettingsEx, {}", err);
                }

                // If we want to set dpi, the following references may be helpful.
                // And setting dpi should be called after changing the display settings.
                // https://stackoverflow.com/questions/35233182/how-can-i-change-windows-10-display-scaling-programmatically-using-c-sharp
                // https://github.com/lihas/windows-DPI-scaling-sample/blob/master/DPIHelper/DpiHelper.cpp
                //
                // But the official API does not provide a way to get/set dpi.
                // https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ne-wingdi-displayconfig_device_info_type
                // https://github.com/lihas/windows-DPI-scaling-sample/blob/738ac18b7a7ce2d8fdc157eb825de9cb5eee0448/DPIHelper/DpiHelper.h#L37
            }
        }

        Ok(())
    }

    fn disable_physical_displays(&self) -> ResultType<()> {
        for display in &self.displays {
            let mut dm = display.dm.clone();
            unsafe {
                dm.u1.s2_mut().dmPosition.x = 10000;
                dm.u1.s2_mut().dmPosition.y = 10000;
                dm.dmPelsHeight = 0;
                dm.dmPelsWidth = 0;
                let flags = CDS_UPDATEREGISTRY | CDS_NORESET;
                let rc = ChangeDisplaySettingsExW(
                    display.name.as_ptr(),
                    &mut dm,
                    NULL as _,
                    flags,
                    NULL as _,
                );
                if rc != DISP_CHANGE_SUCCESSFUL {
                    let err = Self::change_display_settings_ex_err_msg(rc);
                    log::error!(
                        "Failed ChangeDisplaySettingsEx, device name: {:?}, flags: {}, {}",
                        std::string::String::from_utf16(&display.name),
                        flags,
                        &err
                    );
                    bail!("Failed ChangeDisplaySettingsEx, {}", err);
                }
            }
        }
        Ok(())
    }

    #[inline]
    fn default_display_modes() -> Vec<MonitorMode> {
        vec![MonitorMode {
            width: 1920,
            height: 1080,
            sync: 60,
        }]
    }

    pub fn ensure_virtual_display(&mut self) -> ResultType<()> {
        if self.virtual_displays.is_empty() {
            let displays =
                virtual_display_manager::plug_in_peer_request(vec![Self::default_display_modes()])?;
            if virtual_display_manager::is_amyuni_idd() {
                thread::sleep(Duration::from_secs(3));
            }
            self.set_displays();

            // No physical displays, no need to use the privacy mode.
            if self.displays.is_empty() {
                virtual_display_manager::plug_out_monitor_indices(&displays, false, false)?;
                bail!(NO_PHYSICAL_DISPLAYS);
            }

            self.virtual_displays_added.extend(displays);
        }

        Ok(())
    }

    #[inline]
    fn commit_change_display(flags: DWORD) -> ResultType<()> {
        unsafe {
            // use winapi::{
            //     shared::windef::HDESK,
            //     um::{
            //         processthreadsapi::GetCurrentThreadId,
            //         winnt::MAXIMUM_ALLOWED,
            //         winuser::{CloseDesktop, GetThreadDesktop, OpenInputDesktop, SetThreadDesktop},
            //     },
            // };
            // let mut desk_input: HDESK = NULL as _;
            // let desk_current: HDESK = GetThreadDesktop(GetCurrentThreadId());
            // if !desk_current.is_null() {
            //     desk_input = OpenInputDesktop(0, FALSE, MAXIMUM_ALLOWED);
            //     if desk_input.is_null() {
            //         SetThreadDesktop(desk_input);
            //     }
            // }

            let rc = ChangeDisplaySettingsExW(NULL as _, NULL as _, NULL as _, flags, NULL as _);
            if rc != DISP_CHANGE_SUCCESSFUL {
                let err = Self::change_display_settings_ex_err_msg(rc);
                bail!("Failed ChangeDisplaySettingsEx, {}", err);
            }

            // if !desk_current.is_null() {
            //     SetThreadDesktop(desk_current);
            // }
            // if !desk_input.is_null() {
            //     CloseDesktop(desk_input);
            // }
        }
        Ok(())
    }

    fn restore(&mut self) {
        Self::restore_displays(&self.displays);
        Self::restore_displays(&self.virtual_displays);
        allow_err!(Self::commit_change_display(0));
        self.restore_plug_out_monitor();
        self.displays.clear();
        self.virtual_displays.clear();
    }

    fn restore_displays(displays: &[Display]) {
        for display in displays {
            unsafe {
                let mut dm = display.dm.clone();
                let flags = if display.primary {
                    CDS_NORESET | CDS_UPDATEREGISTRY | CDS_SET_PRIMARY
                } else {
                    CDS_NORESET | CDS_UPDATEREGISTRY
                };
                ChangeDisplaySettingsExW(
                    display.name.as_ptr(),
                    &mut dm,
                    std::ptr::null_mut(),
                    flags,
                    std::ptr::null_mut(),
                );
            }
        }
    }
}

impl PrivacyMode for PrivacyModeImpl {
    fn is_async_privacy_mode(&self) -> bool {
        virtual_display_manager::is_amyuni_idd()
    }

    fn init(&self) -> ResultType<()> {
        Ok(())
    }

    fn clear(&mut self) {
        allow_err!(self.turn_off_privacy(self.conn_id, None));
    }

    fn turn_on_privacy(&mut self, conn_id: i32) -> ResultType<bool> {
        if !virtual_display_manager::is_virtual_display_supported() {
            bail!("idd_not_support_under_win10_2004_tip");
        }

        if self.check_on_conn_id(conn_id)? {
            log::debug!("Privacy mode of conn {} is already on", conn_id);
            return Ok(true);
        }
        self.set_displays();
        if self.displays.is_empty() {
            log::debug!("{}", NO_PHYSICAL_DISPLAYS);
            bail!(NO_PHYSICAL_DISPLAYS);
        }

        let mut guard = TurnOnGuard {
            privacy_mode: self,
            succeeded: false,
        };

        guard.ensure_virtual_display()?;
        if guard.virtual_displays.is_empty() {
            log::debug!("No virtual displays");
            bail!("No virtual displays.");
        }

        let reg_connectivity_1 = reg_display_settings::read_reg_connectivity()?;
        guard.set_primary_display()?;
        guard.disable_physical_displays()?;
        Self::commit_change_display(CDS_RESET)?;
        let reg_connectivity_2 = reg_display_settings::read_reg_connectivity()?;

        if let Some(reg_recovery) =
            reg_display_settings::diff_recent_connectivity(reg_connectivity_1, reg_connectivity_2)
        {
            Config::set_option(
                CONFIG_KEY_REG_RECOVERY.to_owned(),
                serde_json::to_string(&reg_recovery)?,
            );
        } else {
            reset_config_reg_connectivity();
        };

        // OpenInputDesktop and block the others' input ?
        guard.conn_id = conn_id;
        guard.succeeded = true;

        allow_err!(super::win_input::hook());

        Ok(true)
    }

    fn turn_off_privacy(
        &mut self,
        conn_id: i32,
        state: Option<PrivacyModeState>,
    ) -> ResultType<()> {
        self.check_off_conn_id(conn_id)?;
        super::win_input::unhook()?;
        let _tmp_ignore_changed_holder = crate::display_service::temp_ignore_displays_changed();
        self.restore();
        restore_reg_connectivity(false);

        if self.conn_id != INVALID_PRIVACY_MODE_CONN_ID {
            if let Some(state) = state {
                allow_err!(super::set_privacy_mode_state(
                    conn_id,
                    state,
                    PRIVACY_MODE_IMPL.to_string(),
                    1_000
                ));
            }
            self.conn_id = INVALID_PRIVACY_MODE_CONN_ID.to_owned();
        }

        Ok(())
    }

    #[inline]
    fn pre_conn_id(&self) -> i32 {
        self.conn_id
    }

    #[inline]
    fn get_impl_key(&self) -> &str {
        &self.impl_key
    }
}

impl Drop for PrivacyModeImpl {
    fn drop(&mut self) {
        if self.conn_id != INVALID_PRIVACY_MODE_CONN_ID {
            allow_err!(self.turn_off_privacy(self.conn_id, None));
        }
    }
}

#[inline]
fn reset_config_reg_connectivity() {
    Config::set_option(CONFIG_KEY_REG_RECOVERY.to_owned(), "".to_owned());
}

pub fn restore_reg_connectivity(plug_out_monitors: bool) {
    let config_recovery_value = Config::get_option(CONFIG_KEY_REG_RECOVERY);
    if config_recovery_value.is_empty() {
        return;
    }
    if plug_out_monitors {
        let _ = virtual_display_manager::plug_out_monitor(-1, true, false);
    }
    if let Ok(reg_recovery) =
        serde_json::from_str::<reg_display_settings::RegRecovery>(&config_recovery_value)
    {
        if let Err(e) = reg_display_settings::restore_reg_connectivity(reg_recovery) {
            log::error!("Failed restore_reg_connectivity, error: {}", e);
        }
    }
    reset_config_reg_connectivity();
}
