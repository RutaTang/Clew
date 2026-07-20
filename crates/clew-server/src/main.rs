//! The `clew-server` binary: a thin stdio entry point around the backend.
//!
//! The client spawns this — locally as a child process, or on a remote host
//! over SSH — and drives it entirely through clew-protocol on stdin/stdout.

#[tokio::main]
async fn main() {
    // `--version` prints the protocol version, so the client can check a deployed
    // binary is compatible before running it (part of the SSH bootstrap).
    if std::env::args().any(|a| a == "--version") {
        println!("clew-server protocol {}", clew_protocol::PROTOCOL_VERSION);
        return;
    }
    clew_server::serve_stdio().await;
}
