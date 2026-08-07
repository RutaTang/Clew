//! The `clew-server` binary: a thin stdio entry point around the backend.
//!
//! The client spawns this — locally as a child process, or on a remote host
//! over SSH — and drives it entirely through clew-protocol on stdin/stdout.

#[tokio::main]
async fn main() {
    // `--version` prints the protocol version and build fingerprint, so the
    // client can check a deployed binary is compatible before running it
    // (part of the SSH bootstrap). The fingerprint catches a wire change
    // that shares the numeric version (e.g. a dev build over a cached one).
    if std::env::args().any(|a| a == "--version") {
        println!(
            "clew-server protocol {} fingerprint {}",
            clew_protocol::PROTOCOL_VERSION,
            clew_protocol::SCHEMA_FINGERPRINT
        );
        return;
    }
    clew_server::serve_stdio().await;
}
