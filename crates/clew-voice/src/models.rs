//! The speech models Clew can listen with, and where they are kept.
//!
//! Each is one of whisper.cpp's own ggml files, fetched from where
//! whisper.cpp publishes them. The three Echo offers — its Small, Medium
//! and Large: `small-q5_1`, `large-v3-turbo-q5_0`, `large-v3-q5_0` — come
//! with the SHA-256 and the exact size Echo checks them against; the
//! English-only ones for slower machines with the SHA-1 whisper.cpp lists
//! in its `models/README.md`. A download is accepted only if its bytes hash
//! to that, and are as many as that ([`Install::finish`]).
//!
//! Fetching is the caller's — Clew's own transport, with its proxy and
//! trust — writing into an [`Install`]: this crate decides what a model is,
//! where it lives and whether what arrived is it, and keeps no network code
//! of its own.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha1::Sha1;
use sha2::{Digest, Sha256};

/// What a model's bytes must hash to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checksum {
    /// SHA-256, lowercase hex.
    Sha256(&'static str),
    /// SHA-1, lowercase hex — as whisper.cpp's model list gives them.
    Sha1(&'static str),
}

impl Checksum {
    /// The expected digest, lowercase hex.
    pub fn hex(&self) -> &'static str {
        match self {
            Checksum::Sha256(h) | Checksum::Sha1(h) => h,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Checksum::Sha256(_) => "SHA-256",
            Checksum::Sha1(_) => "SHA-1",
        }
    }
}

/// One whisper model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    /// Its name in whisper.cpp's list (`small-q5_1`), and the id Clew's
    /// settings store.
    pub id: &'static str,
    /// What the settings call it.
    pub name: &'static str,
    /// Its download size, in MB: shown before anything is fetched.
    pub size_mb: u32,
    /// Its exact size in bytes, where it is published.
    pub bytes: Option<u64>,
    /// What its bytes hash to.
    pub checksum: Checksum,
    /// Understands English only — and English better, size for size.
    pub english_only: bool,
    /// One line on when to pick it.
    pub about: &'static str,
}

impl Model {
    /// The file's name, in the store and upstream: `ggml-small-q5_1.bin`.
    pub fn file_name(&self) -> String {
        format!("ggml-{}.bin", self.id)
    }

    /// Where whisper.cpp publishes it.
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            self.file_name()
        )
    }

    /// Largest download accepted for it: its exact size where that is
    /// known, else its size and a margin — so a server that never stops
    /// sending is cut off.
    pub fn max_bytes(&self) -> u64 {
        self.bytes
            .unwrap_or(u64::from(self.size_mb) * 1_000_000 * 5 / 4 + 16_000_000)
    }
}

/// Every model Clew offers, smallest first.
pub const MODELS: &[Model] = &[
    Model {
        id: "tiny.en",
        name: "Tiny (English)",
        size_mb: 78,
        bytes: None,
        checksum: Checksum::Sha1("c78c86eb1a8faa21b369bcd33207cc90d64ae9df"),
        english_only: true,
        about: "Fastest; fine for short commands on a slow machine.",
    },
    Model {
        id: "base.en",
        name: "Base (English)",
        size_mb: 148,
        bytes: None,
        checksum: Checksum::Sha1("137c40403d78fd54d454da0f9bd998f78703390c"),
        english_only: true,
        about: "Quick and accurate for commands and short questions in English.",
    },
    Model {
        id: "small-q5_1",
        name: "Small",
        size_mb: 190,
        bytes: Some(190_085_487),
        checksum: Checksum::Sha256(
            "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb",
        ),
        english_only: false,
        about: "Fastest multilingual model, for lower-memory computers.",
    },
    Model {
        id: "large-v3-turbo-q5_0",
        name: "Medium (Large v3 Turbo)",
        size_mb: 574,
        bytes: Some(574_041_195),
        checksum: Checksum::Sha256(
            "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
        ),
        english_only: false,
        about: "Best balance of accuracy and speed, in any language.",
    },
    Model {
        id: "large-v3-q5_0",
        name: "Large (Large v3)",
        size_mb: 1081,
        bytes: Some(1_081_140_203),
        checksum: Checksum::Sha256(
            "d75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1",
        ),
        english_only: false,
        about: "Highest accuracy, for powerful computers.",
    },
];

/// The model offered first: Echo's default, small enough to fetch in a
/// moment, in any language.
pub const DEFAULT_MODEL: &str = "small-q5_1";

/// The model `id` names.
pub fn find(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

/// The folder models are kept in.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

/// What a download in progress is written to, beside the model's own name.
const PART: &str = ".part";

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where `model` is, once installed.
    pub fn path(&self, model: &Model) -> PathBuf {
        self.dir.join(model.file_name())
    }

    /// Whether `model` is installed. Only a verified download is ever
    /// renamed to the model's name, so its presence is enough.
    pub fn installed(&self, model: &Model) -> bool {
        fs::symlink_metadata(self.path(model)).is_ok_and(|m| m.is_file())
    }

    /// Start installing `model`: a fresh partial file, written through the
    /// returned [`Install`] and checked by its [`Install::finish`].
    pub fn begin(&self, model: &'static Model) -> io::Result<Install> {
        Install::begin(
            self.path(model),
            Expected {
                name: model.file_name(),
                bytes: model.bytes,
                max_bytes: model.max_bytes(),
                checksum: model.checksum,
            },
        )
    }

    /// Remove `model` (and any download of it left part-way).
    pub fn remove(&self, model: &Model) -> io::Result<()> {
        for path in [
            self.path(model),
            self.dir.join(format!("{}{PART}", model.file_name())),
        ] {
            match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }
}

/// What a download must turn out to be: its name (for what is said of
/// it), its size where that is published, the most it may be, and what it
/// hashes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    pub name: String,
    pub bytes: Option<u64>,
    pub max_bytes: u64,
    pub checksum: Checksum,
}

/// A model being downloaded: bytes written to it are hashed on their way
/// to a partial file, which [`Install::finish`] renames into place only if
/// the hash is the one listed. Dropped unfinished — cancelled, failed — it
/// deletes what it wrote.
pub struct Install {
    expected: Expected,
    file: Option<File>,
    part: PathBuf,
    dest: PathBuf,
    hasher: Hasher,
    written: u64,
}

/// The digest a download is hashed with as it arrives.
enum Hasher {
    Sha256(Sha256),
    Sha1(Sha1),
}

impl Hasher {
    fn of(checksum: &Checksum) -> Self {
        match checksum {
            Checksum::Sha256(_) => Hasher::Sha256(Sha256::new()),
            Checksum::Sha1(_) => Hasher::Sha1(Sha1::new()),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Sha256(h) => h.update(bytes),
            Hasher::Sha1(h) => h.update(bytes),
        }
    }

    fn finish(self) -> String {
        match self {
            Hasher::Sha256(h) => hex(&h.finalize()),
            Hasher::Sha1(h) => hex(&h.finalize()),
        }
    }
}

impl Install {
    /// Start a download that, once it is `expected`, becomes `dest`: a
    /// fresh partial file beside it.
    pub fn begin(dest: PathBuf, expected: Expected) -> io::Result<Install> {
        let dir = dest
            .parent()
            .ok_or_else(|| io::Error::other("a download needs a folder"))?;
        fs::create_dir_all(dir)?;
        let name = dest
            .file_name()
            .ok_or_else(|| io::Error::other("a download needs a name"))?;
        let part = dir.join(format!("{}{PART}", name.to_string_lossy()));
        // Whatever an earlier attempt left — or a link planted in its place
        // — is removed (a link itself, not what it points at), and the file
        // is then created new, so the bytes cannot be steered elsewhere.
        match fs::remove_file(&part) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)?;
        Ok(Install {
            hasher: Hasher::of(&expected.checksum),
            expected,
            file: Some(file),
            part,
            dest,
            written: 0,
        })
    }

    /// Bytes written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Check what was written against the model's checksum and, if it is
    /// the model, put it in place. Returns where it is.
    pub fn finish(mut self) -> Result<PathBuf, String> {
        let mut file = self.file.take().ok_or("the download was already closed")?;
        file.flush()
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("could not save the model: {e}"))?;
        drop(file);
        let expected = self.expected.checksum;
        if let Some(bytes) = self.expected.bytes.filter(|b| *b != self.written) {
            return Err(format!(
                "the download is not {} as published ({} bytes, not {bytes}) — it was \
                 discarded",
                self.expected.name, self.written
            ));
        }
        let hasher = std::mem::replace(&mut self.hasher, Hasher::of(&expected));
        let digest = hasher.finish();
        if digest != expected.hex() {
            return Err(format!(
                "the download is not {} as published (its {} is {digest}, not {}) — it \
                 was discarded",
                self.expected.name,
                expected.name(),
                expected.hex()
            ));
        }
        fs::rename(&self.part, &self.dest).map_err(|e| format!("could not save the model: {e}"))?;
        Ok(self.dest.clone())
    }
}

impl Write for Install {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > self.expected.max_bytes {
            return Err(io::Error::other(format!(
                "the download is larger than {} can be",
                self.expected.name
            )));
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("the download was closed"))?;
        let n = file.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        // Closed first: Windows cannot delete an open file.
        self.file.take();
        let _ = fs::remove_file(&self.part);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model whose SHA-256 is that of `bytes`, and its size theirs, for
    /// a test's install.
    fn model_of(bytes: &[u8]) -> &'static Model {
        let sha: &'static str = Box::leak(hex(&Sha256::digest(bytes)).into_boxed_str());
        Box::leak(Box::new(Model {
            id: "test",
            name: "Test",
            size_mb: 1,
            bytes: Some(bytes.len() as u64),
            checksum: Checksum::Sha256(sha),
            english_only: true,
            about: "",
        }))
    }

    /// The same, by SHA-1 and with no size published.
    fn sha1_model_of(bytes: &[u8]) -> &'static Model {
        let sha: &'static str = Box::leak(hex(&Sha1::digest(bytes)).into_boxed_str());
        Box::leak(Box::new(Model {
            id: "test1",
            name: "Test",
            size_mb: 1,
            bytes: None,
            checksum: Checksum::Sha1(sha),
            english_only: true,
            about: "",
        }))
    }

    fn temp_store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("clew-voice-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Store::new(dir)
    }

    /// Echo's three models, as Echo checks them, and whisper.cpp's small
    /// English ones by the checksums its model list gives.
    #[test]
    fn the_catalog_is_echos_and_whisper_cpps() {
        assert!(find(DEFAULT_MODEL).is_some());
        let mut ids = std::collections::HashSet::new();
        for m in MODELS {
            assert!(ids.insert(m.id), "{} twice", m.id);
            let (len, hex) = match m.checksum {
                Checksum::Sha256(h) => (64, h),
                Checksum::Sha1(h) => (40, h),
            };
            assert_eq!(hex.len(), len, "{}", m.id);
            assert!(
                hex.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
            assert_eq!(m.english_only, m.id.ends_with(".en"), "{}", m.id);
            match m.bytes {
                // Its size in MB is its size in bytes, rounded.
                Some(bytes) => {
                    assert_eq!(
                        u64::from(m.size_mb),
                        (bytes + 500_000) / 1_000_000,
                        "{}",
                        m.id
                    );
                    assert_eq!(m.max_bytes(), bytes);
                }
                None => assert!(m.max_bytes() > u64::from(m.size_mb) * 1_000_000),
            }
        }
        for (id, file) in [
            ("small-q5_1", "ggml-small-q5_1.bin"),
            ("large-v3-turbo-q5_0", "ggml-large-v3-turbo-q5_0.bin"),
            ("large-v3-q5_0", "ggml-large-v3-q5_0.bin"),
        ] {
            let m = find(id).unwrap();
            assert_eq!(m.file_name(), file);
            assert!(matches!(m.checksum, Checksum::Sha256(_)), "{id}");
            assert!(m.bytes.is_some(), "{id}");
        }
        assert_eq!(DEFAULT_MODEL, "small-q5_1");
        assert_eq!(
            find("base.en").unwrap().url(),
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin"
        );
        // Smallest first.
        assert!(MODELS.windows(2).all(|w| w[0].size_mb <= w[1].size_mb));
    }

    /// A model checked by SHA-1 (whisper.cpp's list) installs alike.
    #[test]
    fn a_model_checked_by_sha1_installs() {
        let store = temp_store("sha1");
        let bytes = b"a smaller model".repeat(100);
        let model = sha1_model_of(&bytes);
        let mut install = store.begin(model).unwrap();
        install.write_all(&bytes).unwrap();
        install.finish().unwrap();
        assert!(store.installed(model));
        let mut install = store.begin(model).unwrap();
        install.write_all(b"other bytes").unwrap();
        let err = install.finish().unwrap_err();
        assert!(err.contains("its SHA-1 is"), "{err}");
        let _ = fs::remove_dir_all(store.dir());
    }

    /// A download of the published size is still refused when one byte
    /// differs; one of another size, before its hash is even looked at.
    #[test]
    fn size_and_hash_must_both_match() {
        let store = temp_store("size");
        let model = model_of(b"0123456789");
        let mut install = store.begin(model).unwrap();
        install.write_all(b"0123456780").unwrap();
        let err = install.finish().unwrap_err();
        assert!(err.contains("its SHA-256 is"), "{err}");
        let mut install = store.begin(model).unwrap();
        install.write_all(b"012345678").unwrap();
        let err = install.finish().unwrap_err();
        assert!(err.contains("9 bytes, not 10"), "{err}");
        // One byte more than published is cut off as it arrives.
        let mut install = store.begin(model).unwrap();
        assert!(install.write_all(b"0123456789X").is_err());
        drop(install);
        assert_eq!(fs::read_dir(store.dir()).unwrap().count(), 0);
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn a_verified_download_is_installed() {
        let store = temp_store("ok");
        let bytes = b"ggml model bytes".repeat(1000);
        let model = model_of(&bytes);
        assert!(!store.installed(model));
        let mut install = store.begin(model).unwrap();
        for chunk in bytes.chunks(777) {
            install.write_all(chunk).unwrap();
        }
        assert_eq!(install.written(), bytes.len() as u64);
        let path = install.finish().unwrap();
        assert_eq!(path, store.path(model));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(store.installed(model));
        let left: Vec<_> = fs::read_dir(store.dir()).unwrap().collect();
        assert_eq!(left.len(), 1, "no partial file is left");
        store.remove(model).unwrap();
        assert!(!store.installed(model));
        // Removing what is not there is not an error.
        store.remove(model).unwrap();
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn a_download_that_is_not_the_model_is_discarded() {
        let store = temp_store("bad");
        let model = model_of(b"the real model");
        let mut install = store.begin(model).unwrap();
        install.write_all(b"something else").unwrap();
        let err = install.finish().unwrap_err();
        assert!(err.contains("not ggml-test.bin as published"), "{err}");
        assert!(!store.installed(model));
        assert_eq!(fs::read_dir(store.dir()).unwrap().count(), 0);
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn an_abandoned_download_leaves_nothing() {
        let store = temp_store("drop");
        let model = model_of(b"whole");
        let mut install = store.begin(model).unwrap();
        install.write_all(b"wh").unwrap();
        drop(install);
        assert_eq!(fs::read_dir(store.dir()).unwrap().count(), 0);
        // Nor does an earlier attempt's partial file stop a new one.
        fs::write(store.dir().join("ggml-test.bin.part"), b"stale").unwrap();
        let mut install = store.begin(model).unwrap();
        install.write_all(b"whole").unwrap();
        install.finish().unwrap();
        assert_eq!(fs::read(store.path(model)).unwrap(), b"whole");
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn a_download_larger_than_the_model_is_cut_off() {
        let store = temp_store("big");
        let model = model_of(b"x");
        let mut install = store.begin(model).unwrap();
        let chunk = vec![0u8; 1 << 20];
        let mut wrote = 0u64;
        let err = loop {
            match install.write_all(&chunk) {
                Ok(()) => wrote += chunk.len() as u64,
                Err(e) => break e,
            }
        };
        assert!(wrote <= model.max_bytes(), "{wrote}");
        assert!(err.to_string().contains("larger than"), "{err}");
        drop(install);
        let _ = fs::remove_dir_all(store.dir());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_planted_as_the_partial_file_is_not_followed() {
        let store = temp_store("link");
        fs::create_dir_all(store.dir()).unwrap();
        let target = store.dir().join("elsewhere");
        fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, store.dir().join("ggml-test.bin.part")).unwrap();
        let model = model_of(b"x");
        let mut install = store.begin(model).unwrap();
        install.write_all(b"x").unwrap();
        install.finish().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        let _ = fs::remove_dir_all(store.dir());
    }
}
