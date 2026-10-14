use crate::proto::{self, RpcError};
use serde_json::Value;
use std::{io, sync::Arc};

#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub process_id: u32,
    pub is_admin: bool,
}
pub trait Handler: Send + Sync {
    fn call(&self, method: &str, params: Value, client: &ClientInfo) -> Result<Value, RpcError>;
}
/// Adapter for a typed service implementation.
pub struct ServiceHandler<T: proto::Service>(pub T);
impl<T: proto::Service> Handler for ServiceHandler<T> {
    fn call(&self, method: &str, params: Value, client: &ClientInfo) -> Result<Value, RpcError> {
        proto::dispatch(&self.0, method, params, client)
    }
}
#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub require_admin: bool,
    pub sddl: String,
}
impl Default for ServerOptions {
    fn default() -> Self {
        Self { require_admin: true, sddl: "D:P(A;;GA;;;BA)(A;;GA;;;SY)".into() }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::{
        framing,
        transport::{self, Pipe, PipeStream},
    };
    use crossbeam_channel::{bounded, Sender};
    use parking_lot::Mutex;
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicBool, Ordering},
        thread::{self, JoinHandle},
        time::Duration,
    };
    struct Connection {
        pipe: Arc<Pipe>,
        sender: Sender<Arc<[u8]>>,
    }
    struct Shared {
        stopped: AtomicBool,
        clients: Mutex<HashMap<u64, Connection>>,
        listener: Mutex<Option<Arc<Pipe>>>,
    }
    #[derive(Clone)]
    pub struct Notifier {
        shared: Arc<Shared>,
    }
    impl Notifier {
        /// Queues once per client without waiting for pipe I/O. Full queues disconnect slow clients.
        pub fn broadcast<P: serde::Serialize>(&self, method: &str, params: P) -> Result<(), RpcError> {
            let message = serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0", "method":method, "params":params}))
                .map_err(|_| RpcError::new(proto::INTERNAL_ERROR, "Notification serialization failed"))?;
            if message.len() > framing::MAX_CONTENT_LENGTH {
                return Err(RpcError::invalid_params("Notification too large"));
            }
            let message: Arc<[u8]> = message.into();
            self.shared.clients.lock().retain(|_, client| {
                if client.sender.try_send(message.clone()).is_err() {
                    client.pipe.stop();
                    false
                } else {
                    true
                }
            });
            Ok(())
        }
    }
    pub struct Server {
        shared: Arc<Shared>,
        thread: Mutex<Option<JoinHandle<()>>>,
    }
    impl Server {
        pub fn start(pipe_name: &str, handler: Arc<dyn Handler>) -> io::Result<Self> {
            Self::start_with_options(pipe_name, handler, ServerOptions::default())
        }
        pub fn start_with_options(
            pipe_name: &str,
            handler: Arc<dyn Handler>,
            options: ServerOptions,
        ) -> io::Result<Self> {
            if !pipe_name.starts_with(r"\\.\pipe\") || pipe_name.contains('\0') {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "Expected a local \\.\\pipe\\ name"));
            }
            let first = transport::create(pipe_name, &options.sddl, true)?;
            let shared = Arc::new(Shared {
                stopped: AtomicBool::new(false),
                clients: Mutex::new(HashMap::new()),
                listener: Mutex::new(Some(first.clone())),
            });
            let state = shared.clone();
            let name = pipe_name.to_owned();
            let thread = thread::Builder::new().name("ipc-listener".into()).spawn(move || {
                let mut pipe = first;
                let mut workers: Vec<JoinHandle<()>> = vec![];
                let mut id = 0;
                while !state.stopped.load(Ordering::Acquire) {
                    if pipe.accept().is_err() {
                        break;
                    }
                    if state.stopped.load(Ordering::Acquire) {
                        pipe.stop();
                        break;
                    }
                    let next = match transport::create(&name, &options.sddl, false) {
                        Ok(next) => next,
                        Err(error) => {
                            tracing::error!(%error, "Cannot create next pipe instance");
                            pipe.stop();
                            break;
                        }
                    };
                    *state.listener.lock() = Some(next.clone());
                    // Stop might race the replacement of the listening instance.
                    if state.stopped.load(Ordering::Acquire) {
                        next.stop();
                        pipe.stop();
                        break;
                    }
                    id += 1;
                    let connection_id = id;
                    let connection_pipe = pipe;
                    let connection_state = state.clone();
                    let handler = handler.clone();
                    let require_admin = options.require_admin;
                    match thread::Builder::new()
                        .name(format!("ipc-client-{id}"))
                        .spawn(move || serve(connection_id, connection_pipe, connection_state, handler, require_admin))
                    {
                        Ok(worker) => workers.push(worker),
                        Err(error) => tracing::error!(%error, "Cannot spawn connection worker"),
                    }
                    let mut index = 0;
                    while index < workers.len() {
                        if workers[index].is_finished() {
                            let _ = workers.swap_remove(index).join();
                        } else {
                            index += 1;
                        }
                    }
                    pipe = next;
                }
                state.stopped.store(true, Ordering::Release);
                for client in state.clients.lock().values() {
                    client.pipe.stop();
                }
                for worker in workers {
                    let _ = worker.join();
                }
            })?;
            Ok(Self { shared, thread: Mutex::new(Some(thread)) })
        }
        pub fn notifier(&self) -> Notifier {
            Notifier { shared: self.shared.clone() }
        }
        /// Cancels pipe I/O and joins workers. Handlers must finish their synchronous call.
        pub fn shutdown(&self) {
            self.shared.stopped.store(true, Ordering::Release);
            if let Some(pipe) = self.shared.listener.lock().take() {
                pipe.stop();
            }
            for client in self.shared.clients.lock().values() {
                client.pipe.stop();
            }
            if let Some(thread) = self.thread.lock().take() {
                let _ = thread.join();
            }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    fn reply(request: Value, handler: &dyn Handler, client: &ClientInfo) -> Option<Value> {
        let object = request.as_object();
        let id = request.get("id").cloned();
        let valid_id = id.as_ref().is_none_or(|id| id.is_null() || id.is_string() || id.is_number());
        if object.is_none()
            || request.get("jsonrpc") != Some(&Value::String("2.0".into()))
            || !request.get("method").is_some_and(Value::is_string)
            || !valid_id
        {
            return Some(
                serde_json::json!({"jsonrpc":"2.0", "id":null, "error":RpcError::new(proto::INVALID_REQUEST, "Invalid request")}),
            );
        }
        let method = request["method"].as_str().unwrap();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let result = if !params.is_null() && !params.is_object() && !params.is_array() {
            Err(RpcError::invalid_params("Params must be an object"))
        } else {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler.call(method, params, client)))
                .unwrap_or_else(|_| Err(RpcError::new(proto::INTERNAL_ERROR, "Handler failed")))
        };
        id.map(|id| match result {
            Ok(result) => serde_json::json!({"jsonrpc":"2.0", "id":id, "result":result}),
            Err(error) => serde_json::json!({"jsonrpc":"2.0", "id":id, "error":error}),
        })
    }
    fn serve(id: u64, pipe: Arc<Pipe>, shared: Arc<Shared>, handler: Arc<dyn Handler>, require_admin: bool) {
        // Register before reading so shutdown also cancels unauthenticated/idle connections.
        let (sender, receiver) = bounded::<Arc<[u8]>>(64);
        shared.clients.lock().insert(id, Connection { pipe: pipe.clone(), sender: sender.clone() });
        if shared.stopped.load(Ordering::Acquire) {
            pipe.stop();
        }
        let mut reader = PipeStream(pipe.clone());
        let first = framing::read_frame(&mut reader);
        let client = match first.as_ref().ok().and_then(|_| transport::client_info(&pipe, require_admin).ok()) {
            Some(client) => client,
            None => {
                pipe.stop();
                shared.clients.lock().remove(&id);
                return;
            }
        };
        let writer_pipe = pipe.clone();
        let writer = thread::spawn(move || {
            let mut stream = PipeStream(writer_pipe.clone());
            while !writer_pipe.stopped.load(Ordering::Acquire) {
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(body) => {
                        if framing::write_frame(&mut stream, &body).is_err() {
                            break;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                }
            }
            writer_pipe.stop();
        });
        let mut frame = first;
        while let Ok(body) = frame {
            let response = match serde_json::from_slice(&body) {
                Ok(request) => reply(request, handler.as_ref(), &client),
                Err(_) => Some(
                    serde_json::json!({"jsonrpc":"2.0", "id":null, "error":RpcError::new(proto::PARSE_ERROR, "Invalid JSON")}),
                ),
            };
            if let Some(response) = response {
                let Ok(body) = serde_json::to_vec(&response) else {
                    break;
                };
                if body.len() > framing::MAX_CONTENT_LENGTH || sender.try_send(body.into()).is_err() {
                    break;
                }
            }
            frame = framing::read_frame(&mut reader);
        }
        pipe.stop();
        shared.clients.lock().remove(&id);
        drop(sender);
        let _ = writer.join();
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        struct Echo;
        impl Handler for Echo {
            fn call(&self, _: &str, params: Value, _: &ClientInfo) -> Result<Value, RpcError> {
                Ok(params)
            }
        }
        #[test]
        fn json_rpc_envelopes() {
            let client = ClientInfo { process_id: 0, is_admin: false };
            assert!(reply(serde_json::json!({"jsonrpc":"2.0","method":"x"}), &Echo, &client).is_none());
            assert_eq!(reply(serde_json::json!([]), &Echo, &client).unwrap()["error"]["code"], proto::INVALID_REQUEST);
            assert_eq!(
                reply(serde_json::json!({"jsonrpc":"2.0","id":7,"method":"x","params":1}), &Echo, &client).unwrap()
                    ["error"]["code"],
                proto::INVALID_PARAMS
            );
        }
    }
}
#[cfg(windows)]
pub use platform::{Notifier, Server};
