#[allow(dead_code)]
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};

#[cfg(any(target_os = "windows", feature = "unix-file-copy-paste",))]
use hbb_common::{allow_err, bail};
use hbb_common::{
    lazy_static,
    tokio::sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Mutex as TokioMutex,
    },
    ResultType,
};
use serde_derive::{Deserialize, Serialize};
use thiserror::Error;

pub mod context_send;
pub mod platform;
pub use context_send::*;

#[cfg(target_os = "windows")]
const ERR_CODE_SERVER_FUNCTION_NONE: u32 = 0x00000001;
#[cfg(target_os = "windows")]
const ERR_CODE_INVALID_PARAMETER: u32 = 0x00000002;
#[cfg(target_os = "windows")]
const ERR_CODE_SEND_MSG: u32 = 0x00000003;

pub(crate) use platform::create_cliprdr_context;

/// Ability to handle Clipboard File from remote rustdesk client
///
/// # Note
/// There actually should be 2 parts to implement a useable clipboard file service,
/// but this only contains the RPC server part.
/// The local listener and transport part is too platform specific to wrap up in typeclasses.
pub trait CliprdrServiceContext: Send + Sync {
    /// set to be stopped
    fn set_is_stopped(&mut self) -> Result<(), CliprdrError>;
    /// clear the content on clipboard
    fn empty_clipboard(&mut self, conn_id: i32) -> Result<bool, CliprdrError>;

    /// run as a server for clipboard RPC
    fn server_clip_file(&mut self, conn_id: i32, msg: ClipboardFile) -> Result<(), CliprdrError>;
}

#[derive(Error, Debug)]
pub enum CliprdrError {
    #[error("invalid cliprdr name")]
    CliprdrName,
    #[error("failed to init cliprdr")]
    CliprdrInit,
    #[error("cliprdr out of memory")]
    CliprdrOutOfMemory,
    #[error("cliprdr internal error")]
    ClipboardInternalError,
    #[error("cliprdr occupied")]
    ClipboardOccupied,
    #[error("conversion failure")]
    ConversionFailure,
    #[error("failure to read clipboard")]
    OpenClipboard,
    #[error("failure to read file metadata or content")]
    FileError { path: PathBuf, err: std::io::Error },
    #[error("invalid request")]
    InvalidRequest { description: String },
    #[error("unknown cliprdr error")]
    Unknown(u32),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum ClipboardFile {
    NotifyCallback {
        r#type: String,
        title: String,
        text: String,
    },
    MonitorReady,
    FormatList {
        format_list: Vec<(i32, String)>,
    },
    FormatListResponse {
        msg_flags: i32,
    },
    FormatDataRequest {
        requested_format_id: i32,
    },
    FormatDataResponse {
        msg_flags: i32,
        format_data: Vec<u8>,
    },
    FileContentsRequest {
        stream_id: i32,
        list_index: i32,
        dw_flags: i32,
        n_position_low: i32,
        n_position_high: i32,
        cb_requested: i32,
        have_clip_data_id: bool,
        clip_data_id: i32,
    },
    FileContentsResponse {
        msg_flags: i32,
        stream_id: i32,
        requested_data: Vec<u8>,
    },
}

struct MsgChannel {
    peer_id: String,
    conn_id: i32,
    #[allow(dead_code)]
    sender: UnboundedSender<ClipboardFile>,
    receiver: Arc<TokioMutex<UnboundedReceiver<ClipboardFile>>>,
}

lazy_static::lazy_static! {
    static ref VEC_MSG_CHANNEL: RwLock<Vec<MsgChannel>> = Default::default();
    static ref CLIENT_CONN_ID_COUNTER: Mutex<i32> = Mutex::new(0);
}

impl ClipboardFile {
    pub fn is_stopping_allowed(&self) -> bool {
        matches!(
            self,
            ClipboardFile::MonitorReady
                | ClipboardFile::FormatList { .. }
                | ClipboardFile::FormatDataRequest { .. }
        )
    }

    pub fn is_beginning_message(&self) -> bool {
        matches!(
            self,
            ClipboardFile::MonitorReady | ClipboardFile::FormatList { .. }
        )
    }
}

pub fn get_client_conn_id(peer_id: &str) -> Option<i32> {
    VEC_MSG_CHANNEL
        .read()
        .unwrap()
        .iter()
        .find(|x| x.peer_id == peer_id)
        .map(|x| x.conn_id)
}

fn get_conn_id() -> i32 {
    let mut lock = CLIENT_CONN_ID_COUNTER.lock().unwrap();
    *lock += 1;
    *lock
}

pub fn get_rx_cliprdr_client(
    peer_id: &str,
) -> (i32, Arc<TokioMutex<UnboundedReceiver<ClipboardFile>>>) {
    let mut lock = VEC_MSG_CHANNEL.write().unwrap();
    match lock.iter().find(|x| x.peer_id == peer_id) {
        Some(msg_channel) => (msg_channel.conn_id, msg_channel.receiver.clone()),
        None => {
            let (sender, receiver) = unbounded_channel();
            let receiver = Arc::new(TokioMutex::new(receiver));
            let receiver2 = receiver.clone();
            let conn_id = get_conn_id();
            let msg_channel = MsgChannel {
                peer_id: peer_id.to_owned(),
                conn_id,
                sender,
                receiver,
            };
            lock.push(msg_channel);
            (conn_id, receiver2)
        }
    }
}

pub fn get_rx_cliprdr_server(conn_id: i32) -> Arc<TokioMutex<UnboundedReceiver<ClipboardFile>>> {
    let mut lock = VEC_MSG_CHANNEL.write().unwrap();
    match lock.iter().find(|x| x.conn_id == conn_id) {
        Some(msg_channel) => msg_channel.receiver.clone(),
        None => {
            let (sender, receiver) = unbounded_channel();
            let receiver = Arc::new(TokioMutex::new(receiver));
            let receiver2 = receiver.clone();
            let msg_channel = MsgChannel {
                peer_id: "".to_string(),
                conn_id,
                sender,
                receiver,
            };
            lock.push(msg_channel);
            receiver2
        }
    }
}

#[cfg(any(target_os = "windows", feature = "unix-file-copy-paste",))]
#[inline]
fn send_data(conn_id: i32, data: ClipboardFile) -> ResultType<()> {
    #[cfg(target_os = "windows")]
    return send_data_to_channel(conn_id, data);
    #[cfg(not(target_os = "windows"))]
    if conn_id == 0 {
        send_data_to_all(data);
    } else {
        send_data_to_channel(conn_id, data);
    }
}
#[cfg(any(target_os = "windows", feature = "unix-file-copy-paste",))]
#[inline]
fn send_data_to_channel(conn_id: i32, data: ClipboardFile) -> ResultType<()> {
    if let Some(msg_channel) = VEC_MSG_CHANNEL
        .read()
        .unwrap()
        .iter()
        .find(|x| x.conn_id == conn_id)
    {
        msg_channel.sender.send(data)?;
        Ok(())
    } else {
        bail!("conn_id not found");
    }
}

#[cfg(feature = "unix-file-copy-paste")]
#[inline]
fn send_data_to_all(data: ClipboardFile) -> ResultType<()> {
    // Need more tests to see if it's necessary to handle the error.
    for msg_channel in VEC_MSG_CHANNEL.read().unwrap().iter() {
        allow_err!(msg_channel.sender.send(data.clone()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // #[test]
    // fn test_cliprdr_run() {
    //     super::cliprdr_run();
    // }
}
