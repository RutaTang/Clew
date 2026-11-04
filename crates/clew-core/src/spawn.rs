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
