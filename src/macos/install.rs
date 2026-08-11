//! Applying a downloaded update: verify the new bundle, then swap it in and
//! relaunch.
//!
//! The trust anchor is the running app itself. The gate that establishes
//! PROVENANCE is `verify`: the `Clew.app` inside the downloaded image must
//! satisfy a code requirement pinning `anchor apple generic` plus the SAME Team
//! Identifier as the app currently running, which the user already trusted
//! enough to install. It is offline and never skipped. No key or team id is
//! hard-coded — an update is accepted only when it was signed by whoever signed
//! the copy you are already running, with a certificate chain that really
//! terminates at an Apple root.
//!
//! A second gate assesses Apple notarization. It adds Apple's malware scan of
//! this build and post-hoc revocation, and it is currently REQUIRED — but only
//! because the release workflow guarantees every signed release is notarized.
//! [`REQUIRE_NOTARIZATION`] states that coupling; neither side may move alone.
//!
//! The one gate has to be spelled out, because the obvious spellings check
//! nothing. `codesign --verify` with no `-R` verifies a bundle only for internal
//! integrity and against its OWN designated requirement, which a self-signed
//! forgery satisfies by construction, and the `TeamIdentifier=` line `codesign
//! -d` prints is a field the candidate declares about itself rather than one
//! read from a certificate. Nothing in the download path sets
//! `com.apple.quarantine` either, so the swapped bundle is never assessed by
//! Gatekeeper on relaunch: whatever `-R` does not check, nothing downstream
//! will.
//!
//! Verification proves something about the bundle on the read-only image, but
//! what gets installed is the copy we make of it, so that copy lands somewhere
//! only this user can reach ([`crate::updater::create_private_dir`]) and the script that
//! installs it never touches the filesystem at all ([`spawn_swap_helper`]).
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

/// Whether an in-app install can work at all: clew runs from a `.app`, AND that
/// bundle names a signing team for an update to be anchored to.
///
/// The team half is what keeps an ad-hoc-signed install from downloading a
/// whole disk image and only then failing with "cannot read this app's signing
/// team" — that tier has never been able to self-update, and the user should be
/// sent to the release page before spending the bandwidth. `install_dmg`
/// re-reads the team instead of trusting this answer: it is the value every
/// signature check is made against, and the two calls are a download apart.
pub fn self_install_supported() -> bool {
    installed_bundle().is_some_and(|bundle| team_id(&bundle).is_ok())
}

/// Verify a downloaded DMG's `Clew.app`, stage it, then hand off to a detached
/// helper that swaps it in and relaunches once this process exits. Returns as
/// soon as the helper is launched; the caller then quits the app so the helper
/// can proceed. `reopen` is the project to reopen after relaunch, if any.
/// Blocking; run off the UI thread.
pub fn install_dmg(dmg: &Path, reopen: Option<PathBuf>) -> Result<(), String> {
    let target = installed_bundle()
        .ok_or("clew is not running from an installed app, so it can't self-update")?;
    // Fails outright on an ad-hoc-signed install, which reports `TeamIdentifier=
    // not set`: there is no team to anchor an update to, so there is nothing
    // this path could check. That is the tier a release with no signing secrets
    // produces, and it has never been able to self-update. `App::can_self_install`
    // asks `self_install_supported()` before the download, so that tier is
    // routed to the release page rather than being told this after waiting for
    // a disk image. Re-read here regardless: it is the value every signature
    // check below is made against, so it is not taken on the earlier answer's
    // word.
    let expected_team =
        team_id(&target).map_err(|e| format!("cannot read this app's signing team: {e}"))?;
    // Assessed, then judged by policy. Under the shipped policy this REFUSES an
    // un-notarized image; the admitting branch survives so that flipping
    // `REQUIRE_NOTARIZATION` back cannot make such an install silent.
    if let Some(reason) = notarization_gate(assess_notarized(dmg), REQUIRE_NOTARIZATION)? {
        eprintln!(
            "[clew] {reason} — installing it anyway because its signature is anchored to this app's own team"
        );
    }

    // Created before the image is touched, so every later failure has one
    // directory to clean up and nothing untrusted is written until `verify`
    // has passed.
    let staging = crate::updater::create_private_dir(&staging_parent()?, STAGING_PREFIX)?;
    let launched = stage_from_image(dmg, &expected_team, &staging).and_then(|staged_app| {
        spawn_swap_helper(std::process::id(), &staged_app, &staging, &target, reopen)
    });
    if launched.is_err() {
        // Only the helper prunes the staging directory, so if it never starts
        // nothing else will: a staged copy is a whole Clew.app, and this
        // directory — unlike the temp dir it used to live in — is not cleared
        // at reboot. A hard crash between here and the swap still leaks one.
        let _ = std::fs::remove_dir_all(&staging);
    }
    launched
}

/// Mount the image read-only, verify the `Clew.app` on it, and copy that app
/// into `staging` so the image can be detached before the swap. Returns the
/// staged copy.
fn stage_from_image(dmg: &Path, expected_team: &str, staging: &Path) -> Result<PathBuf, String> {
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
    let staged = (|| -> Result<PathBuf, String> {
        let src_app = mount_point.join("Clew.app");
        if !src_app.exists() {
            return Err("the update image has no Clew.app".into());
        }
        verify(&src_app, expected_team)?;
        let staged_app = staging.join("Clew.app");
        run(Command::new("/usr/bin/ditto")
            .arg(&src_app)
            .arg(&staged_app))
        .map_err(|e| format!("could not stage the update: {e}"))?;
        Ok(staged_app)
    })();

    let _ = run(Command::new("/usr/bin/hdiutil")
        .args(["detach", "-quiet"])
        .arg(&mount_point));
    staged
}

/// Where an update is staged between verification and the swap: clew's own
/// data directory, never the shared temp dir.
///
/// `std::env::temp_dir()` is `$TMPDIR`, or with it unset the world-traversable
/// `/tmp`. The old name there was `clew-update-<pid>` — five digits, so an
/// attacker can pre-create the whole range, own the directory clew then writes
/// the verified bundle into, and replace it during the twenty seconds the
/// helper spends waiting for clew to exit. That would install a bundle the
/// signature check never saw.
///
/// There is deliberately no fallback to the temp dir when there is no data
/// directory: an install with nowhere private to stage fails instead of
/// staging somewhere shared. Same policy the download path states in
/// `src/backend/updater.rs`.
fn staging_parent() -> Result<PathBuf, String> {
    Ok(clew_core::lsp::store::data_root()
        .ok_or("no data directory to stage the update in")?
        .join("updates"))
}

/// Names the private per-install directory the verified bundle is staged in.
/// The directory itself comes from [`crate::updater::create_private_dir`],
/// which the download half of an update uses too — one exclusive-create,
/// 0700, per-attempt helper rather than two that can drift apart.
const STAGING_PREFIX: &str = "stage";

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

/// Whether an update must carry a stapled Apple notarization ticket, as
/// opposed to merely being assessed for one.
///
/// True, and true only because the release workflow now guarantees it.
/// `.github/workflows/release.yml` gates "Notarize & staple" on the signing
/// identity alone and fails the build when the notarization secrets are
/// missing, so a signed release is a notarized release; it then asserts the
/// very predicate [`assess_notarized`] evaluates, so a DMG that would fail here
/// fails CI first.
///
/// That coupling is the whole justification. While the workflow published
/// signed-but-un-notarized images, requiring a ticket turned those releases
/// into a permanent "Update failed" with a Retry that could never succeed,
/// while the identical file installed by hand through Gatekeeper's "Open
/// Anyway" — a gate the user walks around by downloading the same bytes buys
/// nothing. If the workflow is ever loosened again, this has to go back to
/// `false` in the same change.
///
/// What it adds over [`verify`] alone: Apple's malware scan of this exact
/// build, and Apple's ability to revoke a ticket after the fact. Provenance is
/// NOT what it buys — that comes from `verify`, which is never skipped and
/// reads the team out of the leaf certificate of a chain terminating at an
/// Apple root, so forging an update requires a Developer ID certificate issued
/// to this project's own team either way.
const REQUIRE_NOTARIZATION: bool = true;

/// Ask Gatekeeper to assess the downloaded image exactly as it would if the
/// user had double-clicked it: a valid signature over the whole image plus a
/// stapled Apple notarization ticket. The release workflow staples the ticket
/// onto the DMG, so this needs no network.
///
/// Advisory, per [`REQUIRE_NOTARIZATION`]. It is also not a sound place to
/// decide: a refusal does not distinguish "no ticket" from "broken signature",
/// and the user can install the very image it refuses through Gatekeeper's
/// override. `verify` is where the decision is made.
fn assess_notarized(dmg: &Path) -> Result<(), String> {
    run(Command::new("/usr/sbin/spctl")
        .args([
            "--assess",
            "--type",
            "open",
            "--context",
            "context:primary-signature",
        ])
        .arg(dmg))
    .map(|_| ())
}

/// Apply [`REQUIRE_NOTARIZATION`] to Gatekeeper's verdict on the image.
///
/// `Ok(None)` — assessed clean. `Ok(Some(reason))` — no ticket, admitted by
/// policy, and the reason is returned rather than dropped so an un-notarized
/// release is logged instead of passing silently. `Err` — no ticket where
/// policy demands one.
fn notarization_gate(
    assessed: Result<(), String>,
    required: bool,
) -> Result<Option<String>, String> {
    match assessed {
        Ok(()) => Ok(None),
        Err(e) => {
            let reason = format!("the update is not notarized by Apple: {e}");
            if required {
                Err(reason)
            } else {
                Ok(Some(reason))
            }
        }
    }
}

/// The `-R` argument an update's signature must satisfy: a certificate chain
/// that really terminates at an Apple root, with the team taken from the leaf
/// certificate instead of the self-declared CodeDirectory field. The leading
/// `=` is what makes codesign read this as requirement source text; without it
/// the string is taken as a path to a compiled requirement and the check fails
/// with "No such file or directory".
///
/// A team id that is not plain alphanumerics is refused rather than escaped: it
/// would be interpolated into a requirement expression, and a quote in it would
/// silently reshape the expression we are trusting.
fn requirement_arg(team: &str) -> Result<String, String> {
    if team.is_empty() || !team.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("`{team}` is not a usable Team Identifier"));
    }
    Ok(format!(
        "=anchor apple generic and certificate leaf[subject.OU] = \"{team}\""
    ))
}

/// Verify `app` is an intact clew build from the same signer as us.
fn verify(app: &Path, expected_team: &str) -> Result<(), String> {
    // Structural integrity plus the pinned requirement. `-R` is the whole point:
    // without it codesign only checks the bundle against its own designated
    // requirement, so an attacker who signs their own Clew.app passes.
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "-R"])
        .arg(requirement_arg(expected_team)?)
        .arg(app))
    .map_err(|e| format!("the update's signature is invalid: {e}"))?;
    // Belt and braces: the team the bundle declares must agree with the leaf
    // certificate the requirement just matched. A disagreement means a
    // hand-assembled signature, not something Apple's toolchain produced.
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

/// Build and launch the detached helper that performs the swap once clew exits.
///
/// The script is handed to bash through argv, not written into the staging
/// directory and run by path. A file would reopen the check-to-use gap that
/// staging privately closes: bash runs whatever is at that path when it opens
/// it, which is seconds later, after clew has exited. `-c` delivers the exact
/// bytes built here, atomically at `exec`, so there is nothing to substitute.
/// (Feeding it on stdin would keep the text out of `ps`, but a parent that
/// dies mid-write leaves bash executing a truncated script, and a truncation
/// just past `mv "$TARGET" "$BACKUP"` destroys the installed app. The text is
/// only paths.)
fn spawn_swap_helper(
    pid: u32,
    staged_app: &Path,
    staging: &Path,
    target: &Path,
    reopen: Option<PathBuf>,
) -> Result<(), String> {
    let script = swap_script(pid, staged_app, staging, target, reopen);
    // `std::process::Command` does not kill the child on drop, so the helper
    // outlives clew's exit and performs the swap.
    Command::new("/bin/bash")
        .arg("-c")
        .arg(&script)
        .spawn()
        .map_err(|e| format!("could not start the updater helper: {e}"))?;
    Ok(())
}

/// The helper's body. Wait for our pid to die, back up the old bundle, ditto
/// the new one in (rolling back on failure), relaunch, then clean up. Every
/// path is single-quoted, so spaces are safe. No shebang: this is an argument
/// to `bash -c`, never a file.
fn swap_script(
    pid: u32,
    staged_app: &Path,
    staging: &Path,
    target: &Path,
    reopen: Option<PathBuf>,
) -> String {
    let reopen_args = match reopen {
        Some(p) => format!("--args {}", quote(&p.to_string_lossy())),
        None => String::new(),
    };
    format!(
        "set -u\n\
         for _ in $(seq 1 200); do kill -0 {pid} 2>/dev/null || break; sleep 0.1; done\n\
         TARGET={target}\n\
         STAGED={staged}\n\
         BACKUP=\"$TARGET.bak-$$\"\n\
         rm -rf \"$BACKUP\"\n\
         if [ -d \"$TARGET\" ]; then mv \"$TARGET\" \"$BACKUP\" || exit 1; fi\n\
         if ditto \"$STAGED\" \"$TARGET\"; then\n\
         \trm -rf \"$BACKUP\"\n\
         else\n\
         \trm -rf \"$TARGET\"; [ -d \"$BACKUP\" ] && mv \"$BACKUP\" \"$TARGET\"; rm -rf {staging}; exit 1\n\
         fi\n\
         open \"$TARGET\" {reopen_args}\n\
         rm -rf {staging}\n",
        target = quote(&target.to_string_lossy()),
        staged = quote(&staged_app.to_string_lossy()),
        staging = quote(&staging.to_string_lossy()),
    )
}

/// Single-quote a string for safe interpolation into the shell helper.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{
        REQUIRE_NOTARIZATION, STAGING_PREFIX, notarization_gate, parse_mount_point, quote,
        requirement_arg, staging_parent, swap_script, verify,
    };
    use std::path::Path;
    use std::process::Command;

    /// Build a minimal ad-hoc signed `.app` under `dir`. Ad-hoc means no
    /// certificate chain at all, which is what a forged update looks like to
    /// everything except a requirement that names the anchor.
    fn ad_hoc_bundle(dir: &Path) -> std::path::PathBuf {
        let app = dir.join("Fake.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/MacOS/Fake"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <plist version=\"1.0\"><dict>\
             <key>CFBundleExecutable</key><string>Fake</string>\
             <key>CFBundleIdentifier</key><string>com.example.fake</string>\
             <key>CFBundlePackageType</key><string>APPL</string>\
             </dict></plist>\n",
        )
        .unwrap();
        let out = Command::new("/usr/bin/codesign")
            .args(["--force", "-s", "-"])
            .arg(&app)
            .output()
            .unwrap();
        assert!(out.status.success(), "ad-hoc signing failed: {out:?}");
        app
    }

    /// The requirement has to name an Apple anchor and read the team out of the
    /// leaf certificate, and it has to carry the leading `=` — codesign reads a
    /// `-R` argument without it as a path to a compiled requirement file.
    #[test]
    fn requirement_pins_an_apple_anchor_and_reads_the_team_from_the_certificate() {
        let arg = requirement_arg("ABCDE12345").unwrap();
        assert_eq!(
            arg,
            "=anchor apple generic and certificate leaf[subject.OU] = \"ABCDE12345\""
        );
        // csreq is the compiler codesign hands `-R` text to, so this proves the
        // expression parses rather than just that we spelled a string.
        let out = Command::new("/usr/bin/csreq")
            .args(["-r", &arg, "-b", "/dev/null"])
            .output()
            .unwrap();
        assert!(out.status.success(), "requirement did not compile: {out:?}");
    }

    /// The team id is interpolated into a requirement expression, so anything
    /// that could close the quote and bolt on another clause is refused outright.
    #[test]
    fn requirement_refuses_a_team_id_that_could_reshape_it() {
        for bad in ["", "AB\" or anchor apple generic and \"", "AB CDE", "AB-CD"] {
            assert!(
                requirement_arg(bad).is_err(),
                "{bad:?} should not build a requirement"
            );
        }
    }

    /// The gate the finding was about: `codesign --verify --deep --strict` alone
    /// exits 0 on a bundle with no certificate chain whatsoever, because it
    /// checks the candidate against the candidate's own designated requirement.
    /// Only the pinned `-R` requirement rejects it.
    #[test]
    fn ad_hoc_bundle_passes_bare_verify_but_fails_the_pinned_requirement() {
        let dir = std::env::temp_dir().join("clew-update-verify-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let app = ad_hoc_bundle(&dir);

        let bare = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&app)
            .status()
            .unwrap();
        assert!(bare.success(), "the old gate is supposed to accept this");

        let err = verify(&app, "ABCDE12345").unwrap_err();
        assert!(
            err.contains("signature is invalid"),
            "expected the signature check to reject it, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// The shipped stance, pinned: an un-notarized image is refused in-app.
    ///
    /// This pins the RUST half alone. Nothing in `cargo test` reads a workflow
    /// file, so it says nothing about the release side. The drift that hurts is
    /// the other one — a release.yml edit that lets a signed DMG ship without a
    /// ticket while this constant still demands one — and that is caught in the
    /// workflow layer instead, by release.yml's "A signed DMG is a notarized
    /// DMG" step, deliberately outside "Notarize & staple" so narrowing that
    /// step's own `if:` cannot carry the assertion away with it. The opposite
    /// drift (this constant returning to `false` while the workflow keeps
    /// notarizing) only widens what installs, and nothing guards it.
    #[test]
    fn the_shipped_policy_demands_a_notarization_ticket() {
        let err = notarization_gate(Err("rejected".into()), REQUIRE_NOTARIZATION)
            .expect_err("an un-notarized image must not be installed in-app");
        assert!(err.contains("not notarized"), "got: {err}");
        // A clean assessment has nothing to report, under either policy.
        assert_eq!(
            notarization_gate(Ok(()), REQUIRE_NOTARIZATION).unwrap(),
            None
        );
    }

    /// The admitting branch is still reachable and still not silent, so a
    /// future rollback to `false` cannot make an un-notarized install quiet.
    #[test]
    fn admitting_an_unnotarized_image_still_reports_why() {
        let admitted = notarization_gate(Err("rejected".into()), false)
            .expect("policy false admits the image");
        assert!(
            admitted
                .as_deref()
                .is_some_and(|r| r.contains("not notarized")),
            "the caller logs this reason: {admitted:?}"
        );
    }

    /// The assessment is still wired up and still means something: a clean
    /// verdict passes with nothing to report, and the gate does refuse once
    /// the policy demands a ticket.
    #[test]
    fn notarization_gate_reports_a_clean_verdict_and_can_be_made_mandatory() {
        assert_eq!(notarization_gate(Ok(()), false).unwrap(), None);
        assert_eq!(notarization_gate(Ok(()), true).unwrap(), None);
        let err = notarization_gate(Err("rejected".into()), true).unwrap_err();
        assert!(err.contains("not notarized"), "got: {err}");
    }

    /// Staging goes under clew's own data root. The temp dir is `$TMPDIR` or,
    /// unset, the shared `/tmp`, where the verified bundle and the swap could
    /// be replaced by another account between verification and use.
    #[test]
    fn updates_stage_under_the_data_root_never_the_shared_temp_dir() {
        // `data_root()` reads `CLEW_DATA_DIR`, and other tests in this binary
        // point that at a directory under the temp dir while they run. Without
        // the lock this test read whichever value happened to be installed and
        // failed on somebody else's fixture; without clearing the variable it
        // would still be asserting about that fixture rather than about the
        // shipped default.
        let _env = clew_core::env_lock();
        let prev = std::env::var_os("CLEW_DATA_DIR");
        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::remove_var("CLEW_DATA_DIR") };

        let parent = staging_parent().expect("a data root to stage in");
        let root = clew_core::lsp::store::data_root().unwrap();
        let under_temp = parent.starts_with(std::env::temp_dir());

        // SAFETY: env mutation serialized by env_lock.
        if let Some(p) = prev {
            unsafe { std::env::set_var("CLEW_DATA_DIR", p) };
        }
        assert_eq!(parent, root.join("updates"));
        assert!(!under_temp, "{} is under the temp dir", parent.display());
    }

    /// The staging directory is the shared per-attempt helper's, under the
    /// staging parent — the properties themselves are asserted next to that
    /// helper in `crate::updater`, which the download half uses too. Here only
    /// that this half still asks for one, and does not go back to a name a
    /// second window would share.
    #[test]
    fn staging_asks_the_shared_helper_for_a_private_per_install_directory() {
        let parent = std::env::temp_dir().join(format!("clew-stage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);

        let first = crate::updater::create_private_dir(&parent, STAGING_PREFIX).unwrap();
        let second = crate::updater::create_private_dir(&parent, STAGING_PREFIX).unwrap();
        assert!(first.starts_with(&parent) && second.starts_with(&parent));
        assert_ne!(second, first, "two installs must not share a directory");

        let _ = std::fs::remove_dir_all(&parent);
    }

    /// The helper is an argument to `bash -c`, so what it does is fixed at
    /// exec. It must install from the private staging copy and clean that
    /// directory up on both outcomes, including the rollback.
    #[test]
    fn the_swap_script_installs_from_staging_and_prunes_it_either_way() {
        let staging = Path::new("/Users/x/Library/Application Support/clew/updates/stage-9-0");
        let script = swap_script(
            9,
            &staging.join("Clew.app"),
            staging,
            Path::new("/Applications/Clew.app"),
            None,
        );
        assert!(
            !script.contains("#!"),
            "no shebang: this is argv, not a file"
        );
        assert!(script.contains(&format!(
            "STAGED={}",
            quote(&staging.join("Clew.app").to_string_lossy())
        )));
        assert_eq!(
            script
                .matches(&format!("rm -rf {}", quote(&staging.to_string_lossy())))
                .count(),
            2,
            "both the success and the rollback path must prune the staging directory"
        );
    }
}
