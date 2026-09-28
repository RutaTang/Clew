//! Unpacking downloaded server (and debug-adapter) archives, confined and
//! bounded.
//!
//! Every artifact is checked against its pinned SHA-256 before it reaches
//! this module, so the extractor is not the defense against a swapped
//! download. It is the defense against a PINNED artifact that turns out to be
//! pathological — a compression bomb or a hostile entry published upstream —
//! which would otherwise act with the full reach of one consent click. So:
//!
//! - every entry name must be a plain relative path: absolute paths, `..`,
//!   NULs and backslashes are refused and fail the whole install (they used
//!   to be skipped silently in zips, and skipped by `tar::unpack_in`'s own
//!   check without anyone looking at its answer);
//! - symbolic and hard links are refused outright, as are device and FIFO
//!   entries. None of the pinned artifacts contains one, and a link is the
//!   one entry kind whose effect depends on what else is on disk — a link
//!   planted by one entry redirects the writes of later ones;
//! - files are created with `create_new`, so no entry writes through anything
//!   that already exists, and their modes are normalized (0755 or 0644: no
//!   setuid, no group/world write);
//! - the budget counts bytes actually WRITTEN, not the sizes entries declare,
//!   and the xz decoder feeds tar through a bounded channel instead of
//!   inflating the whole tarball in memory before anything is checked;
//! - the decompressed tar STREAM is capped too, and a stream that runs past
//!   its cap is an error — never an end of archive (see [`CappedReader`]).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use super::registry::Archive;

/// How much one archive may expand to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    /// Bytes written to disk, all entries together.
    pub bytes: u64,
    /// Entries of any kind, directories included.
    pub entries: usize,
}

/// The budget real installs get. The largest artifact expands to a few
/// hundred megabytes; the cap exists for a pinned artifact that expands
/// pathologically and would otherwise fill the disk.
pub(crate) const BUDGET: Budget = Budget {
    bytes: 2 * 1024 * 1024 * 1024,
    entries: 20_000,
};

/// Unpack `bytes` (an already verified artifact) into `dest`, which must be
/// an empty directory the caller owns. `binary` names the output file for a
/// single-file gzip artifact; archives carry their own names.
pub(crate) fn unpack(
    archive: Archive,
    bytes: &[u8],
    dest: &Path,
    binary: &str,
    budget: Budget,
) -> Result<(), String> {
    match archive {
        Archive::Gzip => gunzip_single(bytes, dest, binary, budget),
        Archive::Zip => extract_zip(bytes, dest, budget),
        Archive::TarXz => extract_tar_xz(bytes, dest, budget),
        Archive::TarGz => extract_tar(flate2::read::GzDecoder::new(bytes), dest, budget),
    }
}

/// What an entry name turned out to be.
enum Name {
    /// A plain relative path.
    Path(PathBuf),
    /// The archive's own root (`./`): fine for a directory entry, nothing to
    /// create.
    Root,
}

/// Classify an entry name, refusing anything that could land outside the
/// destination: absolute paths, `..` anywhere, NULs, and backslashes (a
/// separator to Windows, and never part of a real name in these archives).
fn entry_name(name: &str) -> Result<Name, String> {
    let refuse = || {
        Err(format!(
            "archive entry {name:?} is not a plain relative path — refusing"
        ))
    };
    if name.starts_with('/') || name.contains(['\0', '\\']) {
        return refuse();
    }
    let mut path = PathBuf::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => return refuse(),
            // `C:` would be a drive prefix to Windows.
            part if cfg!(windows) && part.contains(':') => return refuse(),
            part => path.push(part),
        }
    }
    Ok(if path.as_os_str().is_empty() {
        Name::Root
    } else {
        Name::Path(path)
    })
}

/// The write side of an extraction: budget accounting and the file-system
/// effects, identical for every archive format.
struct Extractor<'a> {
    dest: &'a Path,
    bytes_left: u64,
    entries_left: usize,
    entry_cap: usize,
}

impl<'a> Extractor<'a> {
    fn new(dest: &'a Path, budget: Budget) -> Self {
        Extractor {
            dest,
            bytes_left: budget.bytes,
            entries_left: budget.entries,
            entry_cap: budget.entries,
        }
    }

    /// Count one entry against the budget.
    fn entry(&mut self) -> Result<(), String> {
        if self.entries_left == 0 {
            return Err(format!(
                "archive has over {} entries — refusing",
                self.entry_cap
            ));
        }
        self.entries_left -= 1;
        Ok(())
    }

    fn dir(&mut self, rel: &Path) -> Result<(), String> {
        std::fs::create_dir_all(self.dest.join(rel)).map_err(|e| format!("unpack failed: {e}"))
    }

    /// Write one regular file from `data`, through the remaining budget.
    fn file(&mut self, rel: &Path, data: &mut dyn Read, mode: Option<u32>) -> Result<(), String> {
        let path = self.dest.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("unpack failed: {e}"))?;
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("unpack of {} failed: {e}", rel.display()))?;
        let mut writer = std::io::BufWriter::new(file);
        // `+ 1` so an entry exactly at the remaining budget still fits and one
        // byte more is visible as an overrun. The declared size is not
        // trusted: the budget binds what the decoder actually produces.
        let written = std::io::copy(&mut data.take(self.bytes_left + 1), &mut writer)
            .map_err(|e| format!("unpack of {} failed: {e}", rel.display()))?;
        if written > self.bytes_left {
            return Err("archive contents exceed the unpack budget — refusing".into());
        }
        self.bytes_left -= written;
        writer
            .flush()
            .map_err(|e| format!("unpack of {} failed: {e}", rel.display()))?;
        set_mode(&path, mode)
    }
}

/// 0755 for anything the archive marks executable, 0644 otherwise: never
/// setuid/setgid/sticky, never group- or world-writable.
#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let executable = mode.is_some_and(|m| m & 0o111 != 0);
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("unpack failed: {e}"))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Option<u32>) -> Result<(), String> {
    Ok(())
}

/// A single gzip-compressed executable, written as `dest/<binary>`.
fn gunzip_single(bytes: &[u8], dest: &Path, binary: &str, budget: Budget) -> Result<(), String> {
    let Name::Path(rel) = entry_name(binary)? else {
        return Err("no output name for a single-file artifact".into());
    };
    let mut ex = Extractor::new(dest, budget);
    ex.entry()?;
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    ex.file(&rel, &mut decoder, Some(0o755))
        .map_err(|e| e.replace("unpack of", "gunzip of"))
}

/// Extract a zip archive, preserving its tree.
fn extract_zip(bytes: &[u8], dest: &Path, budget: Budget) -> Result<(), String> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("bad zip: {e}"))?;
    if archive.len() > budget.entries {
        return Err(format!(
            "archive has over {} entries — refusing",
            budget.entries
        ));
    }
    let mut ex = Extractor::new(dest, budget);
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("bad zip: {e}"))?;
        ex.entry()?;
        let name = entry.name().to_string();
        let rel = match entry_name(&name)? {
            Name::Path(rel) => rel,
            Name::Root if entry.is_dir() => continue,
            Name::Root => return Err(format!("archive entry {name:?} has no name — refusing")),
        };
        if entry.is_symlink() {
            return Err(format!("archive entry {name:?} is a link — refusing"));
        }
        if entry.is_dir() {
            ex.dir(&rel)?;
            continue;
        }
        let mode = entry.unix_mode();
        ex.file(&rel, &mut entry, mode)?;
    }
    Ok(())
}

/// Headroom in the tar STREAM over the content budget: headers, padding and
/// long-name records ride on top of the file bytes.
fn tar_stream_cap(budget: Budget) -> u64 {
    budget
        .bytes
        .saturating_add((budget.entries as u64 + 16).saturating_mul(4096))
}

/// What a stream past its cap is refused with.
const OVER_BUDGET: &str = "archive expands past the unpack budget — refusing";

/// A reader that yields at most `left` bytes and then FAILS if its source has
/// more, where `Read::take` would quietly end.
///
/// The difference is the whole point. To a tar reader, end of input where the
/// next header would start is the end of the archive — so a stream cut at a
/// 512-byte block boundary read as a complete, smaller archive, and an
/// over-budget `.tar.gz` installed silently truncated. (The `.tar.xz` path's
/// decoder already reported its overrun; this gives every tar stream the same
/// verdict.)
struct CappedReader<R> {
    inner: R,
    left: u64,
    /// Set once the source was found to hold more than the cap. Sticky: every
    /// later read fails too.
    over_budget: bool,
}

impl<R: Read> CappedReader<R> {
    fn new(inner: R, cap: u64) -> Self {
        CappedReader {
            inner,
            left: cap,
            over_budget: false,
        }
    }

    fn overrun(&mut self) -> std::io::Error {
        self.over_budget = true;
        std::io::Error::other(OVER_BUDGET)
    }
}

impl<R: Read> Read for CappedReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.over_budget {
            return Err(self.overrun());
        }
        if out.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            // At the cap: the source has to be at its end as well.
            let mut probe = [0u8; 1];
            let more = loop {
                match self.inner.read(&mut probe) {
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    read => break read? > 0,
                }
            };
            return if more { Err(self.overrun()) } else { Ok(0) };
        }
        let room = usize::try_from(self.left)
            .unwrap_or(usize::MAX)
            .min(out.len());
        let n = self.inner.read(&mut out[..room])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// Extract a tar stream, preserving its tree. The stream itself is capped
/// (see [`CappedReader`]), and running past the cap fails the extraction
/// with the budget's own message, whatever the tar reader made of it.
fn extract_tar<R: Read>(reader: R, dest: &Path, budget: Budget) -> Result<(), String> {
    let mut stream = CappedReader::new(reader, tar_stream_cap(budget));
    let extracted = extract_tar_entries(&mut stream, dest, budget);
    if stream.over_budget {
        return Err(OVER_BUDGET.into());
    }
    extracted
}

fn extract_tar_entries<R: Read>(reader: R, dest: &Path, budget: Budget) -> Result<(), String> {
    use tar::EntryType;
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| format!("tar extract failed: {e}"))?;
    let mut ex = Extractor::new(dest, budget);
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("tar extract failed: {e}"))?;
        let kind = entry.header().entry_type();
        // Archive-wide metadata (`git archive` writes one), not a file.
        if kind == EntryType::XGlobalHeader {
            continue;
        }
        ex.entry()?;
        let raw = entry.path_bytes();
        let name = std::str::from_utf8(&raw)
            .map_err(|_| "archive entry with a non-UTF-8 name — refusing".to_string())?
            .to_string();
        let rel = match entry_name(&name)? {
            Name::Path(rel) => rel,
            Name::Root if kind == EntryType::Directory => continue,
            Name::Root => return Err(format!("archive entry {name:?} has no name — refusing")),
        };
        match kind {
            EntryType::Directory => ex.dir(&rel)?,
            EntryType::Regular | EntryType::Continuous => {
                let mode = entry.header().mode().ok();
                ex.file(&rel, &mut entry, mode)?;
            }
            EntryType::Symlink | EntryType::Link => {
                return Err(format!("archive entry {name:?} is a link — refusing"));
            }
            other => {
                return Err(format!(
                    "archive entry {name:?} is of an unsupported kind ({other:?}) — refusing"
                ));
            }
        }
    }
    Ok(())
}

/// Bytes flow from the xz decoder thread to the tar reader in chunks of this
/// size, at most a few in flight: memory stays small whatever the archive
/// expands to.
const CHUNK: usize = 256 * 1024;

/// The decoder's side of the channel, refusing to produce more than `left`
/// bytes in total.
struct ChunkWriter {
    tx: SyncSender<Vec<u8>>,
    buf: Vec<u8>,
    left: u64,
    over_budget: bool,
}

impl ChunkWriter {
    fn send(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK));
        self.tx
            .send(chunk)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "extractor gone"))
    }
}

impl Write for ChunkWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() as u64 > self.left {
            self.over_budget = true;
            return Err(std::io::Error::other("the archive expands past the budget"));
        }
        self.left -= data.len() as u64;
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.send()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.send()
    }
}

/// The extractor's side: a `Read` over the chunks, at EOF once the decoder
/// is done and has dropped its sender.
struct ChunkReader {
    rx: Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    at: usize,
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.at >= self.chunk.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.at = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.chunk.len() - self.at);
        out[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Extract an xz-compressed tar. The decoder runs on its own thread and
/// streams into the tar reader, so the tarball is never materialized whole;
/// it stops as soon as the stream passes the budget or the extractor quits.
fn extract_tar_xz(bytes: &[u8], dest: &Path, budget: Budget) -> Result<(), String> {
    let cap = tar_stream_cap(budget);
    let (extracted, (decoded, over_budget)) = std::thread::scope(|scope| {
        let (tx, rx) = sync_channel::<Vec<u8>>(4);
        let decoder = scope.spawn(move || {
            let mut writer = ChunkWriter {
                tx,
                buf: Vec::with_capacity(CHUNK),
                left: cap,
                over_budget: false,
            };
            let result = lzma_rs::xz_decompress(&mut std::io::Cursor::new(bytes), &mut writer)
                .map_err(|e| format!("xz decode failed: {e}"))
                .and_then(|()| writer.flush().map_err(|e| format!("xz decode failed: {e}")));
            (result, writer.over_budget)
        });
        let mut reader = ChunkReader {
            rx,
            chunk: Vec::new(),
            at: 0,
        };
        let extracted = extract_tar(&mut reader, dest, budget);
        if extracted.is_ok() {
            // Let the decoder finish (its integrity check included) by
            // taking what is left: tar padding, bounded by the writer's cap.
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        }
        // Dropping the receiver stops a decoder the extractor gave up on.
        drop(reader);
        let decoded = decoder
            .join()
            .unwrap_or_else(|_| (Err("the xz decoder panicked".to_string()), false));
        (extracted, decoded)
    });
    if over_budget {
        return Err(OVER_BUDGET.into());
    }
    extracted?;
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: Budget = Budget {
        bytes: 1024 * 1024,
        entries: 64,
    };

    /// A fresh directory, unique per process AND per call (several tests
    /// share a tag, and the test harness runs them in parallel), removed
    /// when the guard drops.
    fn scratch(tag: &str) -> crate::testutil::TempDir {
        crate::testutil::TempDir::new(&format!("archive-{tag}"))
    }

    /// A tar with raw, unvalidated names — `tar::Builder` refuses to write the
    /// hostile ones, which is exactly what these tests need to produce.
    fn raw_tar(entries: &[(&str, tar::EntryType, &[u8], &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut out);
            for (name, kind, data, link) in entries {
                let mut header = tar::Header::new_old();
                {
                    let bytes = header.as_mut_bytes();
                    bytes[..name.len()].copy_from_slice(name.as_bytes());
                }
                header.set_entry_type(*kind);
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                if !link.is_empty() {
                    let bytes = header.as_mut_bytes();
                    bytes[157..157 + link.len()].copy_from_slice(link.as_bytes());
                }
                header.set_cksum();
                builder.append(&header, *data).unwrap();
            }
            builder.finish().unwrap();
        }
        out
    }

    fn zip_of(entries: &[(&str, &[u8])], links: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            for (name, data) in entries {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(data).unwrap();
            }
            for (name, target) in links {
                zw.add_symlink(*name, *target, opts).unwrap();
            }
            zw.finish().unwrap();
        }
        buf
    }

    fn xz(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        lzma_rs::xz_compress(&mut std::io::Cursor::new(data), &mut out).unwrap();
        out
    }

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    /// A tarball in both compressed forms clew unpacks, so every tar test
    /// covers both decoders: they reach `extract_tar` by different roads (a
    /// streaming gzip reader; an xz decoder thread behind a bounded channel).
    fn compressed_tars(tar: &[u8]) -> [(Archive, Vec<u8>); 2] {
        [(Archive::TarXz, xz(tar)), (Archive::TarGz, gz(tar))]
    }

    #[test]
    fn a_clean_tar_and_zip_extract_with_normalized_modes() {
        let dir = scratch("clean");
        let tar = raw_tar(&[
            ("./", tar::EntryType::Directory, b"", ""),
            ("pkg/", tar::EntryType::Directory, b"", ""),
            ("pkg/bin/tool", tar::EntryType::Regular, b"#!/bin/sh\n", ""),
        ]);
        unpack(Archive::TarXz, &xz(&tar), &dir.join("x"), "", SMALL).unwrap();
        assert_eq!(
            std::fs::read(dir.join("x/pkg/bin/tool")).unwrap(),
            b"#!/bin/sh\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("x/pkg/bin/tool"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o7777, 0o755);
        }
        let zip = zip_of(&[("a/b.txt", b"hello")], &[]);
        unpack(Archive::Zip, &zip, &dir.join("z"), "", SMALL).unwrap();
        assert_eq!(std::fs::read(dir.join("z/a/b.txt")).unwrap(), b"hello");
    }

    /// Traversal and absolute names fail the WHOLE install. Zips used to skip
    /// such an entry silently, and tar's own `unpack_in` also skips — it
    /// reports `Ok(false)`, which nobody read.
    #[test]
    fn traversal_and_absolute_entries_are_refused() {
        for name in [
            "../evil",
            "a/../../evil",
            "/etc/evil",
            "a/..",
            "a\\..\\evil",
        ] {
            let tar = raw_tar(&[(name, tar::EntryType::Regular, b"x", "")]);
            for (format, bytes) in compressed_tars(&tar) {
                let dir = scratch("traversal");
                let err = unpack(format, &bytes, &dir.join("t"), "", SMALL).unwrap_err();
                assert!(
                    err.contains("not a plain relative path"),
                    "{format:?} {name:?}: {err}"
                );
                assert!(!dir.join("evil").exists());
            }
        }
        // Zip, including the forms `enclosed_name` would have skipped.
        let dir = scratch("zip-traversal");
        for (i, name) in ["../evil", "/abs/evil", "a/../../evil"].iter().enumerate() {
            let zip = zip_of(&[("ok.txt", b"fine"), (name, b"x")], &[]);
            let dest = dir.join(format!("z{i}"));
            let err = unpack(Archive::Zip, &zip, &dest, "", SMALL).unwrap_err();
            assert!(err.contains("not a plain relative path"), "{name:?}: {err}");
        }
        assert!(!dir.join("evil").exists());
    }

    /// A link is the one entry kind that can redirect later writes, and no
    /// pinned artifact carries one — so any link fails the install, whether
    /// it points out of the tree or not.
    #[test]
    fn link_entries_are_refused() {
        for (kind, target) in [
            (tar::EntryType::Symlink, "/etc"),
            (tar::EntryType::Symlink, "inside"),
            (tar::EntryType::Link, "inside"),
        ] {
            let tar = raw_tar(&[
                ("inside", tar::EntryType::Regular, b"x", ""),
                ("link", kind, b"", target),
            ]);
            for (format, bytes) in compressed_tars(&tar) {
                let dir = scratch("tar-link");
                let err = unpack(format, &bytes, &dir.join("t"), "", SMALL).unwrap_err();
                assert!(
                    err.contains("is a link"),
                    "{format:?} {kind:?} -> {target}: {err}"
                );
                assert!(std::fs::symlink_metadata(dir.join("t/link")).is_err());
            }
        }
        let dir = scratch("zip-link");
        let zip = zip_of(&[("a.txt", b"x")], &[("escape", "/etc/passwd")]);
        let err = unpack(Archive::Zip, &zip, &dir.join("z"), "", SMALL).unwrap_err();
        assert!(err.contains("is a link"), "{err}");
        assert!(std::fs::symlink_metadata(dir.join("z/escape")).is_err());
    }

    #[test]
    fn device_and_fifo_entries_are_refused() {
        let dir = scratch("special");
        let tar = raw_tar(&[("pipe", tar::EntryType::Fifo, b"", "")]);
        let err = unpack(Archive::TarXz, &xz(&tar), &dir.join("t"), "", SMALL).unwrap_err();
        assert!(err.contains("unsupported kind"), "{err}");
    }

    /// The byte budget binds bytes actually produced — for every format —
    /// and the entry budget binds entries.
    #[test]
    fn unpack_budgets_are_enforced() {
        let tight = Budget {
            bytes: 1000,
            entries: 8,
        };
        let big = vec![b'a'; 4096];
        let dir = scratch("budget");

        let tar = raw_tar(&[("big", tar::EntryType::Regular, &big, "")]);
        let err = unpack(Archive::TarXz, &xz(&tar), &dir.join("t1"), "", tight).unwrap_err();
        assert!(err.contains("budget"), "tar.xz: {err}");

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar).unwrap();
        let tgz = gz.finish().unwrap();
        let err = unpack(Archive::TarGz, &tgz, &dir.join("t2"), "", tight).unwrap_err();
        assert!(err.contains("budget"), "tar.gz: {err}");

        let zip = zip_of(&[("big", &big)], &[]);
        let err = unpack(Archive::Zip, &zip, &dir.join("t3"), "", tight).unwrap_err();
        assert!(err.contains("budget"), "zip: {err}");

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&big).unwrap();
        let single = gz.finish().unwrap();
        let err = unpack(Archive::Gzip, &single, &dir.join("t4"), "tool", tight).unwrap_err();
        assert!(err.contains("budget"), "gzip: {err}");

        // Exactly at the budget still fits.
        let exact = vec![b'b'; 1000];
        let tar = raw_tar(&[("exact", tar::EntryType::Regular, &exact, "")]);
        unpack(Archive::TarXz, &xz(&tar), &dir.join("t5"), "", tight).unwrap();

        // Entries.
        let many: Vec<(String, tar::EntryType, &[u8], &str)> = (0..20)
            .map(|i| (format!("f{i}"), tar::EntryType::Regular, &b"x"[..], ""))
            .collect();
        let many: Vec<(&str, tar::EntryType, &[u8], &str)> = many
            .iter()
            .map(|(n, k, d, l)| (n.as_str(), *k, *d, *l))
            .collect();
        let err = unpack(
            Archive::TarXz,
            &xz(&raw_tar(&many)),
            &dir.join("t6"),
            "",
            tight,
        )
        .unwrap_err();
        assert!(err.contains("entries"), "{err}");
        let names: Vec<String> = (0..20).map(|i| format!("f{i}")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
        let err = unpack(
            Archive::Zip,
            &zip_of(&files, &[]),
            &dir.join("t7"),
            "",
            tight,
        )
        .unwrap_err();
        assert!(err.contains("entries"), "{err}");
    }

    /// The DECOMPRESSED stream is capped, not only the files written: the
    /// decoder stops as soon as it passes the cap, even when the tar side
    /// sees nothing worth writing (a run of zero blocks reads as an empty
    /// archive). The old path inflated the entire tarball into memory before
    /// checking anything.
    #[test]
    fn the_decompressed_stream_is_capped() {
        let tight = Budget {
            bytes: 64 * 1024,
            entries: 4,
        };
        let bomb = xz(&vec![0u8; 4 * 1024 * 1024]);
        let dir = scratch("bomb");
        let err = unpack(Archive::TarXz, &bomb, &dir.join("b"), "", tight).unwrap_err();
        assert!(err.contains("budget"), "{err}");

        // Stream the tar reader consumes without writing a byte or counting
        // an entry — a pax global header's body, which is skipped — must be
        // capped for BOTH decoders. Through gzip it used to hit a plain
        // `take`, and fail (if at all) as a confusing "unexpected EOF".
        let tar = raw_tar(&[
            (
                "pax_global_header",
                tar::EntryType::XGlobalHeader,
                &vec![b'x'; 4 * 1024 * 1024],
                "",
            ),
            ("after", tar::EntryType::Regular, b"x", ""),
        ]);
        for (format, bytes) in compressed_tars(&tar) {
            let dir = scratch("stream-bomb");
            let err = unpack(format, &bytes, &dir.join("b"), "", tight).unwrap_err();
            assert!(err.contains("budget"), "{format:?}: {err}");
            assert!(!dir.join("b/after").exists());
        }
    }

    /// F12: a tar stream that runs past its cap exactly at a block boundary
    /// used to read as a COMPLETE archive through gzip — `take` ends the
    /// stream there, and end of input where a header would start is the end
    /// of the archive to a tar reader — so the install went ahead with
    /// whatever came before the cut. Past the cap is now an error.
    #[test]
    fn a_stream_cut_at_a_block_boundary_is_refused_not_truncated() {
        let budget = Budget {
            bytes: 1024,
            entries: 4,
        };
        let cap = tar_stream_cap(budget);
        // "first" takes two blocks, the global header's own block one more:
        // size the header's body so the cap falls exactly after it.
        let filler = vec![b' '; (cap - 3 * 512) as usize];
        assert_eq!(
            filler.len() % 512,
            0,
            "the cut must land on a block boundary"
        );
        let tar = raw_tar(&[
            ("first", tar::EntryType::Regular, b"1", ""),
            (
                "pax_global_header",
                tar::EntryType::XGlobalHeader,
                &filler,
                "",
            ),
            ("second", tar::EntryType::Regular, b"2", ""),
        ]);
        for (format, bytes) in compressed_tars(&tar) {
            let dir = scratch("cut");
            let err = unpack(format, &bytes, &dir.join("c"), "", budget).unwrap_err();
            assert!(err.contains("budget"), "{format:?}: {err}");
            assert!(!dir.join("c/second").exists(), "{format:?}");
        }
    }

    /// The capped reader itself: exactly the cap followed by the end of the
    /// source is fine; one byte more is an error, and stays one.
    #[test]
    fn the_capped_reader_errors_past_the_cap_instead_of_ending() {
        let read_all = |source: &[u8], cap: u64| {
            let mut reader = CappedReader::new(source, cap);
            let mut out = Vec::new();
            let result = reader.read_to_end(&mut out).map(|_| out);
            (result, reader.over_budget)
        };
        let (fits, over) = read_all(&[7u8; 100], 100);
        assert_eq!(fits.unwrap().len(), 100);
        assert!(!over);
        let (short, over) = read_all(&[7u8; 10], 100);
        assert_eq!(short.unwrap().len(), 10);
        assert!(!over);
        let (err, over) = read_all(&[7u8; 101], 100);
        assert!(err.unwrap_err().to_string().contains("budget"));
        assert!(over);

        let mut reader = CappedReader::new(&[7u8; 5][..], 2);
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert!(reader.read(&mut buf).is_err());
        assert!(reader.read(&mut buf).is_err(), "the overrun is sticky");
    }

    #[test]
    fn a_file_is_never_written_through_an_existing_entry() {
        let dir = scratch("dup");
        let tar = raw_tar(&[
            ("same", tar::EntryType::Regular, b"first", ""),
            ("same", tar::EntryType::Regular, b"second", ""),
        ]);
        let err = unpack(Archive::TarXz, &xz(&tar), &dir.join("d"), "", SMALL).unwrap_err();
        assert!(err.contains("same"), "{err}");
    }
}
