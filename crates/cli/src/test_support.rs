use ipc::{
    client::Client,
    server::{Handler, Server, ServerOptions},
};
use std::{sync::Arc, time::Duration};

pub fn loopback(handler: Arc<dyn Handler>) -> (Server, Client) {
    let name = format!(r"\\.\pipe\inocrium-cli-test-{}", uuid::Uuid::new_v4());
    let server = Server::start_with_options(
        &name,
        handler,
        ServerOptions { require_admin: false, sddl: "D:P(A;;GA;;;OW)(A;;GA;;;SY)".into() },
    )
    .unwrap();
    let client = Client::connect(&name, Duration::from_secs(5)).unwrap();
    (server, client)
}
