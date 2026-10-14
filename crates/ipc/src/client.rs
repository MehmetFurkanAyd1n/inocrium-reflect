use crate::proto::RpcError;
use serde_json::Value;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Administrator privileges required to access the service")]
    AccessDenied,
    #[error("Service disconnected")]
    Disconnected,
    #[error("Request timed out")]
    Timeout,
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Io(std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        if error.raw_os_error() == Some(5) {
            Self::AccessDenied
        } else {
            Self::Io(error)
        }
    }
}
#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::{
        framing, proto,
        transport::{self, Pipe, PipeStream},
    };
    use crossbeam_channel::{bounded, Receiver, Sender};
    use parking_lot::Mutex;
    use serde::{de::DeserializeOwned, Serialize};
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        thread::{self, JoinHandle},
        time::Duration,
    };
    type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, Error>>>>>;
    pub struct Client {
        pipe: Arc<Pipe>,
        writer: Mutex<PipeStream>,
        pending: Pending,
        notifications: Receiver<Notification>,
        next: AtomicU64,
        reader: Option<JoinHandle<()>>,
    }
    impl Client {
        pub fn connect(pipe_name: &str, timeout: Duration) -> Result<Self, Error> {
            let pipe = transport::connect(pipe_name, timeout)?;
            let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
            let (notifications, notification_rx) = bounded(256);
            let state = pending.clone();
            let read_pipe = pipe.clone();
            let reader = thread::Builder::new().name("ipc-client-reader".into()).spawn(move || {
                let mut stream = PipeStream(read_pipe.clone());
                while let Ok(body) = framing::read_frame(&mut stream) {
                    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
                        break;
                    };
                    if message["jsonrpc"] != "2.0" {
                        break;
                    }
                    if let Some(id) = message.get("id").and_then(Value::as_u64) {
                        if let Some(sender) = state.lock().remove(&id) {
                            let response = if let Some(error) = message.get("error") {
                                serde_json::from_value::<RpcError>(error.clone())
                                    .map_or_else(|e| Err(Error::Json(e)), |e| Err(Error::Rpc(e)))
                            } else if let Some(result) = message.get("result") {
                                Ok(result.clone())
                            } else {
                                Err(Error::Disconnected)
                            };
                            let _ = sender.try_send(response);
                        }
                    } else if let Some(method) = message.get("method").and_then(Value::as_str) {
                        if notifications
                            .try_send(Notification {
                                method: method.into(),
                                params: message.get("params").cloned().unwrap_or(Value::Null),
                            })
                            .is_err()
                        {
                            // Do not silently lose progress/state: disconnect and let caller refresh after reconnect.
                            break;
                        }
                    }
                }
                read_pipe.stop();
                for (_, sender) in state.lock().drain() {
                    let _ = sender.try_send(Err(Error::Disconnected));
                }
            })?;
            Ok(Self {
                writer: Mutex::new(PipeStream(pipe.clone())),
                pipe,
                pending,
                notifications: notification_rx,
                next: AtomicU64::new(1),
                reader: Some(reader),
            })
        }
        pub fn call<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: P) -> Result<R, Error> {
            self.call_with_timeout(method, params, Duration::from_secs(30))
        }
        pub fn call_with_timeout<P: Serialize, R: DeserializeOwned>(
            &self,
            method: &str,
            params: P,
            timeout: Duration,
        ) -> Result<R, Error> {
            if self.pipe.stopped.load(Ordering::Acquire) {
                return Err(Error::Disconnected);
            }
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            let body =
                serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))?;
            let (sender, receiver) = bounded(1);
            self.pending.lock().insert(id, sender);
            if let Err(error) = framing::write_frame(&mut *self.writer.lock(), &body) {
                self.pending.lock().remove(&id);
                return Err(error.into());
            }
            let response = receiver.recv_timeout(timeout);
            self.pending.lock().remove(&id);
            let value = response.map_err(|e| match e {
                crossbeam_channel::RecvTimeoutError::Timeout => Error::Timeout,
                _ => Error::Disconnected,
            })??;
            Ok(serde_json::from_value(value)?)
        }
        /// Clones share one queue; use a single consumer to receive every notification.
        pub fn notifications(&self) -> Receiver<Notification> {
            self.notifications.clone()
        }
        pub fn is_connected(&self) -> bool {
            !self.pipe.stopped.load(Ordering::Acquire)
        }
    }
    impl Drop for Client {
        fn drop(&mut self) {
            self.pipe.stop();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }
    pub fn is_service_running() -> bool {
        transport::exists(proto::PIPE_NAME)
    }
    pub fn is_pipe_running(pipe_name: &str) -> bool {
        transport::exists(pipe_name)
    }
}
#[cfg(windows)]
pub use platform::{is_pipe_running, is_service_running, Client};
