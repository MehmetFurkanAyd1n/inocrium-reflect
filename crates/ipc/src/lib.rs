//! Blocking JSON-RPC 2.0 over local Windows named pipes.
pub mod client;
pub mod framing;
pub mod proto;
pub mod server;
#[cfg(windows)]
mod transport;
pub use proto::RpcError;
