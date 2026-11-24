//! The commands every child process of Clew is started from.
//!
//! On Windows, Clew's window is a GUI program (in a release build), with no
//! console of its own. A console program it starts — git, a language server,
//! a debug adapter, its own `clew-server` — would be handed a console window
//! of its own by the system, flashing up on the screen for every `git` call.
//! Each is started without one (`CREATE_NO_WINDOW`): it still has a console,
//! which whatever it starts in turn shares, but no window ever shows it. On
//! every other system these are the plain commands.

use std::ffi::OsStr;

/// `CREATE_NO_WINDOW`: a console program runs with a console of its own that
/// is never shown.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A [`std::process::Command`] for `program`, which starts no window.
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, CREATE_NO_WINDOW);
    command
}

/// A [`tokio::process::Command`] for `program`, which starts no window.
pub fn tokio_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut command = tokio::process::Command::new(program);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// How a program run to its end ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    /// It ran and exited with this code.
    Exited(i64),
    /// It was stopped, and everything it started with it.
    Stopped,
}

/// Run `command` to its end, each line it writes handed to `said` as it
/// comes, with whether it was written to stderr; its input is closed and
/// its output piped here. Blocking; stopped, with everything it started,
/// once `cancel` is set.
pub fn run_streaming(
    mut command: std::process::Command,
    mut said: impl FnMut(&str, bool),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<RunEnd, String> {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A group of its own: what it starts runs under it, and stopping it stops all of it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    let _job = crate::procgroup::adopt_scoped(&child);
    let (lines_tx, lines) = std::sync::mpsc::channel::<(bool, String)>();
    let readers: Vec<std::thread::JoinHandle<()>> = [
        child
            .stdout
            .take()
            .map(|s| (false, Box::new(s) as Box<dyn std::io::Read + Send>)),
        child
            .stderr
            .take()
            .map(|s| (true, Box::new(s) as Box<dyn std::io::Read + Send>)),
    ]
    .into_iter()
    .flatten()
    .map(|(stderr, stream)| {
        let tx = lines_tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send((stderr, line)).is_err() {
                    break;
                }
            }
        })
    })
    .collect();
    drop(lines_tx);
    let status = loop {
        while let Ok((stderr, line)) = lines.try_recv() {
            said(&line, stderr);
        }
        if cancel.load(Ordering::SeqCst) {
            crate::procgroup::kill(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return Ok(RunEnd::Stopped);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if let Ok((stderr, line)) = lines.recv_timeout(Duration::from_millis(100)) {
                    said(&line, stderr);
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    for reader in readers {
        let _ = reader.join();
    }
    while let Ok((stderr, line)) = lines.try_recv() {
        said(&line, stderr);
    }
    Ok(RunEnd::Exited(status.code().map_or(-1, i64::from)))
}

#[cfg(test)]
mod tests {
    /// The commands run what they name, with what they are given — on
    /// Windows without a window, which changes nothing else.
    #[test]
    fn a_command_runs_its_program() {
        #[cfg(windows)]
        let (program, args) = ("cmd", ["/C", "echo here"]);
        #[cfg(not(windows))]
        let (program, args) = ("/bin/sh", ["-c", "echo here"]);
        let out = super::command(program).args(args).output().unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "here");
    }

    #[tokio::test]
    async fn a_tokio_command_runs_its_program() {
        #[cfg(windows)]
        let (program, args) = ("cmd", ["/C", "echo here"]);
        #[cfg(not(windows))]
        let (program, args) = ("/bin/sh", ["-c", "echo here"]);
        let out = super::tokio_command(program)
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "here");
    }
}
