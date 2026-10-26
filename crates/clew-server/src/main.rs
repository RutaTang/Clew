//! The `clew-server` binary: a thin stdio entry point around the backend.
//!
//! The client spawns this — locally as a child process, or on a remote host
//! over SSH — and drives it entirely through clew-protocol on stdin/stdout.
//!
//! `clew-server mcp [--root <dir>]` instead serves the project's code tools
//! to an MCP client on stdin/stdout (see `clew_server::mcp`), and
//! `clew-server acp` serves clew's Ask agent to an ACP client — an editor
//! (see `clew_server::acp`).

/// How long the runtime may take to wind down after the client's stream
/// ended. `serve_stdio` has already stopped everything the connection started
/// and flushed what it could; what can remain is a blocking thread parked in a
/// syscall that nothing can interrupt — a provider that never answers, a read
/// from a hung mount. A plain runtime drop waits for those without any limit,
/// which kept a disconnected server (and its child processes) alive; past this
/// they are abandoned and the process exits.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

fn main() {
    // `--version` prints the protocol version and build fingerprint, so the
    // client can check a deployed binary is compatible before running it
    // (part of the SSH bootstrap, which compares this exact line). The
    // fingerprint catches a wire change that shares the numeric version
    // (e.g. a dev build over a cached one).
    if std::env::args().any(|a| a == "--version") {
        println!("{}", clew_protocol::version_line());
        return;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("[clew-server] cannot start the async runtime: {e}");
            std::process::exit(1);
        }
    };
    // A panic on this thread — the request loop — aborts rather than unwinding
    // into a runtime drop that would wait, unbounded, on work nobody stopped;
    // work on the runtime's other threads still unwinds and is answered with
    // an error (see the function's docs for why not `panic = "abort"`).
    clew_server::abort_on_panic_in_this_thread();
    // The `PATH` language servers, debug adapters and MCP servers are given
    // (asked of the login shell once, for an app opened from the Finder,
    // whose server inherits its bare `PATH`) is found now, off the request
    // loop: the first `SpawnProcess` reads it there, and waited for the
    // shell — seconds of every request queued behind it.
    std::thread::spawn(|| {
        let _ = clew_core::linerpc::spawn_path();
    });
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("mcp") {
        let root = match clew_server::mcp::root_from_args(&args) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("[clew-server] mcp: {e}\nusage: clew-server mcp [--root <dir>]");
                std::process::exit(2);
            }
        };
        runtime.block_on(clew_server::mcp::serve_mcp(&root));
        runtime.shutdown_timeout(SHUTDOWN_GRACE);
        return;
    }
    if args.first().map(String::as_str) == Some("acp") {
        runtime.block_on(clew_server::acp::serve_acp());
        runtime.shutdown_timeout(SHUTDOWN_GRACE);
        return;
    }
    runtime.block_on(clew_server::serve_stdio());
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
}
