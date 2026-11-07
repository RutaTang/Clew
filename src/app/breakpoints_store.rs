//! The breakpoints a reader set in a project, kept between runs of clew.
//!
//! They were held in memory only, so every restart dropped them — a
//! conditional breakpoint typed in yesterday was gone today. They are the
//! reader's, not the repository's, so they live beside the project's other
//! derived files in clew's own data directory (`clew_core::derived::dir`,
//! keyed by host and root), never in the project's `.clew/`: a remote
//! project keeps them exactly as a local one does.
//!
//! Each change is made to the file as it stands, one line at a time, under
//! the same locks as Ask's sessions (a `Mutex` for this clew's windows, a
//! file lock for another clew): a window writing its own list whole dropped
//! the breakpoints another window had set since it opened the project.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::app::model::Bp;
use crate::app::prelude::*;
use crate::*;

/// The file, in the project's derived directory.
const FILE: &str = "breakpoints.json";
/// Its layout; a file of another one is not this build's to read or write.
const VERSION: u32 = 1;

/// One kept breakpoint: where, by the project-relative path, and when.
/// A field this build does not know makes the file not its own to write
/// (`load_checked`): read past, it was dropped from every breakpoint as the
/// file was written again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Saved {
    pub(crate) rel: String,
    /// 1-based, as the gutter numbers it.
    pub(crate) line: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) condition: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BreakpointsFile {
    version: u32,
    breakpoints: Vec<Saved>,
}

/// The breakpoints kept in `dir`: none when there are none yet; an error
/// when there is a file this version cannot read — another clew's layout,
/// a hand edit, a disk error — which is then never written over. An entry
/// whose path would leave the project is not shown.
fn load_checked(dir: &Path) -> std::io::Result<Vec<Saved>> {
    let bytes = match std::fs::read(dir.join(FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    // Empty: what a crash can leave of a file written as it struck. It
    // holds nothing to lose, where refused, every later change was.
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    // Its version first, whatever else it holds: another version's file is
    // said to be one.
    #[derive(Deserialize)]
    struct Versioned {
        version: u32,
    }
    let unreadable = |e: serde_json::Error| std::io::Error::other(format!("{FILE}: {e}"));
    let version = serde_json::from_slice::<Versioned>(&bytes)
        .map_err(unreadable)?
        .version;
    if version != VERSION {
        return Err(std::io::Error::other(format!(
            "{FILE} is version {version}, not {VERSION}"
        )));
    }
    let file = serde_json::from_slice::<BreakpointsFile>(&bytes).map_err(unreadable)?;
    Ok(file
        .breakpoints
        .into_iter()
        .filter(|s| s.line > 0 && clew_core::statefile::safe_rel(&s.rel))
        .collect())
}

/// [`load_checked`] for setting them again. A file of this version that
/// holds what this build does not know (a field added without a new
/// version) still sets what it can read: only writing it over would lose
/// that, and refused, none of the project's breakpoints came back. An error
/// says why none could be read.
pub(crate) fn load(dir: &Path) -> std::io::Result<Vec<Saved>> {
    load_checked(dir).or_else(|e| read_past_the_unknown(dir).ok_or(e))
}

/// The breakpoints in a file of this version, past fields this build does
/// not know; `None` when it is not one.
fn read_past_the_unknown(dir: &Path) -> Option<Vec<Saved>> {
    #[derive(Deserialize)]
    struct Loose {
        rel: String,
        line: usize,
        #[serde(default)]
        condition: Option<String>,
    }
    #[derive(Deserialize)]
    struct LooseFile {
        version: u32,
        breakpoints: Vec<Loose>,
    }
    let bytes = std::fs::read(dir.join(FILE)).ok()?;
    let file = serde_json::from_slice::<LooseFile>(&bytes).ok()?;
    (file.version == VERSION).then(|| {
        file.breakpoints
            .into_iter()
            .filter(|s| s.line > 0 && clew_core::statefile::safe_rel(&s.rel))
            .map(|s| Saved {
                rel: s.rel,
                line: s.line,
                condition: s.condition,
            })
            .collect()
    })
}

/// Write `list` whole beside the file, synced to the disk, then move it
/// over: a crash leaves the old file or the new, never half of one (moved
/// over unsynced, a crash could leave it empty).
fn save(dir: &Path, list: &[Saved]) -> std::io::Result<()> {
    clew_core::derived::ensure_private_dir(dir)?;
    let file = BreakpointsFile {
        version: VERSION,
        breakpoints: list.to_vec(),
    };
    let bytes = serde_json::to_vec(&file).map_err(std::io::Error::other)?;
    clew_core::statefile::write_atomic(&dir.join(FILE), &bytes)
}

/// Serializes [`edit`]'s read, change and write across this process's
/// windows.
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Make the kept breakpoint at `rel:line` what `now` says — there, with its
/// condition, or gone — in the file as it stands.
fn edit(dir: &Path, rel: &str, line: usize, now: Option<Option<String>>) -> std::io::Result<()> {
    let _serialized = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clew_core::derived::ensure_private_dir(dir)?;
    let _held = clew_core::statefile::lock(&dir.join(FILE))?;
    // A file it cannot read is left as it is: read as none, it was written
    // over with this one change, and every other breakpoint in it was lost.
    let mut list = load_checked(dir)?;
    list.retain(|s| !(s.rel == rel && s.line == line));
    if let Some(condition) = now {
        list.push(Saved {
            rel: rel.to_string(),
            line,
            condition,
        });
    }
    list.sort_by(|a, b| a.rel.cmp(&b.rel).then(a.line.cmp(&b.line)));
    save(dir, &list)
}

impl App {
    /// Keep the breakpoint at `path:line` as it now is in this window — set
    /// (with its condition) or cleared. A file outside the project, or a
    /// project without a data directory, keeps it for this run only.
    pub(crate) fn keep_breakpoint(&mut self, path: &Path, line: usize) {
        let (Some(dir), Some(root)) = (
            self.proj.derived_dir.clone(),
            self.proj.project.as_ref().map(|p| p.root.clone()),
        ) else {
            return;
        };
        let Ok(rel) = path.strip_prefix(&root) else {
            return;
        };
        let rel = clew_core::rel_string(rel);
        let now = self
            .debug
            .breakpoints
            .get(path)
            .and_then(|file| file.get(&line))
            .map(|bp| bp.condition.clone());
        if let Err(e) = edit(&dir, &rel, line, now) {
            self.status = tr!(
                "Could not save the breakpoints: {error} — kept for this session",
                error = e
            );
        }
    }

    /// Set again the breakpoints kept for the project just opened. One this
    /// window already has keeps its own state.
    pub(crate) fn restore_breakpoints(&mut self) {
        let (Some(dir), Some(root)) = (
            self.proj.derived_dir.clone(),
            self.proj.project.as_ref().map(|p| p.root.clone()),
        ) else {
            return;
        };
        let saved = match load(&dir) {
            Ok(saved) => saved,
            Err(e) => {
                self.status = tr!("Could not read the saved breakpoints: {error}", error = e);
                return;
            }
        };
        for saved in saved {
            self.debug
                .breakpoints
                .entry(root.join(&saved.rel))
                .or_default()
                .entry(saved.line)
                .or_insert_with(|| Bp {
                    condition: saved.condition,
                    ..Bp::default()
                });
        }
    }
}
