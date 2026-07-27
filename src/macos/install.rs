//! Applying a downloaded update: verify the new bundle, then swap it in and
//! relaunch.
//!
//! The trust anchor is the running app itself. The downloaded `Clew.app` must
//! carry a valid Developer ID signature (`codesign --verify`, an offline check
//! of the Apple-anchored certificate chain) AND the SAME Team Identifier as the
//! app currently running, which the user already trusted enough to install. No
//! key or team id is hard-coded: an update is accepted only when it was signed by
//! whoever signed the copy you are already running.
//!
//! The swap itself can't happen in-process (you can't replace a running bundle
//! from inside it cleanly), so a tiny detached shell helper waits for this
//! process to exit, swaps the bundle, and relaunches.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The installed `Clew.app` bundle we are running from, or `None` when clew is
/// not running from a `.app` (e.g. a `cargo run` dev binary). In that case there
/// is nothing to swap and the caller falls back to a manual download.
pub fn installed_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // .../Clew.app/Contents/MacOS/clew  ->  .../Clew.app
    let bundle = exe.parent()?.parent()?.parent()?;
    bundle
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("app"))
        .then(|| bundle.to_path_buf())
}

/// Verify a downloaded DMG's `Clew.app`, stage it, then hand off to a detached
/// helper that swaps it in and relaunches once this process exits. Returns as
/// soon as the helper is launched; the caller then quits the app so the helper
/// can proceed. `reopen` is the project to reopen after relaunch, if any.
/// Blocking; run off the UI thread.
pub fn install_dmg(dmg: &Path, reopen: Option<PathBuf>) -> Result<(), String> {
    let target = installed_bundle()
        .ok_or("clew is not running from an installed app, so it can't self-update")?;
    let expected_team =
        team_id(&target).map_err(|e| format!("cannot read this app's signing team: {e}"))?;

    // Mount read-only, no Finder window, no auto-open.
    let mount_out = run(Command::new("/usr/bin/hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noverify",
            "-noautoopen",
        ])
        .arg(dmg))
    .map_err(|e| format!("could not open the update image: {e}"))?;
    let mount_point = parse_mount_point(&mount_out).ok_or("could not find the update volume")?;

    // Everything between mount and detach goes through this closure so we always
    // unmount, even on an early error.
    let staged = (|| -> Result<(PathBuf, PathBuf), String> {
        let src_app = mount_point.join("Clew.app");
        if !src_app.exists() {
            return Err("the update image has no Clew.app".into());
        }
        verify(&src_app, &expected_team)?;
        // Copy the verified app out so the DMG can be detached before the swap.
        let staging = std::env::temp_dir().join(format!("clew-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
        let staged_app = staging.join("Clew.app");
        run(Command::new("/usr/bin/ditto")
            .arg(&src_app)
            .arg(&staged_app))
        .map_err(|e| format!("could not stage the update: {e}"))?;
        Ok((staging, staged_app))
    })();

    let _ = run(Command::new("/usr/bin/hdiutil")
        .args(["detach", "-quiet"])
        .arg(&mount_point));
    let (staging, staged_app) = staged?;

    spawn_swap_helper(std::process::id(), &staged_app, &staging, &target, reopen)
}

/// Run a command, returning stdout on success or a trimmed stderr (falling back
/// to stdout) on failure.
fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let msg = if err.trim().is_empty() {
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        err.into_owned()
    };
    Err(msg.trim().to_string())
}

/// The Team Identifier a bundle was signed with, read from `codesign` (which
/// prints its details to stderr).
fn team_id(bundle: &Path) -> Result<String, String> {
    let out = Command::new("/usr/bin/codesign")
        .args(["-d", "--verbose=4"])
        .arg(bundle)
        .output()
        .map_err(|e| e.to_string())?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    text.lines()
        .find_map(|l| l.trim().strip_prefix("TeamIdentifier="))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "not set")
        .ok_or_else(|| "the bundle has no Team Identifier".to_string())
}

/// Verify `app` is an intact clew build from the same signer as us.
fn verify(app: &Path, expected_team: &str) -> Result<(), String> {
    // Structural integrity plus a valid, Apple-anchored Developer ID signature,
    // checked entirely offline.
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app))
    .map_err(|e| format!("the update's signature is invalid: {e}"))?;
    let team = team_id(app)?;
    if team != expected_team {
        return Err(format!(
            "the update is signed by a different team ({team}), refusing to install it"
        ));
    }
    Ok(())
}

/// Parse the `/Volumes/…` mount point out of `hdiutil attach`'s text output. The
/// mount point is the last tab-separated column on the volume's line; scan from
/// the bottom so the real volume line wins over device-only rows.
fn parse_mount_point(attach_output: &str) -> Option<PathBuf> {
    attach_output.lines().rev().find_map(|line| {
        line.split('\t')
            .map(str::trim)
            .find(|f| f.starts_with("/Volumes/"))
            .map(PathBuf::from)
    })
}

/// Write and launch the detached helper that performs the swap once clew exits.
fn spawn_swap_helper(
    pid: u32,
    staged_app: &Path,
    staging: &Path,
    target: &Path,
    reopen: Option<PathBuf>,
) -> Result<(), String> {
    let helper = staging.join("swap.sh");
    let reopen_args = match reopen {
        Some(p) => format!("--args {}", quote(&p.to_string_lossy())),
        None => String::new(),
    };
    // Wait for our pid to die, back up the old bundle, ditto the new one in
    // (rolling back on failure), relaunch, then clean up. Every path is
    // single-quoted, so spaces are safe.
    let script = format!(
        "#!/bin/bash\n\
         set -u\n\
         for _ in $(seq 1 200); do kill -0 {pid} 2>/dev/null || break; sleep 0.1; done\n\
         TARGET={target}\n\
         STAGED={staged}\n\
         BACKUP=\"$TARGET.bak-$$\"\n\
         rm -rf \"$BACKUP\"\n\
         if [ -d \"$TARGET\" ]; then mv \"$TARGET\" \"$BACKUP\" || exit 1; fi\n\
         if ditto \"$STAGED\" \"$TARGET\"; then\n\
         \trm -rf \"$BACKUP\"\n\
         else\n\
         \trm -rf \"$TARGET\"; [ -d \"$BACKUP\" ] && mv \"$BACKUP\" \"$TARGET\"; exit 1\n\
         fi\n\
         open \"$TARGET\" {reopen_args}\n\
         rm -rf {staging}\n",
        target = quote(&target.to_string_lossy()),
        staged = quote(&staged_app.to_string_lossy()),
        staging = quote(&staging.to_string_lossy()),
    );
    std::fs::write(&helper, script).map_err(|e| e.to_string())?;
    // `std::process::Command` does not kill the child on drop, so the helper
    // outlives clew's exit and performs the swap.
    Command::new("/bin/bash")
        .arg(&helper)
        .spawn()
        .map_err(|e| format!("could not start the updater helper: {e}"))?;
    Ok(())
}

/// Single-quote a string for safe interpolation into the shell helper.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{parse_mount_point, quote};

    #[test]
    fn finds_mount_point_in_hdiutil_output() {
        let out = "/dev/disk4          \tGUID_partition_scheme\t\n\
                   /dev/disk4s1        \tApple_HFS            \t/Volumes/Clew\n";
        assert_eq!(
            parse_mount_point(out).unwrap().to_str().unwrap(),
            "/Volumes/Clew"
        );
    }

    #[test]
    fn mount_point_none_when_absent() {
        assert!(parse_mount_point("/dev/disk4\tGUID_partition_scheme\t\n").is_none());
    }

    #[test]
    fn quote_escapes_single_quotes_and_spaces() {
        assert_eq!(quote("/Applications/Clew.app"), "'/Applications/Clew.app'");
        assert_eq!(quote("/a b/c"), "'/a b/c'");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }
}
