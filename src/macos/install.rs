//! Applying a downloaded update: verify the new bundle, then swap it in and
//! relaunch.
//!
//! The trust anchor is the running app itself. The gate that establishes
//! PROVENANCE is `verify`: the `Clew.app` inside the downloaded image must
//! satisfy a code requirement pinning `anchor apple generic`, a Developer ID
//! certificate chain (the intermediate's and the leaf's Apple OIDs), the SAME
//! signing identifier and the SAME Team Identifier as the app currently running,
//! which the user already trusted enough to install. It is offline and never
//! skipped. No key or team id is hard-coded — an update is accepted only when it
//! was signed by whoever signed the copy you are already running, as the same
//! app (so a Dev or Test flavor signed by the same team cannot replace the
//! release build, nor the reverse), with a certificate chain that really
//! terminates at an Apple root.
//!
//! Provenance is not freshness, so the verified bundle's own version is checked
//! too (`check_candidate_version`): it must be exactly the release the user was
//! offered and newer than the running one. Without that, any older notarized
//! Clew — every one of which satisfies the requirement — could be served as an
//! "update" and roll the user back to a build with known bugs.
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
//! process to exit, swaps the bundle, and relaunches. Everything that could make
//! that swap fail after clew has already quit — a read-only disk image, App
//! Translocation, an `/Applications` this user cannot write — is checked BEFORE
//! the download ([`self_install_blocker`]), and a swap that fails anyway puts
//! the previous version back, relaunches it, and leaves the reason where the
//! relaunched clew reports it.

use std::path::{Path, PathBuf};
use std::process::Command;

use clew_core::update::Version;

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

/// Why this copy of clew cannot replace itself, or `None` when it can.
///
/// Asked BEFORE the download, and the reasons are all ones that would
/// otherwise surface only after clew had quit for the swap: the helper's first
/// `mv` failed on a read-only disk image, inside App Translocation, or in an
/// `/Applications` a standard user may not write — and with no app left
/// running, nobody saw why. They are now a status message and the release
/// page instead. The signing half keeps an ad-hoc-signed install from
/// downloading a whole disk image to be told there is no team to anchor an
/// update to. `install_dmg` re-checks all of it rather than trusting this
/// answer: the two calls are a download apart.
pub fn self_install_blocker() -> Option<String> {
    let Some(bundle) = installed_bundle() else {
        return Some("clew is not running from an installed app".into());
    };
    if let Err(e) = replaceable(&bundle) {
        return Some(e);
    }
    if let Err(e) = signing_identity(&bundle) {
        return Some(format!("this copy of clew cannot anchor an update ({e})"));
    }
    None
}

/// Whether the swap helper will be able to move `bundle` aside and put the new
/// version in its place.
fn replaceable(bundle: &Path) -> Result<(), String> {
    // Gatekeeper runs a quarantined app that was never moved from a randomized
    // read-only mirror; there is no "installed" copy to replace.
    if bundle.to_string_lossy().contains("/AppTranslocation/") {
        return Err(
            "clew is running from a quarantined, translocated copy — move \
                    Clew.app into /Applications and open it from there"
                .into(),
        );
    }
    let parent = bundle
        .parent()
        .ok_or("the app bundle has no parent directory")?;
    // `access(W_OK)` answers for this user, ACLs and read-only mounts included
    // (a mounted disk image reports EROFS). The parent is where the backup and
    // the new copy are created; the bundle itself is renamed and, once the new
    // version is in, deleted — which needs write access inside it too.
    for (path, what) in [
        (parent, "the folder clew is installed in"),
        (bundle, "Clew.app"),
    ] {
        if let Err(e) = writable(path) {
            return Err(match e.raw_os_error() {
                Some(libc::EROFS) => {
                    "clew is running from a read-only volume (the disk image?) — copy \
                     Clew.app to /Applications and open it from there"
                        .to_string()
                }
                _ => format!(
                    "this account cannot modify {what} ({}): {e}",
                    path.display()
                ),
            });
        }
    }
    Ok(())
}

/// `access(path, W_OK)` for the current user.
fn writable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("path contains a NUL byte"))?;
    // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
    if unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Verify a downloaded DMG's `Clew.app`, stage it, then hand off to a detached
/// helper that swaps it in and relaunches once this process exits. Returns as
/// soon as the helper is launched; the caller then quits the app so the helper
/// can proceed. `expected` is the release version the user was offered;
/// `reopen` is the project to reopen after relaunch, if any. Blocking; run off
/// the UI thread.
pub fn install_dmg(dmg: &Path, expected: Version, reopen: Option<PathBuf>) -> Result<(), String> {
    let result = install_dmg_inner(dmg, expected, reopen);
    if let Err(e) = &result {
        eprintln!("[clew] update not installed: {e}");
    }
    result
}

fn install_dmg_inner(dmg: &Path, expected: Version, reopen: Option<PathBuf>) -> Result<(), String> {
    let target = installed_bundle()
        .ok_or("clew is not running from an installed app, so it can't self-update")?;
    // Re-checked, not taken from the pre-download answer: an install that
    // cannot complete must fail HERE, while clew is still running to say so.
    replaceable(&target)?;
    // Fails outright on an ad-hoc-signed install, which reports `TeamIdentifier=
    // not set`: there is no team to anchor an update to, so there is nothing
    // this path could check. That is the tier a release with no signing secrets
    // produces, and it has never been able to self-update. `App` asks
    // `self_install_blocker()` before the download, so that tier is routed to
    // the release page rather than being told this after waiting for a disk
    // image. Re-read here regardless: it is the identity every signature check
    // below is made against, so it is not taken on the earlier answer's word.
    let identity = signing_identity(&target)
        .map_err(|e| format!("cannot read this app's signing identity: {e}"))?;
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
    let parent = staging_parent()?;
    let staging = crate::updater::create_private_dir(&parent, STAGING_PREFIX)?;
    let launched = stage_from_image(dmg, &identity, expected, &staging).and_then(|staged_app| {
        spawn_swap_helper(&SwapPlan {
            pid: std::process::id(),
            staged_app: &staged_app,
            staging: &staging,
            target: &target,
            reopen: reopen.as_deref(),
            log: &parent.join(crate::updater::INSTALL_LOG),
            failed_marker: &parent.join(crate::updater::INSTALL_FAILED_MARKER),
            opener: Path::new(OPENER),
            wait_ticks: EXIT_WAIT_TICKS,
        })
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
fn stage_from_image(
    dmg: &Path,
    identity: &SigningIdentity,
    expected: Version,
    staging: &Path,
) -> Result<PathBuf, String> {
    let image = attach_image(dmg)?;

    // Everything between mount and detach goes through this closure so we always
    // unmount, even on an early error.
    let staged = (|| -> Result<PathBuf, String> {
        let src_app = image.mount_point.join("Clew.app");
        if !src_app.exists() {
            return Err("the update image has no Clew.app".into());
        }
        verify(&src_app, identity)?;
        // After `verify`: the Info.plist is sealed by the signature just
        // checked, so the version read from it is the signer's.
        check_candidate_version(
            &bundle_short_version(&src_app)?,
            expected,
            crate::updater::current_version(),
        )?;
        let staged_app = staging.join("Clew.app");
        run(Command::new("/usr/bin/ditto")
            .arg(&src_app)
            .arg(&staged_app))
        .map_err(|e| format!("could not stage the update: {e}"))?;
        Ok(staged_app)
    })();

    detach_image(&image.image);
    staged
}

/// The update image, attached.
#[derive(Debug, PartialEq, Eq)]
struct Attached {
    /// The image file, canonical: what it was attached by, which is what
    /// `hdiutil info` lists it under, and so what it is detached by
    /// ([`detach_image`]).
    image: PathBuf,
    /// Where its volume is mounted.
    mount_point: PathBuf,
}

/// Attach `dmg` read-only — no Finder window, no auto-open — and find its
/// volume in what `hdiutil` says about it ([`parse_attach_plist`]).
fn attach_image(dmg: &Path) -> Result<Attached, String> {
    attach_image_reading(dmg, parse_attach_plist)
}

/// [`attach_image`], with what reads `hdiutil attach`'s answer given (a
/// test's fails).
///
/// The image is attached by its canonical path, because that is the path
/// `hdiutil info` then lists it under: it lists the one it was given, a
/// symlink's included. And once `hdiutil attach` has run, any failure
/// detaches the image again, by that path: when the answer could not be
/// read, it used to stay attached for the rest of the session, and the
/// strict parser refuses an answer for any token it does not expect.
fn attach_image_reading(
    dmg: &Path,
    read: impl FnOnce(&str) -> Option<PathBuf>,
) -> Result<Attached, String> {
    let image =
        std::fs::canonicalize(dmg).map_err(|e| format!("could not open the update image: {e}"))?;
    let attached = run(Command::new("/usr/bin/hdiutil")
        .args([
            "attach",
            "-plist",
            "-nobrowse",
            "-readonly",
            "-noverify",
            "-noautoopen",
        ])
        .arg(&image))
    .map_err(|e| format!("could not open the update image: {e}"))
    .and_then(|plist| read(&plist).ok_or_else(|| "could not find the update volume".into()));
    match attached {
        Ok(mount_point) => Ok(Attached { image, mount_point }),
        Err(e) => {
            // Usually nothing is attached, and the lookup says so.
            detach_image(&image);
            Err(e)
        }
    }
}

/// How often a detach is tried before it is forced, how often in all, and
/// the pause after the first failure (it grows with each).
const DETACH_PLAIN: u32 = 3;
const DETACH_ATTEMPTS: u32 = 6;
const DETACH_PAUSE: std::time::Duration = std::time::Duration::from_millis(300);

/// Detach the image attached from the file `image` (canonical), if it still
/// is. A volume just mounted is often busy for a moment — Spotlight and
/// fseventsd look at every new one — and a lone `hdiutil detach` then fails
/// ("Resource busy"), leaving the update image attached for the rest of the
/// session. It is tried again after a pause, then forced: nothing of clew's
/// is open on it any more. On a busy machine a forced detach, or the lookup
/// before it, fails now and then too, so the forcing is tried again as well
/// — about five seconds in all, off the UI thread.
///
/// The image is found by what it is, each time: the entry `hdiutil info`
/// lists for the file ([`attached_disk`]), detached as the whole disk that
/// entry names now. Never by a name that can come to mean another disk. A
/// mount path names whatever is mounted there now, and once the update
/// volume is unmounted by anyone else, another volume can take its name. A
/// device node is no better: disk numbers are handed out again, lowest
/// first, so once someone else ejects the update image, the next image or
/// USB disk attached takes its number. Detached by either, that disk was
/// ejected in the update image's place. An image `hdiutil info` does not
/// list is detached already: nothing is ejected.
///
/// What is left is the moment between a lookup and its detach: the image
/// ejected by someone else within it, and its number taken by another disk
/// within it too.
fn detach_image(image: &Path) {
    for attempt in 1..=DETACH_ATTEMPTS {
        match attached_disk(image) {
            Ok(None) => return,
            Ok(Some(disk)) => {
                let mut detach = Command::new("/usr/bin/hdiutil");
                detach.arg("detach");
                if attempt > DETACH_PLAIN {
                    detach.arg("-force");
                }
                if run(detach.arg("-quiet").arg(&disk)).is_ok() {
                    return;
                }
            }
            // `hdiutil info` failed: looked at again after the pause, the
            // way a failed detach is tried again.
            Err(_) => {}
        }
        if attempt < DETACH_ATTEMPTS {
            std::thread::sleep(DETACH_PAUSE * attempt);
        }
    }
}

/// The disk the image file `image` (canonical) is attached as now, as
/// `hdiutil info` lists it ([`parse_info_plist`]); `None` when it is not
/// attached.
fn attached_disk(image: &Path) -> Result<Option<PathBuf>, String> {
    let plist = run(Command::new("/usr/bin/hdiutil").args(["info", "-plist"]))?;
    parse_info_plist(&plist, image).ok_or_else(|| "could not read what hdiutil info said".into())
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

/// Relaunches the app after the swap (or the old one after a failed swap).
const OPENER: &str = "/usr/bin/open";

/// How long the helper waits for clew to exit, in 0.1 s ticks. Past that it
/// gives up WITHOUT swapping: moving the bundle out from under a clew that is
/// still running, then opening a second copy, is worse than no update.
const EXIT_WAIT_TICKS: u32 = 600;

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

/// Who signed a bundle, as `codesign` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SigningIdentity {
    /// The code-signing identifier (the bundle id for an app).
    identifier: String,
    /// The Team Identifier.
    team: String,
}

/// The signing identity of `bundle`, read from `codesign` (which prints its
/// details to stderr).
fn signing_identity(bundle: &Path) -> Result<SigningIdentity, String> {
    let out = Command::new("/usr/bin/codesign")
        .args(["-d", "--verbose=4"])
        .arg(bundle)
        .output()
        .map_err(|e| e.to_string())?;
    parse_signing_identity(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

/// Pull `Identifier=` and `TeamIdentifier=` out of `codesign -d` output. Both
/// are interpolated into a requirement expression later, so anything but
/// their plain character sets is refused rather than escaped.
fn parse_signing_identity(text: &str) -> Result<SigningIdentity, String> {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(name))
            .map(|s| s.trim().to_string())
    };
    let team = field("TeamIdentifier=")
        .filter(|s| !s.is_empty() && s != "not set")
        .ok_or("the bundle has no Team Identifier")?;
    if !team.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("`{team}` is not a usable Team Identifier"));
    }
    let identifier = field("Identifier=")
        .filter(|s| !s.is_empty())
        .ok_or("the bundle has no signing identifier")?;
    if !identifier
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(format!("`{identifier}` is not a usable signing identifier"));
    }
    Ok(SigningIdentity { identifier, team })
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

/// The `-R` argument an update's signature must satisfy.
///
/// - `anchor apple generic`: a certificate chain that really terminates at an
///   Apple root;
/// - `certificate 1[field.1.2.840.113635.100.6.2.6]`: the intermediate is
///   Apple's Developer ID CA, and `certificate leaf[field.1.2.840.113635.100.6.1.13]`:
///   the leaf is a Developer ID Application certificate — together the
///   standard Developer ID designated requirement, so a Mac App Store,
///   development or enterprise certificate issued to the same team does not
///   qualify;
/// - `identifier`: the same signing identifier as the running app, so another
///   app of the same team (or another clew flavor) cannot pose as an update;
/// - `certificate leaf[subject.OU]`: the team, read from the leaf certificate
///   instead of the self-declared CodeDirectory field.
///
/// The leading `=` is what makes codesign read this as requirement source
/// text; without it the string is taken as a path to a compiled requirement
/// and the check fails with "No such file or directory". Both interpolated
/// values were validated to plain character sets in `parse_signing_identity`,
/// and are re-checked here because this is where a quote would reshape the
/// expression being trusted.
fn requirement_arg(identity: &SigningIdentity) -> Result<String, String> {
    let SigningIdentity { identifier, team } = identity;
    if team.is_empty() || !team.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("`{team}` is not a usable Team Identifier"));
    }
    if identifier.is_empty()
        || !identifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(format!("`{identifier}` is not a usable signing identifier"));
    }
    Ok(format!(
        "=anchor apple generic and identifier \"{identifier}\" \
         and certificate 1[field.1.2.840.113635.100.6.2.6] exists \
         and certificate leaf[field.1.2.840.113635.100.6.1.13] exists \
         and certificate leaf[subject.OU] = \"{team}\""
    ))
}

/// Verify `app` is an intact clew build from the same signer as us.
fn verify(app: &Path, identity: &SigningIdentity) -> Result<(), String> {
    // Structural integrity plus the pinned requirement. `-R` is the whole point:
    // without it codesign only checks the bundle against its own designated
    // requirement, so an attacker who signs their own Clew.app passes.
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "-R"])
        .arg(requirement_arg(identity)?)
        .arg(app))
    .map_err(|e| format!("the update's signature is invalid: {e}"))?;
    // Belt and braces: what the bundle declares must agree with what the
    // requirement just matched. A disagreement means a hand-assembled
    // signature, not something Apple's toolchain produced.
    let declared = signing_identity(app)?;
    if declared != *identity {
        return Err(format!(
            "the update is signed as {} by team {} (this app is {} by {}), refusing to install it",
            declared.identifier, declared.team, identity.identifier, identity.team
        ));
    }
    Ok(())
}

/// The `CFBundleShortVersionString` an app bundle declares, as written.
fn bundle_short_version(app: &Path) -> Result<String, String> {
    let plist = app.join("Contents/Info.plist");
    run(Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :CFBundleShortVersionString"])
        .arg(&plist))
    .map(|out| out.trim().to_string())
    .map_err(|e| format!("the update does not state its version: {e}"))
}

/// The version gate: the verified candidate must be newer than what is
/// running — `clew_core::update::ensure_upgrade`, the no-downgrade rule the
/// update check applies to the release tag, applied here to the version the
/// BUNDLE declares — and exactly the release the user agreed to install.
fn check_candidate_version(
    candidate: &str,
    expected: Version,
    running: Version,
) -> Result<(), String> {
    let candidate = clew_core::update::ensure_upgrade(running, candidate)?;
    if candidate != expected {
        return Err(format!(
            "the update image holds clew {candidate}, but {expected} was offered; refusing it"
        ));
    }
    Ok(())
}

/// Where the volume in `hdiutil attach -plist`'s output is mounted: the
/// first system entity with a `mount-point` (and a `dev-entry`).
///
/// Read as the property list it is — what `hdiutil` tells programs to read —
/// rather than as the columns of its text output: a volume's name is the
/// image's to choose, and a crafted one holds the tabs and newlines those
/// columns are cut at, where the plist keeps it inside its own `<string>`.
fn parse_attach_plist(xml: &str) -> Option<PathBuf> {
    let Plist::Dict(root) = parse_plist(xml)? else {
        return None;
    };
    let Plist::Array(entities) = plist_field(&root, "system-entities")? else {
        return None;
    };
    entities.iter().find_map(|entity| {
        let Plist::Dict(fields) = entity else {
            return None;
        };
        plist_string(fields, "dev-entry").filter(|d| d.starts_with("/dev/"))?;
        plist_string(fields, "mount-point")
            .map(Path::new)
            .filter(|m| m.is_absolute())
            .map(Path::to_path_buf)
    })
}

/// In `hdiutil info -plist`'s output, the disk the image file `image`
/// (canonical) is attached as: `Some(None)` when no attached image is that
/// file, `None` when the document is not what `hdiutil info` prints.
///
/// An image is listed under the path it was attached by (`image-path`),
/// matched here as it is — against every entry first, since clew attaches
/// by the canonical path — and only then once resolved. Only an entry by
/// the image's own file name is resolved: another spelling of the file — a
/// symlinked or relative directory — keeps its name, and resolving any other
/// image's path touches that image's file system, which for one on a
/// network share can take long. So the lookup that finds clew's image not
/// listed — whether it is still attached, after a failed attach — resolves
/// nothing of anyone else's. Its disk is the first whole disk (`/dev/diskN`)
/// among its entities: the image's own, which `hdiutil` lists before any it
/// synthesized for it — an APFS container, which detaches the image all the
/// same, as any of its nodes does and is taken when it lists no whole disk.
fn parse_info_plist(xml: &str, image: &Path) -> Option<Option<PathBuf>> {
    let Plist::Dict(root) = parse_plist(xml)? else {
        return None;
    };
    let images = match plist_field(&root, "images") {
        Some(Plist::Array(images)) => images.as_slice(),
        Some(_) => return None,
        None => &[],
    };
    let listed = || {
        images.iter().filter_map(|entry| match entry {
            Plist::Dict(fields) => Some((Path::new(plist_string(fields, "image-path")?), fields)),
            _ => None,
        })
    };
    let ours = listed()
        .find(|(path, _)| *path == image)
        .or_else(|| {
            listed().find(|(path, _)| {
                path.file_name() == image.file_name()
                    && std::fs::canonicalize(path).is_ok_and(|p| p == image)
            })
        })
        .map(|(_, fields)| fields);
    let Some(fields) = ours else {
        return Some(None);
    };
    let Plist::Array(entities) = plist_field(fields, "system-entities")? else {
        return None;
    };
    let nodes: Vec<&str> = entities
        .iter()
        .filter_map(|entity| match entity {
            Plist::Dict(fields) => plist_string(fields, "dev-entry"),
            _ => None,
        })
        .filter(|node| node.starts_with("/dev/disk"))
        .collect();
    let whole = |node: &str| {
        let number = &node["/dev/disk".len()..];
        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
    };
    let disk = nodes
        .iter()
        .copied()
        .find(|node| whole(node))
        .or(nodes.first().copied())?;
    Some(Some(PathBuf::from(disk)))
}

/// The value of `key` in a plist dictionary's entries.
fn plist_field<'a>(entries: &'a [(String, Plist)], key: &str) -> Option<&'a Plist> {
    entries
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, value)| value)
}

/// The string value of `key` in a plist dictionary's entries.
fn plist_string<'a>(entries: &'a [(String, Plist)], key: &str) -> Option<&'a str> {
    match plist_field(entries, key)? {
        Plist::String(s) => Some(s),
        _ => None,
    }
}

/// A value in an XML property list, as far as `hdiutil`'s output is read:
/// dictionaries, arrays and strings, and any other value as just `Other`.
#[derive(Debug)]
enum Plist {
    Dict(Vec<(String, Plist)>),
    Array(Vec<Plist>),
    String(String),
    Other,
}

/// Parse an XML property list, strictly: `None` for anything else. The
/// document is cut at every `<`, which leaves one tag and the text after it
/// in each piece — XML escapes every `<` in text.
fn parse_plist(xml: &str) -> Option<Plist> {
    let mut pieces = xml.split('<').skip(1).map(|piece| piece.split_once('>'));
    // The prolog, then `<plist version="1.0">` around a single value.
    let value = loop {
        let (tag, _) = pieces.next()??;
        if tag.starts_with('?') || tag.starts_with("!DOCTYPE") {
            continue;
        }
        if tag != "plist" && !tag.starts_with("plist ") {
            return None;
        }
        let (tag, text) = pieces.next()??;
        break plist_value(tag, text, &mut pieces)?;
    };
    let (close, _) = pieces.next()??;
    (close == "/plist" && pieces.next().is_none()).then_some(value)
}

/// The value that opens with `tag` (`text` follows it), reading on through
/// `pieces` to its end. See [`parse_plist`].
fn plist_value<'a>(
    tag: &str,
    text: &str,
    pieces: &mut impl Iterator<Item = Option<(&'a str, &'a str)>>,
) -> Option<Plist> {
    match tag {
        "dict" => {
            let mut entries = Vec::new();
            loop {
                match pieces.next()?? {
                    ("/dict", _) => return Some(Plist::Dict(entries)),
                    ("key", key) => {
                        let key = xml_unescape(key)?;
                        let ("/key", _) = pieces.next()?? else {
                            return None;
                        };
                        let (tag, text) = pieces.next()??;
                        entries.push((key, plist_value(tag, text, pieces)?));
                    }
                    _ => return None,
                }
            }
        }
        "array" => {
            let mut items = Vec::new();
            loop {
                match pieces.next()?? {
                    ("/array", _) => return Some(Plist::Array(items)),
                    (tag, text) => items.push(plist_value(tag, text, pieces)?),
                }
            }
        }
        "string" => {
            let ("/string", _) = pieces.next()?? else {
                return None;
            };
            Some(Plist::String(xml_unescape(text)?))
        }
        "dict/" => Some(Plist::Dict(Vec::new())),
        "array/" => Some(Plist::Array(Vec::new())),
        "string/" => Some(Plist::String(String::new())),
        "true/" | "false/" => Some(Plist::Other),
        "integer" | "real" | "date" | "data" => {
            let (close, _) = pieces.next()??;
            (close.strip_prefix('/') == Some(tag)).then_some(Plist::Other)
        }
        _ => None,
    }
}

/// `text` with its XML escapes decoded: the five named entities and numeric
/// character references. `None` for any other `&`.
fn xml_unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let (entity, after) = rest[at + 1..].split_once(';')?;
        out.push(match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = match entity.strip_prefix("#x") {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => entity.strip_prefix('#')?.parse().ok()?,
                };
                char::from_u32(code)?
            }
        });
        rest = after;
    }
    out.push_str(rest);
    Some(out)
}

/// Everything the swap helper needs, fixed at spawn time.
struct SwapPlan<'a> {
    /// The clew process to wait for.
    pid: u32,
    /// The verified copy to install.
    staged_app: &'a Path,
    /// Its private directory, removed on every outcome.
    staging: &'a Path,
    /// The installed bundle to replace.
    target: &'a Path,
    /// A project to reopen after the relaunch.
    reopen: Option<&'a Path>,
    /// Appended with one line per outcome.
    log: &'a Path,
    /// Written with the reason when the swap fails, for the relaunched clew
    /// to report (`crate::updater::take_install_failure`).
    failed_marker: &'a Path,
    /// What relaunches the app: `/usr/bin/open`, or a recorder in tests.
    opener: &'a Path,
    /// How long to wait for `pid` to exit, in 0.1 s ticks.
    wait_ticks: u32,
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
/// just past the move of the installed app destroys it. The text is only
/// paths.)
fn spawn_swap_helper(plan: &SwapPlan) -> Result<(), String> {
    let script = swap_script(plan);
    // `std::process::Command` does not kill the child on drop, so the helper
    // outlives clew's exit and performs the swap.
    Command::new("/bin/bash")
        .arg("-c")
        .arg(&script)
        .spawn()
        .map_err(|e| format!("could not start the updater helper: {e}"))?;
    Ok(())
}

/// The helper's body: wait for clew to exit (or give up without touching
/// anything), move the installed app aside, `ditto` the verified copy in, and
/// relaunch — the NEW version on success, the RESTORED old one on any failure,
/// so a failed update never leaves the user with no clew running and no word
/// of why. Each outcome is logged, and a failure's reason is also left in
/// `failed_marker` for the relaunched clew to show.
///
/// Every tool is named by absolute path and `PATH` is pinned, so nothing on
/// the user's `PATH` stands in for `mv` or `rm` in a script that deletes
/// trees. Every interpolated path is single-quoted. No shebang: this is an
/// argument to `bash -c`, never a file.
///
/// The error output of every step that can fail the swap — `mv`, `ditto`,
/// the `rm` before a restore, `open` — is appended to the log, so a failed
/// update's log says WHY (a full disk, a permission, a locked file) rather
/// than only that a step failed: the helper has no terminal, and clew has
/// already quit.
fn swap_script(plan: &SwapPlan) -> String {
    let reopen_args = match plan.reopen {
        Some(p) => format!(" --args {}", quote(&p.to_string_lossy())),
        None => String::new(),
    };
    format!(
        "set -u\n\
         PATH=/usr/bin:/bin:/usr/sbin:/sbin\n\
         export PATH\n\
         TARGET={target}\n\
         STAGED={staged}\n\
         STAGING={staging}\n\
         LOG={log}\n\
         FAILED={failed}\n\
         OPENER={opener}\n\
         log() {{ printf '%s %s\\n' \"$(/bin/date '+%Y-%m-%dT%H:%M:%S')\" \"$1\" >> \"$LOG\" 2>/dev/null; }}\n\
         fail() {{ log \"update failed: $1\"; printf '%s\\n' \"$1\" > \"$FAILED\" 2>/dev/null; }}\n\
         relaunch() {{ \"$OPENER\" \"$TARGET\"{reopen_args} 2>>\"$LOG\" || log \"could not open $TARGET\"; }}\n\
         i=0\n\
         while kill -0 {pid} 2>/dev/null; do\n\
         \ti=$((i + 1))\n\
         \tif [ \"$i\" -ge {ticks} ]; then\n\
         \t\tfail \"clew did not quit, so the update was not applied\"\n\
         \t\t/bin/rm -rf \"$STAGING\"\n\
         \t\texit 1\n\
         \tfi\n\
         \t/bin/sleep 0.1\n\
         done\n\
         BACKUP=\"$TARGET.bak-$$\"\n\
         /bin/rm -rf \"$BACKUP\"\n\
         if [ -e \"$TARGET\" ] && ! /bin/mv \"$TARGET\" \"$BACKUP\" 2>>\"$LOG\"; then\n\
         \tfail \"could not move the installed app aside; nothing was changed\"\n\
         \t/bin/rm -rf \"$STAGING\"\n\
         \trelaunch\n\
         \texit 1\n\
         fi\n\
         if /usr/bin/ditto \"$STAGED\" \"$TARGET\" 2>>\"$LOG\"; then\n\
         \t/bin/rm -rf \"$BACKUP\"\n\
         \tlog \"installed $TARGET\"\n\
         else\n\
         \t/bin/rm -rf \"$TARGET\" 2>>\"$LOG\"\n\
         \tif [ -e \"$BACKUP\" ] && ! /bin/mv \"$BACKUP\" \"$TARGET\" 2>>\"$LOG\"; then\n\
         \t\tfail \"could not install the update, nor restore the previous version (it is at $BACKUP)\"\n\
         \t\t/bin/rm -rf \"$STAGING\"\n\
         \t\texit 1\n\
         \tfi\n\
         \tfail \"could not copy the new version into place; the previous version was restored\"\n\
         \t/bin/rm -rf \"$STAGING\"\n\
         \trelaunch\n\
         \texit 1\n\
         fi\n\
         /bin/rm -rf \"$STAGING\"\n\
         relaunch\n",
        pid = plan.pid,
        ticks = plan.wait_ticks,
        target = quote(&plan.target.to_string_lossy()),
        staged = quote(&plan.staged_app.to_string_lossy()),
        staging = quote(&plan.staging.to_string_lossy()),
        log = quote(&plan.log.to_string_lossy()),
        failed = quote(&plan.failed_marker.to_string_lossy()),
        opener = quote(&plan.opener.to_string_lossy()),
    )
}

/// Single-quote a string for safe interpolation into the shell helper.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::test_dir;

    fn identity(identifier: &str, team: &str) -> SigningIdentity {
        SigningIdentity {
            identifier: identifier.into(),
            team: team.into(),
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    /// Build a minimal ad-hoc signed `.app` under `dir`. Ad-hoc means no
    /// certificate chain at all, which is what a forged update looks like to
    /// everything except a requirement that names the anchor.
    fn ad_hoc_bundle(dir: &Path) -> PathBuf {
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
             <key>CFBundleShortVersionString</key><string>9.8.7</string>\
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

    /// The requirement names an Apple anchor, the Developer ID intermediate
    /// and leaf OIDs, the running app's own identifier, and the team from the
    /// leaf certificate — and it has to carry the leading `=`, since codesign
    /// reads a `-R` argument without it as a path to a compiled requirement.
    #[test]
    fn requirement_pins_developer_id_the_identifier_and_the_team() {
        let arg = requirement_arg(&identity("com.rutatang.clew", "ABCDE12345")).unwrap();
        assert!(arg.starts_with("=anchor apple generic and "), "{arg}");
        for clause in [
            "identifier \"com.rutatang.clew\"",
            "certificate 1[field.1.2.840.113635.100.6.2.6] exists",
            "certificate leaf[field.1.2.840.113635.100.6.1.13] exists",
            "certificate leaf[subject.OU] = \"ABCDE12345\"",
        ] {
            assert!(arg.contains(clause), "missing {clause:?} in {arg}");
        }
        // csreq is the compiler codesign hands `-R` text to, so this proves the
        // expression parses rather than just that we spelled a string.
        let out = Command::new("/usr/bin/csreq")
            .args(["-r", &arg, "-b", "/dev/null"])
            .output()
            .unwrap();
        assert!(out.status.success(), "requirement did not compile: {out:?}");
    }

    /// Both values are interpolated into a requirement expression, so anything
    /// that could close a quote and bolt on another clause is refused outright.
    #[test]
    fn requirement_refuses_values_that_could_reshape_it() {
        for bad in ["", "AB\" or anchor apple generic and \"", "AB CDE", "AB-CD"] {
            assert!(
                requirement_arg(&identity("com.rutatang.clew", bad)).is_err(),
                "team {bad:?} should not build a requirement"
            );
        }
        for bad in ["", "com.x\" or anchor apple", "com x", "com/x"] {
            assert!(
                requirement_arg(&identity(bad, "ABCDE12345")).is_err(),
                "identifier {bad:?} should not build a requirement"
            );
        }
    }

    #[test]
    fn signing_identity_is_read_from_codesign_output() {
        let text = "Executable=/Applications/Clew.app/Contents/MacOS/clew\n\
                    Identifier=com.rutatang.clew\n\
                    Format=app bundle with Mach-O thin (arm64)\n\
                    Authority=Developer ID Application: Someone (ABCDE12345)\n\
                    TeamIdentifier=ABCDE12345\n";
        assert_eq!(
            parse_signing_identity(text).unwrap(),
            identity("com.rutatang.clew", "ABCDE12345")
        );
        // Ad-hoc: no team, nothing to anchor to.
        assert!(
            parse_signing_identity("Identifier=x\nSignature=adhoc\nTeamIdentifier=not set\n")
                .is_err()
        );
        // A value outside the plain character set is refused, not escaped.
        assert!(parse_signing_identity("Identifier=a\"b\nTeamIdentifier=ABC\n").is_err());
    }

    /// The gate the finding was about: `codesign --verify --deep --strict` alone
    /// exits 0 on a bundle with no certificate chain whatsoever, because it
    /// checks the candidate against the candidate's own designated requirement.
    /// Only the pinned `-R` requirement rejects it.
    #[test]
    fn ad_hoc_bundle_passes_bare_verify_but_fails_the_pinned_requirement() {
        let dir = test_dir("update-verify");
        std::fs::create_dir_all(&dir).unwrap();
        let app = ad_hoc_bundle(&dir);

        let bare = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&app)
            .status()
            .unwrap();
        assert!(bare.success(), "the old gate is supposed to accept this");

        let err = verify(&app, &identity("com.example.fake", "ABCDE12345")).unwrap_err();
        assert!(
            err.contains("signature is invalid"),
            "expected the signature check to reject it, got: {err}"
        );
        // The version gate reads the bundle's own plist.
        assert_eq!(bundle_short_version(&app).unwrap(), "9.8.7");
    }

    /// Gap 1 (tests findings): the image half of an install, run for real
    /// on disk images `hdiutil` builds here. The volume is attached, the
    /// `Clew.app` on it must pass the pinned signature check before anything
    /// is copied — this machine has no Developer ID to sign one, so every
    /// image here is refused, which is the half an attacker controls — and on
    /// every outcome the image is detached again and staging is left empty.
    /// On both file systems an update image can have ([`FILESYSTEMS`]).
    #[test]
    fn an_image_is_staged_only_past_the_signature_check_and_always_detached() {
        let _disks = disks();
        let dir = test_dir("update-stage");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let staging = dir.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let staged_nothing = || std::fs::read_dir(&staging).unwrap().next().is_none();
        let signer = identity("com.example.fake", "ABCDE12345");
        for (n, fs) in FILESYSTEMS.into_iter().enumerate() {
            // Unique per run: a leftover volume of the same name would make
            // `hdiutil` mount this one elsewhere.
            let volume = format!("ClewStageTest{}-{n}", std::process::id());
            let image = |name: &str, app: bool| -> Detached {
                let src = dir.join(format!("{name}-{n}-src"));
                std::fs::create_dir_all(&src).unwrap();
                if app {
                    std::fs::rename(ad_hoc_bundle(&src), src.join("Clew.app")).unwrap();
                } else {
                    std::fs::write(src.join("README.txt"), "no app here\n").unwrap();
                }
                Detached(create_image(
                    &src,
                    &dir.join(format!("{name}-{n}.dmg")),
                    &volume,
                    fs,
                ))
            };

            let forged = image("forged", true);
            let err = past_busy(|| stage_from_image(&forged.0, &signer, v("9.8.7"), &staging))
                .unwrap_err();
            assert!(err.contains("signature is invalid"), "{fs:?}: {err}");
            assert!(
                !left_attached(&forged.0),
                "{fs:?}: the refused image was left attached"
            );
            assert!(staged_nothing(), "{fs:?}: a refused bundle was staged");

            let empty = image("empty", false);
            let err = past_busy(|| stage_from_image(&empty.0, &signer, v("9.8.7"), &staging))
                .unwrap_err();
            assert!(err.contains("has no Clew.app"), "{fs:?}: {err}");
            assert!(
                !left_attached(&empty.0),
                "{fs:?}: the image was left attached"
            );
            assert!(staged_nothing(), "{fs:?}");
        }

        let junk = dir.join("junk.dmg");
        std::fs::write(&junk, b"not a disk image").unwrap();
        let err = stage_from_image(&junk, &signer, v("9.8.7"), &staging).unwrap_err();
        assert!(err.contains("could not open the update image"), "{err}");
        assert!(staged_nothing());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Held by each test here that attaches a disk image: shared, so they
    /// run side by side — and exclusively by the one that needs the disk
    /// numbers to itself
    /// ([`a_disk_that_took_the_update_images_number_is_not_ejected_in_its_place`]).
    ///
    /// Across processes, not only threads: the machine's disk numbers are
    /// every process's, and two test runs at once — two checkouts' — are
    /// what a busy machine has. That test frees a number for another disk
    /// to take, and ejects by it; held within one run only, the lock let
    /// another run's image take the number and be ejected in the middle of
    /// its own test. So it is a `flock`, on a file every run sees and none
    /// writes — `hdiutil` itself — which leaves nothing behind, and which a
    /// run that dies lets go of.
    fn lock_disks(exclusive: bool) -> std::fs::File {
        let file = std::fs::File::open("/usr/bin/hdiutil").expect("hdiutil is where it always is");
        let locked = if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        };
        locked.expect("the disk images' lock");
        file
    }

    /// A share of the disks ([`lock_disks`]), held until dropped.
    fn disks() -> std::fs::File {
        lock_disks(false)
    }

    /// The file systems update images are tested on: HFS+, and APFS — what
    /// `hdiutil create` makes without `-fs`, so what release.yml ships: its
    /// volume lives on a container disk `hdiutil` synthesizes, a second
    /// whole disk of the image's.
    const FILESYSTEMS: [Option<&str>; 2] = [Some("HFS+"), None];

    /// How often a step of `hdiutil`'s that failed for want of the service,
    /// not for what a test is about, is tried in all: on a busy machine —
    /// two test runs at once, Spotlight looking at every fresh volume — the
    /// disk-image service keeps failing for seconds, not for one pause.
    const TRIES: u32 = 5;

    /// The pause after try `attempt` failed: half a second, doubled after
    /// each try — 7.5 s in all.
    fn pause(attempt: u32) {
        std::thread::sleep(std::time::Duration::from_millis(500) * 2u32.pow(attempt - 1));
    }

    /// Run `command` until it succeeds, [`TRIES`] times at most: `hdiutil`
    /// fails now and then on a busy machine ("Resource busy" on CI runners,
    /// and on any fresh volume Spotlight is still looking at).
    fn retried(mut command: impl FnMut() -> Command) -> bool {
        (1..=TRIES).any(|attempt| {
            let ok = command().status().is_ok_and(|s| s.success());
            if !ok && attempt < TRIES {
                pause(attempt);
            }
            ok
        })
    }

    /// Whether `hdiutil` failed, as `error` says, the way it does now and
    /// then on a busy machine: its service momentarily unavailable or busy,
    /// or a volume that did not mount in time.
    fn transient(error: &str) -> bool {
        [
            "temporarily unavailable",
            "busy",
            "timed out",
            "no mountable file systems",
        ]
        .iter()
        .any(|how| error.contains(how))
    }

    /// `f`, run again — [`TRIES`] times at most — while it fails the way
    /// `hdiutil` does now and then on a busy machine ([`transient`]), rather
    /// than for what a test is about.
    fn past_busy<T>(mut f: impl FnMut() -> Result<T, String>) -> Result<T, String> {
        let mut attempt = 1;
        loop {
            match f() {
                Err(e) if attempt < TRIES && transient(&e) => {
                    pause(attempt);
                    attempt += 1;
                }
                result => return result,
            }
        }
    }

    /// A disk image `dmg` of the folder `src`, its volume named `volume`, on
    /// file system `fs` (`hdiutil`'s default when `None`). Returns its
    /// canonical path, which the image is attached and looked up by.
    fn create_image(src: &Path, dmg: &Path, volume: &str, fs: Option<&str>) -> PathBuf {
        let created = retried(|| {
            let mut create = Command::new("/usr/bin/hdiutil");
            create.args(["create", "-quiet", "-ov", "-volname", volume]);
            if let Some(fs) = fs {
                create.args(["-fs", fs]);
            }
            create.arg("-srcfolder").arg(src).arg(dmg);
            create
        });
        assert!(created, "hdiutil create failed ({fs:?})");
        dmg.canonicalize().unwrap()
    }

    /// A disk image of `dir/src` whose volume is named `volume`, on file
    /// system `fs`, attached the way an update image is ([`attach_image`]).
    fn attached_image(dir: &Path, volume: &str, fs: Option<&str>) -> Attached {
        let dmg = create_image(
            &dir.join("src"),
            &dir.join(format!("{volume}.dmg")),
            volume,
            fs,
        );
        past_busy(|| attach_image(&dmg)).unwrap()
    }

    /// [`attached_disk`], asked again — [`TRIES`] times at most — while
    /// `hdiutil info` fails, as it does now and then on a busy machine: a
    /// read that failed says nothing about what a test looks for.
    fn disk_of(image: &Path) -> Result<Option<PathBuf>, String> {
        let mut attempt = 1;
        loop {
            match attached_disk(image) {
                Err(_) if attempt < TRIES => {
                    pause(attempt);
                    attempt += 1;
                }
                result => return result,
            }
        }
    }

    /// How long `hdiutil info` may go on listing an image whose detach has
    /// returned: on a busy machine it trails the detach, an APFS image's
    /// container disk torn down after the image's own.
    const SETTLE: std::time::Duration = std::time::Duration::from_secs(10);

    /// The disk the image attached from the file `image` is still attached
    /// as, once `hdiutil info` has had [`SETTLE`] to stop listing an image
    /// detached a moment ago; `None` when it is not.
    fn still_attached(image: &Path) -> Option<PathBuf> {
        let since = std::time::Instant::now();
        loop {
            match disk_of(image).unwrap() {
                None => return None,
                Some(disk) if since.elapsed() >= SETTLE => return Some(disk),
                Some(_) => std::thread::sleep(std::time::Duration::from_millis(250)),
            }
        }
    }

    /// Whether a volume is mounted at `mount_point`: it is there, on
    /// another device than the directory it is in.
    fn mounted_at(mount_point: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let device = |path: &Path| std::fs::metadata(path).map(|m| m.dev());
        match (device(mount_point), mount_point.parent().map(device)) {
            (Ok(here), Some(Ok(parent))) => here != parent,
            _ => false,
        }
    }

    /// Whether the image attached from the file `image` is still attached
    /// ([`still_attached`]), detaching it by force if so.
    fn left_attached(image: &Path) -> bool {
        let Some(disk) = still_attached(image) else {
            return false;
        };
        let _ = Command::new("/usr/bin/hdiutil")
            .args(["detach", "-force", "-quiet"])
            .arg(&disk)
            .status();
        true
    }

    /// Say, where a run shows it, that test `test` was skipped in part, and
    /// why: to the process's own standard output — the harness captures
    /// what `println!` writes, and shows it only for a test that fails —
    /// and on GitHub Actions as a warning, which the run's summary lists.
    fn report_skip(test: &str, why: &str) {
        use std::io::Write;
        let github = std::env::var_os("GITHUB_ACTIONS").is_some();
        let _ = writeln!(std::io::stdout(), "{}", skip_notice(test, why, github));
    }

    /// The line [`report_skip`] writes: a GitHub Actions warning command on
    /// a runner, a plain line elsewhere.
    fn skip_notice(test: &str, why: &str, github: bool) -> String {
        if github {
            format!("::warning title=Test skipped in part: {test}::{why}")
        } else {
            format!("warning: {test} skipped in part: {why}")
        }
    }

    /// A skip is said where a run shows it: on GitHub Actions, as a warning
    /// on the run.
    #[test]
    fn a_skip_is_a_warning_on_the_run() {
        assert_eq!(
            skip_notice("a_test", "no disk to test with", true),
            "::warning title=Test skipped in part: a_test::no disk to test with"
        );
        assert_eq!(
            skip_notice("a_test", "no disk to test with", false),
            "warning: a_test skipped in part: no disk to test with"
        );
    }

    /// Detaches the image attached from the file it holds, if it still is,
    /// when dropped — the cleanup a failing assertion must not skip.
    struct Detached(PathBuf);

    impl Drop for Detached {
        fn drop(&mut self) {
            if let Ok(Some(disk)) = disk_of(&self.0) {
                let _ = Command::new("/usr/bin/hdiutil")
                    .args(["detach", "-force", "-quiet"])
                    .arg(&disk)
                    .status();
            }
        }
    }

    /// A volume that is busy — here a file still open on it; on a runner,
    /// Spotlight looking at a fresh volume — refuses a plain detach ("Resource
    /// busy"). The update image is detached anyway: tried again, then forced,
    /// never left attached for the rest of the session.
    #[test]
    fn a_busy_update_image_is_detached_anyway() {
        let _disks = disks();
        let dir = test_dir("update-detach-busy");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/held.txt"), "busy\n").unwrap();
        for (n, fs) in FILESYSTEMS.into_iter().enumerate() {
            let volume = format!("ClewDetachTest{}-{n}", std::process::id());
            let image = attached_image(&dir, &volume, fs);
            let _cleanup = Detached(image.image.clone());
            let open = std::fs::File::open(image.mount_point.join("held.txt"))
                .expect("the volume is mounted");
            detach_image(&image.image);
            let busy_detached = still_attached(&image.image).is_none();
            drop(open);
            let left = left_attached(&image.image);
            assert!(
                busy_detached && !left,
                "{fs:?}: the busy image was left attached"
            );
        }
    }

    /// The image is detached whatever became of its volume, never by its
    /// mount path, which names whatever is mounted there now. Here the volume
    /// is unmounted behind clew's back: the path is gone while the image
    /// stays attached, and a detach by the path left it so for the rest of
    /// the session — by the same token as it could have hit another volume
    /// mounted at that path since.
    #[test]
    fn an_update_image_whose_volume_was_unmounted_is_still_detached() {
        let _disks = disks();
        let dir = test_dir("update-detach-unmounted");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.txt"), "a\n").unwrap();
        for (n, fs) in FILESYSTEMS.into_iter().enumerate() {
            let volume = format!("ClewUnmountTest{}-{n}", std::process::id());
            let image = attached_image(&dir, &volume, fs);
            let _cleanup = Detached(image.image.clone());
            // A fresh volume can be busy for a moment, as the detach knows —
            // and an unmount that failed on a busy machine can finish all
            // the same, when the next one fails for having nothing to do. So
            // what counts is whether the volume is still mounted.
            let mut unmounted = false;
            for attempt in 1..=TRIES {
                let how = if attempt <= 2 { "-quiet" } else { "-force" };
                let _ = Command::new("/usr/bin/hdiutil")
                    .args(["unmount", how])
                    .arg(&image.mount_point)
                    .status();
                unmounted = !mounted_at(&image.mount_point);
                if unmounted || attempt == TRIES {
                    break;
                }
                pause(attempt);
            }
            let still_attached = disk_of(&image.image).unwrap().is_some();
            detach_image(&image.image);
            let left = left_attached(&image.image);
            assert!(
                unmounted && still_attached,
                "{fs:?}: the volume did not unmount alone"
            );
            assert!(!left, "{fs:?}: the image was left attached");
        }
    }

    /// Someone else ejects the update image while clew stages it, and the
    /// next disk attached takes its number: they are handed out again,
    /// lowest first. Detaching the update image then leaves that disk alone —
    /// the image is looked up by its file, found detached already, and
    /// nothing is ejected. By the number it had, the other disk was ejected
    /// in its place.
    ///
    /// Whether the next disk gets the number is the machine's to say: a disk
    /// attached elsewhere meanwhile takes it first. Such a try tells nothing,
    /// and is made again; a file system on which no try did is reported as
    /// skipped, where the run shows it ([`report_skip`]) — never passed in
    /// silence, and never failed for what says nothing of clew.
    #[test]
    fn a_disk_that_took_the_update_images_number_is_not_ejected_in_its_place() {
        // No other test of these, in this run or another, attaches a disk
        // meanwhile: it would take the number first — and be ejected by it.
        let _disks = lock_disks(true);
        let dir = test_dir("update-detach-reused");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.txt"), "a\n").unwrap();
        const TRIES_PER_FS: usize = 3;
        for (n, fs) in FILESYSTEMS.into_iter().enumerate() {
            let mut reused = false;
            for attempt in 0..TRIES_PER_FS {
                let name = |role: &str| format!("Clew{role}{}-{n}-{attempt}", std::process::id());
                let update = attached_image(&dir, &name("Update"), fs);
                let _cleanup = Detached(update.image.clone());
                let number = disk_of(&update.image).unwrap().expect("attached");
                // Ejected by its number, as someone else would — and tried
                // again only while the number is still the update image's:
                // once it is not, it may be another disk's.
                let mut tries = 1;
                while !Command::new("/usr/bin/hdiutil")
                    .args(["detach", "-quiet"])
                    .arg(&number)
                    .status()
                    .is_ok_and(|s| s.success())
                    && tries < TRIES
                    && disk_of(&update.image).unwrap().as_ref() == Some(&number)
                {
                    pause(tries);
                    tries += 1;
                }
                // Gone, not only on its way: the next disk is attached once
                // the number is free.
                assert!(
                    still_attached(&update.image).is_none(),
                    "{fs:?}: could not eject the update image"
                );
                let other = attached_image(&dir, &name("Other"), fs);
                let _cleanup_other = Detached(other.image.clone());
                let other_disk = disk_of(&other.image).unwrap();

                detach_image(&update.image);

                let kept = disk_of(&other.image).unwrap() == other_disk;
                let left = left_attached(&other.image);
                assert!(
                    kept && left,
                    "{fs:?}: the disk that took the update image's number, {other_disk:?}, \
                     was ejected in its place"
                );
                if other_disk.as_ref() == Some(&number) {
                    reused = true;
                    break;
                }
            }
            if !reused {
                report_skip(
                    "a_disk_that_took_the_update_images_number_is_not_ejected_in_its_place",
                    &format!(
                        "{fs:?}: none of the {TRIES_PER_FS} disk numbers the update image freed \
                         went to the disk attached next; a disk attached elsewhere took it first"
                    ),
                );
            }
        }
    }

    /// `hdiutil attach` succeeded, but its answer could not be read — the
    /// strict parser refuses any token it does not expect. The image used to
    /// stay attached for the rest of the session; it is detached again.
    #[test]
    fn an_image_whose_attach_answer_cannot_be_read_is_detached_again() {
        let _disks = disks();
        let dir = test_dir("update-attach-unread");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.txt"), "a\n").unwrap();
        for (n, fs) in FILESYSTEMS.into_iter().enumerate() {
            let volume = format!("ClewUnreadTest{}-{n}", std::process::id());
            let dmg = create_image(
                &dir.join("src"),
                &dir.join(format!("{volume}.dmg")),
                &volume,
                fs,
            );
            let _cleanup = Detached(dmg.clone());
            let err = past_busy(|| attach_image_reading(&dmg, |_| None)).unwrap_err();
            let left = left_attached(&dmg);
            assert!(err.contains("could not find the update volume"), "{err}");
            assert!(
                !left,
                "{fs:?}: an image whose answer could not be read was left attached"
            );
        }
    }

    /// Provenance is not freshness: every older notarized Clew passes the
    /// signature check, so the version is checked on its own.
    #[test]
    fn only_the_offered_newer_version_is_accepted() {
        let running = v("0.2.0");
        assert!(check_candidate_version("0.3.0", v("0.3.0"), running).is_ok());
        let err = check_candidate_version("0.1.9", v("0.3.0"), running).unwrap_err();
        assert!(err.contains("not newer"), "{err}");
        assert!(check_candidate_version("0.2.0", v("0.2.0"), running).is_err());
        let err = check_candidate_version("0.4.0", v("0.3.0"), running).unwrap_err();
        assert!(err.contains("was offered"), "{err}");
        assert!(check_candidate_version("not a version", v("0.3.0"), running).is_err());
    }

    /// What `hdiutil attach -plist` prints for an HFS+ image (its tabs
    /// aside): the volume's entity first, the whole disk's after it.
    const HFS_ATTACH_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>system-entities</key>
  <array>
    <dict>
      <key>content-hint</key>
      <string>Apple_HFS</string>
      <key>dev-entry</key>
      <string>/dev/disk12s1</string>
      <key>mount-point</key>
      <string>/Volumes/Clew &amp; &lt;Test&gt;</string>
      <key>potentially-mountable</key>
      <true/>
      <key>unmapped-content-hint</key>
      <string>48465300-0000-11AA-AA11-00306543ECAC</string>
      <key>volume-kind</key>
      <string>hfs</string>
    </dict>
    <dict>
      <key>content-hint</key>
      <string>GUID_partition_scheme</string>
      <key>dev-entry</key>
      <string>/dev/disk12</string>
      <key>potentially-mountable</key>
      <false/>
      <key>unmapped-content-hint</key>
      <string>GUID_partition_scheme</string>
    </dict>
  </array>
</dict>
</plist>
"#;

    /// An `hdiutil attach -plist` document of `entities`, each a list of
    /// string fields written as XML text.
    fn attach_plist(entities: &[&[(&str, &str)]]) -> String {
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n\
             <dict>\n<key>system-entities</key>\n<array>\n",
        );
        for fields in entities {
            xml.push_str("<dict>\n");
            for (key, value) in *fields {
                xml.push_str(&format!("<key>{key}</key>\n<string>{value}</string>\n"));
            }
            xml.push_str("<key>potentially-mountable</key>\n<true/>\n</dict>\n");
        }
        xml.push_str("</array>\n</dict>\n</plist>\n");
        xml
    }

    /// The update volume is read from `hdiutil`'s property list: the first
    /// entity with a mount point (and a device) — its name's escapes decoded,
    /// and whole even where it holds the tabs and newlines that cut the text
    /// output's columns.
    #[test]
    fn the_update_volume_is_read_from_hdiutils_plist() {
        let volume = |mount_point: &str| Some(PathBuf::from(mount_point));
        assert_eq!(
            parse_attach_plist(HFS_ATTACH_PLIST),
            volume("/Volumes/Clew & <Test>")
        );
        // APFS: the volume is on a container disk listed after the image's.
        let apfs = attach_plist(&[
            &[("dev-entry", "/dev/disk12")],
            &[
                ("content-hint", "Apple_APFS"),
                ("dev-entry", "/dev/disk12s1"),
            ],
            &[
                ("dev-entry", "/dev/disk14s1"),
                ("mount-point", "/Volumes/Clew"),
                ("volume-kind", "apfs"),
            ],
            &[("dev-entry", "/dev/disk14")],
        ]);
        assert_eq!(parse_attach_plist(&apfs), volume("/Volumes/Clew"));
        let crafted = attach_plist(&[&[
            ("dev-entry", "/dev/disk5s1"),
            (
                "mount-point",
                "/Volumes/&#x43;lew\t/Volumes/Evil\n/dev/disk1&#33;",
            ),
        ]]);
        assert_eq!(
            parse_attach_plist(&crafted),
            volume("/Volumes/Clew\t/Volumes/Evil\n/dev/disk1!")
        );
        // No volume: nothing mounted, the old text output, a list cut
        // short, a tag that does not close its own, an unknown escape.
        let unmounted = attach_plist(&[&[("dev-entry", "/dev/disk4")]]);
        let unclosed = apfs.replace("</key>", "</string>");
        let unknown = crafted.replace("&#33;", "&bang;");
        for xml in [
            unmounted.as_str(),
            "/dev/disk4s1\tApple_HFS\t/Volumes/Clew\n",
            &HFS_ATTACH_PLIST[..HFS_ATTACH_PLIST.len() / 2],
            unclosed.as_str(),
            unknown.as_str(),
        ] {
            assert_eq!(parse_attach_plist(xml), None, "{xml}");
        }
    }

    /// An `hdiutil info -plist` document listing `images`, each the path it
    /// was attached by and the device nodes of its entities, in order.
    fn info_plist(images: &[(&str, &[&str])]) -> String {
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\
             <key>framework</key>\n<string>683.100.3</string>\n<key>images</key>\n<array>\n",
        );
        for (path, nodes) in images {
            xml.push_str(&format!(
                "<dict>\n<key>autodiskmount</key>\n<false/>\n<key>blockcount</key>\n\
                 <integer>40960</integer>\n<key>image-path</key>\n<string>{path}</string>\n\
                 <key>system-entities</key>\n<array>\n"
            ));
            for node in *nodes {
                xml.push_str(&format!(
                    "<dict>\n<key>content-hint</key>\n<string>Apple_HFS</string>\n\
                     <key>dev-entry</key>\n<string>{node}</string>\n</dict>\n"
                ));
            }
            xml.push_str("</array>\n</dict>\n");
        }
        xml.push_str("</array>\n</dict>\n</plist>\n");
        xml
    }

    /// The update image is found in `hdiutil info` by the file it was
    /// attached from, never by a device node: listed, it is its first whole
    /// disk (the image's own, ahead of the APFS container `hdiutil`
    /// synthesized); not listed, it is detached, whatever disk now has the
    /// number it had. A path is matched once resolved as well.
    #[test]
    fn the_update_image_is_found_in_hdiutil_info_by_its_file() {
        let ours = Path::new("/private/var/clew/updates/dl-1/Clew-9.9.9.dmg");
        let disk = |node: &str| Some(Some(PathBuf::from(node)));
        let apfs: &[&str] = &[
            "/dev/disk12",
            "/dev/disk12s1",
            "/dev/disk14",
            "/dev/disk14s1",
        ];
        let listed = info_plist(&[
            ("/Library/Other.dmg", &["/dev/disk4", "/dev/disk4s1"]),
            (&ours.to_string_lossy(), apfs),
        ]);
        assert_eq!(parse_info_plist(&listed, ours), disk("/dev/disk12"));
        // Ejected by someone else, and its number taken by another image.
        let reused = info_plist(&[("/Users/me/Other.dmg", apfs)]);
        assert_eq!(parse_info_plist(&reused, ours), Some(None));
        // Nothing attached at all, however `hdiutil` says it.
        let none = info_plist(&[]);
        assert_eq!(parse_info_plist(&none, ours), Some(None));
        let empty = none.replace("<array>\n</array>", "<array/>");
        assert_eq!(parse_info_plist(&empty, ours), Some(None));
        let absent = "<plist version=\"1.0\">\n<dict>\n<key>framework</key>\n\
                      <string>683</string>\n</dict>\n</plist>\n";
        assert_eq!(parse_info_plist(absent, ours), Some(None));
        // No whole disk listed: any node of the image detaches it.
        let partial = info_plist(&[(&ours.to_string_lossy(), &["/dev/disk9s1"])]);
        assert_eq!(parse_info_plist(&partial, ours), disk("/dev/disk9s1"));

        // Attached by another spelling of the same file: through a
        // symlinked directory.
        let dir = test_dir("update-info-link");
        std::fs::create_dir_all(dir.join("real")).unwrap();
        let file = dir.join("real/Clew.dmg");
        std::fs::write(&file, b"image").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let file = file.canonicalize().unwrap();
        let spelled = dir.join("link/Clew.dmg");
        let linked = info_plist(&[(&spelled.to_string_lossy(), &["/dev/disk7"])]);
        assert_eq!(parse_info_plist(&linked, &file), disk("/dev/disk7"));
        // An exact entry is looked for in the whole list before any other
        // entry's path is resolved: resolving touches that image's file
        // system, which on a network share can take long. Listed after
        // another spelling of the same file, it is still the one found.
        let both = info_plist(&[
            (&spelled.to_string_lossy(), &["/dev/disk7"]),
            (&file.to_string_lossy(), &["/dev/disk8"]),
        ]);
        assert_eq!(parse_info_plist(&both, &file), disk("/dev/disk8"));
        // And only an entry by the image's own name is resolved at all: the
        // lookup that finds it not listed touches no other image's file
        // system. Another name is another file, even one linked to it.
        std::os::unix::fs::symlink(&file, dir.join("Other.dmg")).unwrap();
        let other = info_plist(&[(&dir.join("Other.dmg").to_string_lossy(), &["/dev/disk9"])]);
        assert_eq!(parse_info_plist(&other, &file), Some(None));

        // Not what `hdiutil info` prints: cut short, `images` not a list,
        // our entry without its entities or without a disk.
        let not_a_list =
            absent.replace("</dict>", "<key>images</key>\n<string>no</string>\n</dict>");
        let entityless = info_plist(&[(&ours.to_string_lossy(), &[])])
            .replace("<key>system-entities</key>\n<array>\n</array>\n", "");
        let diskless = info_plist(&[(&ours.to_string_lossy(), &[])]);
        for xml in [
            &listed[..listed.len() / 2],
            not_a_list.as_str(),
            entityless.as_str(),
            diskless.as_str(),
        ] {
            assert_eq!(parse_info_plist(xml, ours), None, "{xml}");
        }
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
        // Held so `CLEW_DATA_DIR` stays what it is while it is read twice.
        // The shipped default is asked for directly: clearing the variable
        // for the process, as this test did, pointed every test running
        // beside it at the developer's real data directory.
        let _env = crate::app::tests::env_lock();
        let parent = staging_parent().expect("a data root to stage in");
        let root = clew_core::lsp::store::data_root().unwrap();
        assert_eq!(parent, root.join("updates"));
        let shipped = clew_core::lsp::store::default_data_root().expect("a home directory");
        assert!(
            !shipped.starts_with(std::env::temp_dir()),
            "{} is under the temp dir",
            shipped.display()
        );
    }

    /// The staging directory is the shared per-attempt helper's, under the
    /// staging parent — the properties themselves are asserted next to that
    /// helper in `crate::updater`, which the download half uses too. Here only
    /// that this half still asks for one, and does not go back to a name a
    /// second window would share.
    #[test]
    fn staging_asks_the_shared_helper_for_a_private_per_install_directory() {
        let parent = test_dir("stage");

        let first = crate::updater::create_private_dir(&parent, STAGING_PREFIX).unwrap();
        let second = crate::updater::create_private_dir(&parent, STAGING_PREFIX).unwrap();
        assert!(first.starts_with(&parent) && second.starts_with(&parent));
        assert_ne!(second, first, "two installs must not share a directory");
    }

    /// A bundle clew could not replace is refused before anything is
    /// downloaded, with a reason, instead of after clew has quit for the swap.
    #[test]
    fn a_bundle_this_user_cannot_replace_is_refused_up_front() {
        use std::os::unix::fs::PermissionsExt;
        let root = test_dir("replaceable");
        let apps = root.join("Applications");
        let bundle = apps.join("Clew.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        assert!(replaceable(&bundle).is_ok());

        let translocated = root.join("AppTranslocation/0A1B/d/Clew.app");
        std::fs::create_dir_all(&translocated).unwrap();
        let err = replaceable(&translocated).unwrap_err();
        assert!(err.contains("translocated"), "{err}");

        // The parent directory not writable: the helper's first `mv` fails.
        std::fs::set_permissions(&apps, std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = replaceable(&bundle).unwrap_err();
        std::fs::set_permissions(&apps, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.contains("cannot modify"), "{err}");
    }

    /// A private fixture for running the real swap script: an installed app,
    /// a staged update, and a recorder standing in for `open`.
    struct SwapFixture {
        root: PathBuf,
        target: PathBuf,
        staging: PathBuf,
        staged: PathBuf,
        opener: PathBuf,
    }

    impl SwapFixture {
        fn new(name: &str) -> SwapFixture {
            use std::os::unix::fs::PermissionsExt;
            let root = test_dir(&format!("swap-{name}"));
            let target = root.join("Applications/Clew.app");
            std::fs::create_dir_all(target.join("Contents")).unwrap();
            std::fs::write(target.join("Contents/version"), "old").unwrap();
            let staging = root.join("updates/stage-1-0");
            let staged = staging.join("Clew.app");
            std::fs::create_dir_all(staged.join("Contents")).unwrap();
            std::fs::write(staged.join("Contents/version"), "new").unwrap();
            let opener = root.join("opener.sh");
            std::fs::write(
                &opener,
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$(dirname \"$0\")/opened.txt\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
            SwapFixture {
                root,
                target,
                staging,
                staged,
                opener,
            }
        }

        /// Run the script to completion against a pid that has already exited
        /// (or `pid`, when given), returning bash's exit status.
        fn run(&self, pid: Option<u32>, ticks: u32) -> std::process::ExitStatus {
            let pid = pid.unwrap_or_else(|| {
                let mut child = Command::new("/usr/bin/true").spawn().unwrap();
                let id = child.id();
                child.wait().unwrap();
                id
            });
            let reopen = self.root.join("my project");
            let plan = SwapPlan {
                pid,
                staged_app: &self.staged,
                staging: &self.staging,
                target: &self.target,
                reopen: Some(&reopen),
                log: &self.root.join("update.log"),
                failed_marker: &self.root.join("failed"),
                opener: &self.opener,
                wait_ticks: ticks,
            };
            let script = swap_script(&plan);
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(Command::new("/bin/bash").arg("-c").arg(script).status());
            });
            // A hang detector, not a speed bound: the script itself runs in
            // well under a second, but its first run of the fixture's fresh
            // `opener.sh` waits for macOS to assess the new executable, which
            // takes over ten seconds while the rest of the suite is starting
            // fresh scripts of its own.
            rx.recv_timeout(std::time::Duration::from_secs(90))
                .expect("the swap script hung")
                .unwrap()
        }

        fn version(&self) -> Option<String> {
            std::fs::read_to_string(self.target.join("Contents/version")).ok()
        }

        fn opened(&self) -> String {
            std::fs::read_to_string(self.root.join("opened.txt")).unwrap_or_default()
        }

        fn failure(&self) -> Option<String> {
            std::fs::read_to_string(self.root.join("failed")).ok()
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.root.join("update.log")).unwrap_or_default()
        }

        fn backups(&self) -> usize {
            std::fs::read_dir(self.target.parent().unwrap())
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
                .count()
        }
    }

    impl Drop for SwapFixture {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                self.target.parent().unwrap(),
                std::fs::Permissions::from_mode(0o755),
            );
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The happy path, executed for real: the new bundle replaces the old, the
    /// backup and the staging directory are gone, and the NEW app is opened
    /// with the project to reopen.
    #[test]
    fn the_swap_script_installs_the_staged_copy_and_relaunches_it() {
        let fx = SwapFixture::new("ok");
        assert!(fx.run(None, 50).success());
        assert_eq!(fx.version().as_deref(), Some("new"));
        assert!(!fx.staging.exists(), "the staging directory must be pruned");
        assert_eq!(fx.backups(), 0, "the backup must be removed");
        assert_eq!(fx.failure(), None);
        let opened = fx.opened();
        assert!(opened.contains(&fx.target.to_string_lossy().to_string()));
        assert!(
            opened.contains("--args") && opened.contains("my project"),
            "{opened}"
        );
        let log = std::fs::read_to_string(fx.root.join("update.log")).unwrap();
        assert!(log.contains("installed"), "{log}");
    }

    /// Every path reaches the script single-quoted. An install, a staging
    /// directory and a project whose names hold a single quote, a double
    /// quote, `$`, backticks and spaces still swap — nothing in a name is
    /// expanded or run — and the project is reopened by its exact name.
    #[test]
    fn paths_with_quotes_and_shell_characters_swap_as_named() {
        let fx = SwapFixture::new("it's \"odd\" $HOME `id` & ;");
        assert!(fx.run(None, 50).success(), "{}", fx.log());
        assert_eq!(fx.version().as_deref(), Some("new"));
        assert!(!fx.staging.exists(), "the staging directory must be pruned");
        assert_eq!(fx.backups(), 0, "the backup must be removed");
        assert_eq!(fx.failure(), None);
        assert_eq!(
            fx.opened(),
            format!(
                "{}\n--args\n{}\n",
                fx.target.display(),
                fx.root.join("my project").display()
            ),
            "the opener must get each path as one argument, verbatim"
        );
    }

    /// The copy fails after the old app was moved aside: the old app must come
    /// back, be RELAUNCHED (clew already quit for this), and the reason must be
    /// left for it to report. Before, the user was left with no clew running.
    #[test]
    fn a_failed_copy_restores_and_relaunches_the_previous_version() {
        let fx = SwapFixture::new("copy");
        std::fs::remove_dir_all(&fx.staged).unwrap(); // ditto has nothing to copy
        assert!(!fx.run(None, 50).success());
        assert_eq!(fx.version().as_deref(), Some("old"));
        assert_eq!(fx.backups(), 0);
        assert!(!fx.staging.exists());
        assert!(
            fx.opened()
                .contains(&fx.target.to_string_lossy().to_string())
        );
        assert!(
            fx.failure()
                .is_some_and(|f| f.contains("previous version was restored")),
            "{:?}",
            fx.failure()
        );
        // The log says why: ditto's own complaint about the missing source,
        // not just that the copy failed.
        let log = fx.log();
        assert!(
            log.lines()
                .any(|l| l.starts_with("ditto: ") && l.contains(&*fx.staged.to_string_lossy())),
            "{log}"
        );
    }

    /// The installed app cannot even be moved aside: nothing changes, the old
    /// app is relaunched and the reason is recorded.
    #[test]
    fn an_unmovable_install_is_left_alone_and_relaunched() {
        use std::os::unix::fs::PermissionsExt;
        let fx = SwapFixture::new("mv");
        std::fs::set_permissions(
            fx.target.parent().unwrap(),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        assert!(!fx.run(None, 50).success());
        assert_eq!(fx.version().as_deref(), Some("old"));
        assert!(!fx.staging.exists());
        assert!(
            fx.opened()
                .contains(&fx.target.to_string_lossy().to_string())
        );
        assert!(
            fx.failure()
                .is_some_and(|f| f.contains("nothing was changed"))
        );
        let log = fx.log();
        assert!(
            log.contains("mv") && log.contains("Permission denied"),
            "mv's own error reaches the log: {log}"
        );
    }

    /// A clew that never exits is never swapped out from under itself, and no
    /// second copy is opened next to it.
    #[test]
    fn a_clew_that_does_not_quit_is_not_swapped() {
        let fx = SwapFixture::new("alive");
        assert!(!fx.run(Some(std::process::id()), 3).success());
        assert_eq!(fx.version().as_deref(), Some("old"));
        assert_eq!(
            fx.opened(),
            "",
            "nothing may be opened next to a running clew"
        );
        assert!(!fx.staging.exists());
        assert!(fx.failure().is_some_and(|f| f.contains("did not quit")));
    }
}
