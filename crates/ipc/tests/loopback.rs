#![cfg(windows)]
use ipc::{
    client::Client,
    proto::{self, RpcError},
    server::{ClientInfo, Handler, Server, ServerOptions},
};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
struct Echo;
impl Handler for Echo {
    fn call(&self, method: &str, params: Value, client: &ClientInfo) -> Result<Value, RpcError> {
        if method == "echo" {
            Ok(params)
        } else if method == "admin" {
            Ok(json!(client.is_admin))
        } else {
            Err(RpcError::method_not_found())
        }
    }
}
#[test]
fn unelevated_loopback_and_shutdown() {
    let name = format!(r"\\.\pipe\inocrium-ipc-test-{}", uuid::Uuid::new_v4());
    // OW denotes the descriptor owner (the creating user); no broad Everyone ACL.
    let server = Server::start_with_options(
        &name,
        Arc::new(Echo),
        ServerOptions { require_admin: false, sddl: "D:P(A;;GA;;;OW)(A;;GA;;;SY)".into() },
    )
    .unwrap();
    let client = Client::connect(&name, Duration::from_secs(5)).unwrap();
    let result: Value = client.call("echo", json!({"unicode":"Türkçe"})).unwrap();
    assert_eq!(result["unicode"], "Türkçe");
    let error = client.call::<_, Value>("missing", json!({})).unwrap_err();
    assert!(matches!(error, ipc::client::Error::Rpc(RpcError { code: proto::METHOD_NOT_FOUND, .. })));
    let notifications = client.notifications();
    server.notifier().broadcast(proto::notifications::MOUNTS_CHANGED, json!({})).unwrap();
    assert_eq!(notifications.recv_timeout(Duration::from_secs(3)).unwrap().method, "mounts.changed");
    let idle = Client::connect(&name, Duration::from_secs(3)).unwrap();
    server.shutdown();
    drop(idle);
    drop(client);
}

#[test]
fn admin_gate_rejects_unelevated_client_before_dispatch() {
    let name = format!(r"\\.\pipe\inocrium-ipc-security-test-{}", uuid::Uuid::new_v4());
    let options = ServerOptions { require_admin: false, sddl: "D:P(A;;GA;;;OW)(A;;GA;;;SY)".into() };
    let server = Server::start_with_options(&name, Arc::new(Echo), options.clone()).unwrap();
    let client = Client::connect(&name, Duration::from_secs(3)).unwrap();
    let is_admin: bool = client.call("admin", json!({})).unwrap();
    server.shutdown();
    drop(client);
    if is_admin { return; } // Elevated test runners are allowed by the admin gate.
    let server = Server::start_with_options(&name, Arc::new(Echo), ServerOptions { require_admin: true, ..options }).unwrap();
    let client = Client::connect(&name, Duration::from_secs(3)).unwrap();
    assert!(matches!(client.call::<_, Value>("echo", json!({})), Err(ipc::client::Error::Disconnected)));
    server.shutdown();
    drop(client);
    // Default ACL itself also prevents opening the pipe with a filtered user token.
    let server = Server::start(&name, Arc::new(Echo)).unwrap();
    assert!(matches!(Client::connect(&name, Duration::from_secs(3)), Err(ipc::client::Error::AccessDenied)));
    server.shutdown();
}
