//! The plugins the user installed: what they approved of each, and the
//! copy of each file Clew runs.
//!
//! `<data>/plugins.toml` records, per plugin id, the fingerprint of the file
//! the user approved, the permissions they granted, whether it is on, where
//! it came from, and its manifest (so Clew lists a plugin's commands at
//! startup without reading any plugin). It is a file of its own, not a table
//! of `trust.toml`: an older Clew rewriting `trust.toml` would drop a table
//! it does not know.
//!
//! The bytes the user approved are copied to
//! `<data>/plugins/installed/<fingerprint>.wasm`, read-only, and Clew runs
//! that copy and nothing else, checking its digest each time it reads it.
//! A rebuild after the approval cannot change what runs: a new build is a
//! new fingerprint, and a new question.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::inspect::{Inspected, MAX_PLUGIN_BYTES, fingerprint, hex};
use crate::manifest::{Manifest, permission_bits};

/// `<data>/plugins.toml`.
pub fn store_path(data: &Path) -> PathBuf {
    data.join("plugins.toml")
}

/// `<data>/plugins`: everything else of the plugins'.
pub fn plugins_dir(data: &Path) -> PathBuf {
    data.join("plugins")
}

/// `id`, if it is a plugin's id (`clew_rules::plugin::id_ok`): what every
/// folder of a plugin's is named by goes through this first, so a name an
/// agent passes can only ever be one plain name under Clew's own folder.
pub fn checked_id(id: &str) -> Result<&str, String> {
    if clew_rules::plugin::id_ok(id.as_bytes(), 3) {
        Ok(id)
    } else {
        Err(format!(
            "\"{}\" is not a plugin's id: an id is 3 to 40 of a-z, 0-9 and -, starting with a letter",
            id.chars().take(60).collect::<String>()
        ))
    }
}

/// Where the approved copies are.
pub fn installed_dir(data: &Path) -> PathBuf {
    plugins_dir(data).join("installed")
}

/// The approved copy of the file whose fingerprint is `fingerprint`.
pub(crate) fn installed_path(data: &Path, fingerprint: &str) -> PathBuf {
    installed_dir(data).join(format!("{fingerprint}.wasm"))
}

/// Where a plugin came from, as the user is told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Made by Clew's Ask agent.
    Ask,
    /// Made by an agent over ACP, by its name.
    Agent(String),
    /// A file the user picked, by its path.
    File(String),
}

/// What the user approved of one plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The approved file's fingerprint (`inspect::fingerprint`).
    pub fingerprint: String,
    /// SHA-256 of the approved file, hex: what its copy is checked against.
    pub sha256: String,
    /// The permissions granted, by name.
    pub permissions: Vec<String>,
    pub enabled: bool,
    /// When it was approved, in seconds since 1970.
    pub installed: u64,
    pub origin: Origin,
    /// Its `plugin.toml`, as its file carries it.
    pub manifest: String,
    /// Fields a later Clew wrote, kept as they are when this one rewrites
    /// the record.
    #[serde(flatten)]
    pub later: toml::Table,
}

impl Record {
    /// Its manifest, read. `None` for one this Clew cannot read (written by
    /// a later one): the plugin is left alone, not run.
    pub fn manifest(&self) -> Option<Manifest> {
        Manifest::parse(&self.manifest).ok()
    }

    /// The permissions granted, as bits.
    pub fn granted(&self) -> u8 {
        permission_bits(&self.permissions)
    }
}

/// Every plugin installed, by id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub plugin: BTreeMap<String, Record>,
    #[serde(flatten)]
    pub later: toml::Table,
}

/// One writer in this process at a time; the file lock extends it to other
/// Clew processes.
static SAVE_LOCK: Mutex<()> = Mutex::new(());

impl Store {
    /// The store, or an empty one when there is none or it cannot be read.
    /// For reading what is installed; never for writing back.
    pub fn load(data: &Path) -> Store {
        Store::load_checked(data).unwrap_or_default()
    }

    /// The store; an error when it exists but cannot be read or parsed, so
    /// that it is never overwritten with less than it holds.
    pub fn load_checked(data: &Path) -> Result<Store, String> {
        let path = store_path(data);
        match clew_core::statefile::read_checked(&path) {
            Ok(None) => Ok(Store::default()),
            Ok(Some(text)) => {
                toml::from_str(&text).map_err(|e| format!("{}: {}", path.display(), e.message()))
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Change the store under its lock, read afresh, and write it back
    /// atomically: two Clew windows, or two Clew processes, add to it
    /// rather than overwrite each other.
    pub fn update<T>(
        data: &Path,
        change: impl FnOnce(&mut Store) -> Result<T, String>,
    ) -> Result<T, String> {
        Self::locked(data, change, false)
    }

    /// [`Store::update`], then — still under the lock — the copies no
    /// record names any more removed. Swept outside it, an install between
    /// another's copy and its record lost its copy to the sweep.
    fn update_and_sweep<T>(
        data: &Path,
        change: impl FnOnce(&mut Store) -> Result<T, String>,
    ) -> Result<T, String> {
        Self::locked(data, change, true)
    }

    fn locked<T>(
        data: &Path,
        change: impl FnOnce(&mut Store) -> Result<T, String>,
        sweep: bool,
    ) -> Result<T, String> {
        let _serialized = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clew_core::derived::ensure_private_dir(data).map_err(|e| e.to_string())?;
        let path = store_path(data);
        let _exclusive = clew_core::statefile::lock_named(&path, "plugins.toml.lock")
            .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
        let mut store = Store::load_checked(data)?;
        let out = change(&mut store)?;
        let text = toml::to_string(&store).map_err(|e| e.to_string())?;
        clew_core::statefile::write_atomic(&path, text.as_bytes()).map_err(|e| e.to_string())?;
        if sweep {
            sweep_unused(data, &store);
        }
        Ok(out)
    }

    /// The plugins that are on, with their manifests, by id.
    pub fn enabled(&self) -> impl Iterator<Item = (&str, &Record, Manifest)> {
        self.plugin.iter().filter_map(|(id, record)| {
            (record.enabled)
                .then(|| record.manifest())
                .flatten()
                .map(|m| (id.as_str(), record, m))
        })
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Install `bytes` — the file the user just approved, as [`crate::inspect()`]
/// read it — granting what its code needs: copy it where Clew runs it from,
/// and record the approval. Replaces an earlier version of the same id.
pub fn install(
    data: &Path,
    bytes: &[u8],
    inspected: &Inspected,
    origin: Origin,
) -> Result<Record, String> {
    let sha256: [u8; 32] = Sha256::digest(bytes).into();
    if sha256 != inspected.sha256 {
        return Err("the plugin's file changed after it was read".into());
    }
    let fingerprint = fingerprint(&sha256);
    let record = Record {
        fingerprint,
        sha256: hex(&sha256),
        permissions: inspected.permissions(),
        enabled: true,
        installed: now(),
        origin,
        manifest: inspected.manifest.to_toml(),
        later: toml::Table::new(),
    };
    let id = inspected.manifest.plugin.id.clone();
    let kept = record.clone();
    // The copy, its record and the sweep under one lock: no other install
    // or remove sweeps between the copy written and its record.
    Store::update_and_sweep(data, move |store| {
        write_copy(data, &kept.fingerprint, bytes)?;
        store.plugin.insert(id, kept);
        Ok(())
    })?;
    Ok(record)
}

/// Write the approved copy, read-only, unless it is there already with the
/// same bytes (the same fingerprint is the same file).
fn write_copy(data: &Path, fingerprint: &str, bytes: &[u8]) -> Result<(), String> {
    let dir = installed_dir(data);
    clew_core::derived::ensure_private_dir(&plugins_dir(data)).map_err(|e| e.to_string())?;
    clew_core::derived::ensure_private_dir(&dir).map_err(|e| e.to_string())?;
    let path = installed_path(data, fingerprint);
    if let Ok(existing) = read_capped(&path) {
        if Sha256::digest(&existing)[..] == Sha256::digest(bytes)[..] {
            return Ok(());
        }
        // A copy that is not what its name says: replace it.
        make_writable(&path);
    }
    clew_core::statefile::write_atomic(&path, bytes)
        .map_err(|e| format!("could not keep the plugin: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400));
    }
    #[cfg(not(unix))]
    {
        if let Ok(meta) = std::fs::metadata(&path) {
            let mut permissions = meta.permissions();
            permissions.set_readonly(true);
            let _ = std::fs::set_permissions(&path, permissions);
        }
    }
    Ok(())
}

/// Let `path` be replaced or removed (a read-only file cannot be on
/// Windows).
fn make_writable(path: &Path) {
    #[cfg(not(unix))]
    if let Ok(meta) = std::fs::metadata(path) {
        let mut permissions = meta.permissions();
        #[expect(clippy::permissions_set_readonly_false, reason = "Windows only")]
        permissions.set_readonly(false);
        let _ = std::fs::set_permissions(path, permissions);
    }
    #[cfg(unix)]
    let _ = path;
}

/// `path`'s bytes: a plain file (no symlink, no FIFO), [`MAX_PLUGIN_BYTES`]
/// at most.
fn read_capped(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = clew_core::statefile::open_plain_checked(path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "it is not there".to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_PLUGIN_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_PLUGIN_BYTES {
        return Err("it is larger than a plugin may be".into());
    }
    Ok(bytes)
}

/// Why a plugin's approved copy is not run: it is not the file approved
/// (changed on disk since). Said to the user, who can do what it says:
/// installed again, the copy is written anew. Removed first, as this said
/// to, the plugin's saved data went with it, for nothing.
pub const CHANGED: &str = "Clew's copy of it changed after you approved it. Install it again from its file, or ask the agent that made it to install it again.";

/// The approved copy of `record`'s plugin, checked to be the file approved.
pub fn read_approved(data: &Path, record: &Record) -> Result<Vec<u8>, String> {
    let path = installed_path(data, &record.fingerprint);
    let bytes = read_capped(&path).map_err(|e| format!("the plugin's file: {e}"))?;
    let sha256: [u8; 32] = Sha256::digest(&bytes).into();
    if hex(&sha256) != record.sha256 || fingerprint(&sha256) != record.fingerprint {
        return Err(CHANGED.into());
    }
    Ok(bytes)
}

/// Turn the plugin `id` on or off.
pub fn set_enabled(data: &Path, id: &str, on: bool) -> Result<(), String> {
    Store::update(data, |store| {
        let record = store
            .plugin
            .get_mut(id)
            .ok_or_else(|| format!("no plugin \"{id}\" is installed"))?;
        record.enabled = on;
        Ok(())
    })
}

/// Remove the plugin `id`, and its file.
pub fn remove(data: &Path, id: &str) -> Result<(), String> {
    Store::update_and_sweep(data, |store| {
        store
            .plugin
            .remove(id)
            .map(drop)
            .ok_or_else(|| format!("no plugin \"{id}\" is installed"))
    })
}

/// Remove the copies no record names any more. Best effort: a copy left
/// behind is never run.
pub fn sweep(data: &Path) {
    // Under the store's lock, as installs and removes sweep.
    let _ = Store::update_and_sweep(data, |_| Ok(()));
}

/// Remove the copies `store` names none of. Called under the store's lock.
fn sweep_unused(data: &Path, store: &Store) {
    let keep: Vec<String> = store
        .plugin
        .values()
        .map(|r| format!("{}.wasm", r.fingerprint))
        .collect();
    let Ok(entries) = std::fs::read_dir(installed_dir(data)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".wasm") && !keep.contains(&name) {
            make_writable(&entry.path());
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests_support;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("clew-plugin-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An `Inspected` for `bytes`, as if read from a plugin file.
    fn inspected(bytes: &[u8], needs: u8) -> Inspected {
        Inspected {
            manifest: tests_support::manifest(),
            imports: Vec::new(),
            exports: Vec::new(),
            needs,
            sha256: Sha256::digest(bytes).into(),
        }
    }

    #[test]
    fn an_installed_plugin_is_recorded_and_its_copy_read_back() {
        let data = temp("install");
        let bytes = b"plugin bytes, version one";
        let record = install(
            &data,
            bytes,
            &inspected(bytes, clew_rules::plugin::READ),
            Origin::Ask,
        )
        .unwrap();
        assert_eq!(record.permissions, ["read"]);
        assert_eq!(record.granted(), clew_rules::plugin::READ);
        let store = Store::load_checked(&data).unwrap();
        assert_eq!(store.plugin["sample"], record);
        assert_eq!(store.enabled().count(), 1);
        assert_eq!(read_approved(&data, &record).unwrap(), bytes);

        // A new version replaces the old, whose copy goes.
        let two = b"plugin bytes, version two";
        let newer = install(&data, two, &inspected(two, 0), Origin::File("x".into())).unwrap();
        assert!(!installed_path(&data, &record.fingerprint).exists());
        assert_eq!(read_approved(&data, &newer).unwrap(), two);
        assert_eq!(Store::load(&data).plugin.len(), 1);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_copy_changed_after_approval_is_refused() {
        let data = temp("tamper");
        let bytes = b"approved";
        let record = install(&data, bytes, &inspected(bytes, 0), Origin::Ask).unwrap();
        let path = installed_path(&data, &record.fingerprint);
        make_writable(&path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::write(&path, b"changed").unwrap();
        assert!(
            read_approved(&data, &record)
                .unwrap_err()
                .contains("changed after you approved it")
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn bytes_that_are_not_what_was_inspected_are_not_installed() {
        let data = temp("mismatch");
        let err = install(&data, b"other", &inspected(b"inspected", 0), Origin::Ask).unwrap_err();
        assert!(err.contains("changed after it was read"));
        assert!(Store::load(&data).plugin.is_empty());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_plugin_is_turned_off_and_removed() {
        let data = temp("toggle");
        let bytes = b"toggle me";
        let record = install(&data, bytes, &inspected(bytes, 0), Origin::Ask).unwrap();
        set_enabled(&data, "sample", false).unwrap();
        let store = Store::load(&data);
        assert!(!store.plugin["sample"].enabled);
        assert_eq!(store.enabled().count(), 0);
        remove(&data, "sample").unwrap();
        assert!(Store::load(&data).plugin.is_empty());
        assert!(!installed_path(&data, &record.fingerprint).exists());
        assert!(remove(&data, "sample").unwrap_err().contains("no plugin"));
        assert!(set_enabled(&data, "nope", true).is_err());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn an_unreadable_store_is_never_overwritten() {
        let data = temp("unreadable");
        std::fs::write(store_path(&data), "[plugin.x\n").unwrap();
        let bytes = b"anything";
        assert!(install(&data, bytes, &inspected(bytes, 0), Origin::Ask).is_err());
        assert_eq!(
            std::fs::read_to_string(store_path(&data)).unwrap(),
            "[plugin.x\n"
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn what_a_later_clew_wrote_is_kept() {
        let data = temp("later");
        let bytes = b"kept";
        install(&data, bytes, &inspected(bytes, 0), Origin::Ask).unwrap();
        let text = std::fs::read_to_string(store_path(&data)).unwrap();
        let text = format!("future = 1\n{}\nsignature = \"abc\"\n", text);
        std::fs::write(store_path(&data), text).unwrap();
        set_enabled(&data, "sample", false).unwrap();
        let store = Store::load_checked(&data).unwrap();
        assert_eq!(
            store.later.get("future").and_then(|v| v.as_integer()),
            Some(1)
        );
        assert_eq!(
            store.plugin["sample"]
                .later
                .get("signature")
                .and_then(|v| v.as_str()),
            Some("abc")
        );
        let _ = std::fs::remove_dir_all(&data);
    }
}
