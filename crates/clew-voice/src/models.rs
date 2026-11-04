//! The speech models Clew can listen with, and where they are kept.
//!
//! Each is one of whisper.cpp's own ggml files, as published in its
//! repository's model list with a SHA-1 for every file: the catalog below
//! ([`MODELS`]) is that list's checksums, and a download is accepted only if
//! its bytes hash to the one listed ([`Install::finish`]). The choice runs
//! from the fastest English-only model to Echo's most accurate one.
//!
//! Fetching is the caller's — Clew's own transport, with its proxy and
//! trust — writing into an [`Install`]: this crate decides what a model is,
//! where it lives and whether what arrived is it, and keeps no network code
//! of its own.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

/// One whisper model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    /// Its name in whisper.cpp's list (`base.en`), and the id Clew's
    /// settings store.
    pub id: &'static str,
    /// What the settings call it.
    pub name: &'static str,
    /// Its download size, in MB: shown before anything is fetched.
    pub size_mb: u32,
    /// The SHA-1 whisper.cpp lists for the file, lowercase hex.
    pub sha1: &'static str,
    /// Understands English only — and English better, size for size.
    pub english_only: bool,
    /// One line on when to pick it.
    pub about: &'static str,
}

impl Model {
    /// The file's name, in the store and upstream: `ggml-base.en.bin`.
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

    /// Largest download accepted for it: its size and a margin, so a
    /// server that never stops sending is cut off.
    pub fn max_bytes(&self) -> u64 {
        u64::from(self.size_mb) * 1_000_000 * 5 / 4 + 16_000_000
    }
}

/// Every model Clew offers, smallest first. The checksums are those of
/// whisper.cpp's `models/README.md`.
pub const MODELS: &[Model] = &[
    Model {
        id: "tiny.en",
        name: "Tiny (English)",
        size_mb: 78,
        sha1: "c78c86eb1a8faa21b369bcd33207cc90d64ae9df",
        english_only: true,
        about: "Fastest; fine for short commands on a slow machine.",
    },
    Model {
        id: "base.en",
        name: "Base (English)",
        size_mb: 148,
        sha1: "137c40403d78fd54d454da0f9bd998f78703390c",
        english_only: true,
        about: "Quick and accurate for commands and short questions.",
    },
    Model {
        id: "base",
        name: "Base (multilingual)",
        size_mb: 148,
        sha1: "465707469ff3a37a2b9b8d8f89f2f99de7299dac",
        english_only: false,
        about: "Base for any of whisper's languages.",
    },
    Model {
        id: "small",
        name: "Small (multilingual)",
        size_mb: 488,
        sha1: "55356645c2b361a969dfd0ef2c5a50d530afd8d5",
        english_only: false,
        about: "Better with accents, names and other languages.",
    },
    Model {
        id: "large-v3-turbo-q5_0",
        name: "Large v3 Turbo",
        size_mb: 574,
        sha1: "e050f7970618a659205450ad97eb95a18d69c9ee",
        english_only: false,
        about: "Near the best accuracy at a fraction of its cost.",
    },
    Model {
        id: "large-v3-q5_0",
        name: "Large v3",
        size_mb: 1080,
        sha1: "e6e2ed78495d403bef4b7cff42ef4aaadcfea8de",
        english_only: false,
        about: "Most accurate; slow without a fast GPU.",
    },
];

/// The model offered first: small enough to fetch in a moment, and good at
/// what voice in Clew is mostly for.
pub const DEFAULT_MODEL: &str = "base.en";

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
        fs::create_dir_all(&self.dir)?;
        let part = self.dir.join(format!("{}{PART}", model.file_name()));
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
            model,
            file: Some(file),
            part,
            dest: self.path(model),
            hasher: Sha1::new(),
            written: 0,
        })
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

/// A model being downloaded: bytes written to it are hashed on their way
/// to a partial file, which [`Install::finish`] renames into place only if
/// the hash is the one listed. Dropped unfinished — cancelled, failed — it
/// deletes what it wrote.
pub struct Install {
    model: &'static Model,
    file: Option<File>,
    part: PathBuf,
    dest: PathBuf,
    hasher: Sha1,
    written: u64,
}

impl Install {
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
        let digest = hex(&std::mem::take(&mut self.hasher).finalize());
        if digest != self.model.sha1 {
            return Err(format!(
                "the download is not {} as published (its checksum is {digest}, not {}) — \
                 it was discarded",
                self.model.file_name(),
                self.model.sha1
            ));
        }
        fs::rename(&self.part, &self.dest).map_err(|e| format!("could not save the model: {e}"))?;
        Ok(self.dest.clone())
    }
}

impl Write for Install {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > self.model.max_bytes() {
            return Err(io::Error::other(format!(
                "the download is larger than {} can be",
                self.model.file_name()
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

    /// A model whose checksum is that of `bytes`, for a test's install.
    fn model_of(bytes: &[u8]) -> &'static Model {
        let sha1: &'static str = Box::leak(hex(&Sha1::digest(bytes)).into_boxed_str());
        Box::leak(Box::new(Model {
            id: "test",
            name: "Test",
            size_mb: 1,
            sha1,
            english_only: true,
            about: "",
        }))
    }

    fn temp_store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("clew-voice-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Store::new(dir)
    }

    #[test]
    fn the_catalog_is_whisper_cpps() {
        assert!(find(DEFAULT_MODEL).is_some());
        let mut ids = std::collections::HashSet::new();
        for m in MODELS {
            assert!(ids.insert(m.id), "{} twice", m.id);
            assert_eq!(m.sha1.len(), 40, "{}", m.id);
            assert!(
                m.sha1
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
            assert_eq!(m.english_only, m.id.ends_with(".en"), "{}", m.id);
            assert!(m.max_bytes() > u64::from(m.size_mb) * 1_000_000);
        }
        let base = find("base.en").unwrap();
        assert_eq!(base.file_name(), "ggml-base.en.bin");
        assert_eq!(
            base.url(),
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin"
        );
        // Smallest first.
        assert!(MODELS.windows(2).all(|w| w[0].size_mb <= w[1].size_mb));
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
        let model = model_of(b"x");
        let mut install = store.begin(model).unwrap();
        install.write_all(b"half").unwrap();
        drop(install);
        assert_eq!(fs::read_dir(store.dir()).unwrap().count(), 0);
        // Nor does an earlier attempt's partial file stop a new one.
        fs::write(store.dir().join("ggml-test.bin.part"), b"stale").unwrap();
        let mut install = store.begin(model).unwrap();
        install.write_all(b"x").unwrap();
        install.finish().unwrap();
        assert_eq!(fs::read(store.path(model)).unwrap(), b"x");
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
