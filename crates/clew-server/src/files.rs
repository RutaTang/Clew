//! File reads served to the client (source views, notebooks) and the remote
//! folder picker's directory listing. Everything here is blocking; the
//! request loop runs it on a blocking thread.

use std::path::{Path, PathBuf};

use clew_core::confine::ConfineError;
use clew_core::{docs, highlight, inactive, outline};
use clew_protocol::Event;

use crate::{failed, refused};

/// Largest regular file `ReadFile` will serve — matching the client viewer's
/// own display limit, checked BEFORE reading so the size can't balloon the
/// reply first.
pub(crate) const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;
/// Notebooks embed base64 images, so their JSON runs far past source-file
/// sizes; still bounded.
pub(crate) const MAX_NOTEBOOK_BYTES: u64 = 64 * 1024 * 1024;
/// Most entries one directory listing returns. The folder picker lists
/// directories first, so a directory holding more than this — a mail spool, a
/// cache — still shows its subdirectories; the rest are left out rather than
/// building a frame the size of the directory.
pub(crate) const MAX_DIR_ENTRIES: usize = 10_000;

/// The reply to a client path that could not be confined to the project: a
/// refusal when its shape or a symlink would leave the project, a plain error
/// when the file simply is not there.
pub(crate) fn confine_refusal(rel: &str, e: &ConfineError) -> Event {
    if e.is_escape() {
        refused(format!("refused: path escapes project: {rel}"))
    } else {
        failed(format!("{rel}: {e}"))
    }
}

/// The whole `ReadFile` job: confine `rel` to `root`, read it, and build the
/// reply — `FileContent` (highlighted lines, outline, doc comments, inactive
/// `#[cfg]` lines) or `NotebookContent`, or the error saying why not.
pub(crate) fn read_file_event(root: &Path, rel: String, target: &inactive::Target) -> Event {
    // `rel` names whatever the client was pointed at, often by data — a
    // citation in a model's answer, a link in a document — so it is confined:
    // no absolute paths, no `..`, no symlink leading out of the project.
    let abs = match clew_core::confine::confine(root, &rel) {
        Ok(abs) => abs,
        Err(e) => return confine_refusal(&rel, &e),
    };
    // ONE open, then everything from that handle: the type check, the size,
    // and the bytes. Resolving the path three times (metadata, then read) let
    // a concurrent swap turn the target into a symlink, a FIFO that parks this
    // task forever, or a device — after it had passed the checks. The caps
    // match the client's own viewer limits.
    let notebook = clew_core::notebook::is_notebook(&abs);
    let limit = if notebook {
        MAX_NOTEBOOK_BYTES
    } else {
        MAX_READ_BYTES
    };
    let Some(file) = clew_core::statefile::open_plain(&abs) else {
        return failed(format!("{rel}: not a readable regular file"));
    };
    // fstat on the handle we will read, not on the name.
    match file.metadata() {
        Ok(meta) if meta.len() > limit => {
            return refused(format!(
                "{rel}: too large ({:.1} MB, limit {} MB)",
                meta.len() as f64 / (1024.0 * 1024.0),
                limit / (1024 * 1024)
            ));
        }
        Ok(_) => {}
        Err(e) => return failed(format!("read {rel}: {e}")),
    }
    // Read through the cap as well: the size above is a cheap early rejection,
    // but a file can grow while being read.
    let text = {
        use std::io::Read;
        let mut s = String::new();
        match file.take(limit + 1).read_to_string(&mut s) {
            Ok(_) if s.len() as u64 <= limit => Ok(s),
            Ok(_) => Err(format!("{rel}: grew past the {limit}-byte limit")),
            Err(e) => Err(format!("read {rel}: {e}")),
        }
    };
    let source = match text {
        Ok(source) => source,
        Err(message) => return failed(message),
    };
    // A notebook parses into cells (highlighted server-side) and replies as
    // `NotebookContent`; raw JSON is never shown.
    if notebook {
        return match clew_core::notebook::parse(&source) {
            Some(nb) => notebook_event(rel, nb),
            None => failed(format!("{rel}: not a readable notebook")),
        };
    }
    let lang = highlight::detect(&abs);
    let lines = highlight::highlight_lines(&source, lang);
    // Symbols, doc comments, and inactive #[cfg] lines — the rest of what a
    // file view shows, from one read.
    let (symbols, docs, inactive) = match lang {
        Some(key) => {
            let symbols = outline::extract(&source, key);
            let docs = docs::extract(&source, key, &symbols);
            let inactive = inactive::inactive_lines(&source, key, target);
            (symbols, docs, inactive)
        }
        None => Default::default(),
    };
    Event::FileContent {
        rel,
        source,
        lines,
        symbols,
        docs: docs.into_iter().collect(),
        inactive: inactive.into_iter().collect(),
    }
}

/// List a directory on this host for the remote folder picker. `path` is an
/// absolute or `~`-relative directory, or `None` for the login home. Directories
/// sort before files, each alphabetically (case-insensitive). Unreadable entries
/// are skipped rather than failing the whole listing; at most
/// [`MAX_DIR_ENTRIES`] are returned, and the listing counts the rest
/// (`omitted`) so the picker can say it is not showing everything.
///
/// Not confined to a project: the picker browses BEFORE one is chosen, and the
/// server runs as the user on their own host.
pub(crate) fn list_dir(path: Option<String>) -> Event {
    list_dir_capped(path, MAX_DIR_ENTRIES)
}

fn list_dir_capped(path: Option<String>, cap: usize) -> Event {
    let home = std::env::var("HOME").ok();
    // Resolve the target directory: home when unset, `~`-expanded, else as given.
    let dir: PathBuf = match path.as_deref() {
        None | Some("") | Some("~") => match &home {
            Some(h) => PathBuf::from(h),
            None => PathBuf::from("/"),
        },
        Some(p) if p.starts_with("~/") => match &home {
            Some(h) => Path::new(h).join(p.trim_start_matches("~/")),
            None => PathBuf::from(p),
        },
        Some(p) => PathBuf::from(p),
    };
    // Canonicalize so the reported path and its parent are stable and absolute.
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) => return failed(format!("cannot list {}: {e}", dir.display())),
    };
    // Directories and files are kept apart so that a directory crowded with
    // files still lists its subdirectories; each list holds at most `cap`
    // names, and the two together are cut to `cap` after sorting. `seen`
    // counts every entry that would have been listed, kept or not.
    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    let mut seen = 0usize;
    for ent in read.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        // A symlink to a directory should still browse as one.
        let is_dir = match ent.file_type() {
            Ok(ft) if ft.is_symlink() => std::fs::metadata(ent.path())
                .map(|m| m.is_dir())
                .unwrap_or(false),
            Ok(ft) => ft.is_dir(),
            Err(_) => continue,
        };
        seen += 1;
        let list = if is_dir { &mut dirs } else { &mut files };
        if list.len() < cap {
            list.push(clew_protocol::DirEntry { name, is_dir });
        }
    }
    for list in [&mut dirs, &mut files] {
        list.sort_by_cached_key(|e| (e.name.to_lowercase(), e.name.clone()));
    }
    let mut entries = dirs;
    entries.extend(files);
    entries.truncate(cap);
    Event::DirListing {
        path: dir.to_string_lossy().into_owned(),
        parent: dir.parent().map(|p| p.to_string_lossy().into_owned()),
        omitted: seen - entries.len(),
        entries,
    }
}

/// Build the `NotebookContent` reply for a parsed notebook: highlight each
/// code cell with the notebook's language and map cells/outputs/outline onto
/// the protocol types.
pub(crate) fn notebook_event(rel: String, nb: clew_core::notebook::Notebook) -> Event {
    use clew_core::notebook as nbk;
    let key = highlight::static_key(&nb.language);
    let cells = nb
        .cells
        .iter()
        .map(|c| clew_protocol::NotebookCell {
            kind: match c.kind {
                nbk::CellKind::Markdown => "markdown",
                nbk::CellKind::Code => "code",
                nbk::CellKind::Raw => "raw",
            }
            .to_string(),
            lines: if c.kind == nbk::CellKind::Code {
                highlight::highlight_lines(&c.source, key)
            } else {
                Vec::new()
            },
            source: c.source.clone(),
            proj_line: c.proj_line,
            outputs: c
                .outputs
                .iter()
                .map(|o| match o {
                    nbk::Output::Text { spans, stderr } => clew_protocol::NotebookOutput::Text {
                        spans: spans.clone(),
                        stderr: *stderr,
                    },
                    nbk::Output::Image { data } => {
                        clew_protocol::NotebookOutput::Image { data: data.clone() }
                    }
                    nbk::Output::Svg(s) => clew_protocol::NotebookOutput::Svg(s.clone()),
                    nbk::Output::Placeholder(l) => {
                        clew_protocol::NotebookOutput::Placeholder(l.clone())
                    }
                })
                .collect(),
            execution_count: c.execution_count,
        })
        .collect();
    let symbols = nb
        .outline()
        .into_iter()
        .map(|(name, kind, line, end_line)| clew_protocol::Symbol {
            name,
            kind,
            line,
            end_line,
        })
        .collect();
    Event::NotebookContent {
        rel,
        language: nb.language,
        cells,
        symbols,
        projection: nb.projection,
    }
}

#[cfg(test)]
mod tests {
    use super::list_dir_capped;
    use clew_protocol::Event;

    /// A listing is bounded: directories first, then files, and nothing past
    /// the cap — a huge directory must not become a frame its own size.
    #[test]
    fn a_listing_keeps_directories_first_and_stops_at_the_cap() {
        let dir = crate::test_support::Scratch::new("listdir-cap");
        std::fs::create_dir_all(dir.join("Zdir")).unwrap();
        std::fs::create_dir_all(dir.join("adir")).unwrap();
        for f in ["b.txt", "a.txt"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let listing = list_dir_capped(Some(dir.to_string_lossy().into_owned()), 16);
        let Event::DirListing {
            entries, omitted, ..
        } = listing
        else {
            panic!("expected a listing, got {listing:?}");
        };
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["adir", "Zdir", "a.txt", "b.txt"]);
        assert_eq!(omitted, 0, "a complete listing omits nothing");

        let Event::DirListing {
            entries, omitted, ..
        } = list_dir_capped(Some(dir.to_string_lossy().into_owned()), 2)
        else {
            panic!("expected a listing");
        };
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["adir", "Zdir"], "capped, directories kept first");
        assert_eq!(omitted, 2, "the listing says what it left out");
    }
}
