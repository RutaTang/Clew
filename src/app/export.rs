//! Exporting what a reader has gathered — the notes, bookmarks, reading
//! trail, saved walkthroughs and glossary of the open project — as one
//! Markdown document, to keep, hand over or paste into a review. Everything
//! in it is the reader's own record; nothing is generated for the export.

use crate::app::prelude::*;
use crate::*;

use crate::app::glossary::Glossary;
use crate::bookmarks::Bookmark;
use crate::history::Visit;
use crate::notes::Note;
use crate::walkthrough::Walkthrough;

/// What one export renders (borrowed from the session at the moment the
/// reader picks where to save).
pub(crate) struct Export<'a> {
    /// The project's name (its root folder's).
    pub(crate) project: &'a str,
    /// The project root, to spell the trail's absolute paths relative.
    pub(crate) root: &'a Path,
    pub(crate) notes: &'a [Note],
    pub(crate) bookmarks: &'a [Bookmark],
    pub(crate) trail: &'a [Visit],
    pub(crate) tours: &'a [Walkthrough],
    pub(crate) glossary: &'a Glossary,
}

/// The Markdown document for `x`. Sections in reading order — notes,
/// bookmarks, trail, walkthroughs, glossary — each with its count, an empty
/// one saying so rather than vanishing (so the reader can see what the
/// export covers).
pub(crate) fn render(x: &Export<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {} — reading notes\n\n", x.project));
    out.push_str("Exported from clew.\n\n");

    out.push_str(&format!("## Notes ({})\n\n", x.notes.len()));
    if x.notes.is_empty() {
        out.push_str("_No notes._\n\n");
    }
    for n in x.notes {
        let mark = if n.understood { " ✓ understood" } else { "" };
        out.push_str(&format!("- `{}` · **{}**{mark}\n", n.rel, code(&n.symbol)));
        for line in n.text.lines().filter(|l| !l.trim().is_empty()) {
            out.push_str(&format!("  > {}\n", line.trim_end()));
        }
    }
    out.push('\n');

    out.push_str(&format!("## Bookmarks ({})\n\n", x.bookmarks.len()));
    if x.bookmarks.is_empty() {
        out.push_str("_No bookmarks._\n\n");
    }
    for b in x.bookmarks {
        let preview = code(b.preview.trim());
        if preview.is_empty() {
            out.push_str(&format!("- `{}:{}`\n", b.rel, b.line));
        } else {
            out.push_str(&format!("- `{}:{}` — `{preview}`\n", b.rel, b.line));
        }
        if let Some(note) = b.note.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            for line in note.lines() {
                out.push_str(&format!("  > {}\n", line.trim_end()));
            }
        }
    }
    out.push('\n');

    out.push_str(&format!("## Reading trail ({})\n\n", x.trail.len()));
    if x.trail.is_empty() {
        out.push_str("_No trail._\n\n");
    }
    for v in x.trail {
        let rel = v
            .loc
            .path
            .strip_prefix(x.root)
            .unwrap_or(&v.loc.path)
            .to_string_lossy()
            .replace('\\', "/");
        let at = match v.loc.line {
            Some(line) => format!("{rel}:{line}"),
            None => rel,
        };
        let label = v
            .label
            .as_deref()
            .map(|l| format!(" — {}", code(l)))
            .unwrap_or_default();
        let here = if v.is_current { " ← here" } else { "" };
        out.push_str(&format!("{}- `{at}`{label}{here}\n", "  ".repeat(v.depth)));
    }
    out.push('\n');

    out.push_str(&format!("## Walkthroughs ({})\n\n", x.tours.len()));
    if x.tours.is_empty() {
        out.push_str("_No walkthroughs._\n\n");
    }
    for t in x.tours {
        out.push_str(&format!("### {}\n\n", t.title.trim()));
        let scope = t.scope.trim();
        if !scope.is_empty() {
            out.push_str(&format!("_Scope: {scope}_\n\n"));
        }
        for (i, s) in t.steps.iter().enumerate() {
            let mut at = s.file.clone();
            if let Some(line) = s.line {
                at.push_str(&format!(":{line}"));
            }
            let symbol = s
                .symbol
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!(" ({})", code(s)))
                .unwrap_or_default();
            out.push_str(&format!(
                "{}. **{}** — `{at}`{symbol}\n",
                i + 1,
                s.title.trim()
            ));
            for line in s.narration.lines() {
                out.push_str(&format!("   {}\n", line.trim_end()));
            }
            out.push('\n');
        }
    }

    let terms = x.glossary.terms();
    out.push_str(&format!("## Glossary ({})\n\n", terms.len()));
    if terms.is_empty() {
        out.push_str("_No terms._\n");
    }
    for t in terms {
        out.push_str(&format!(
            "- **{}** ({}) — {} · `{}:{}`\n",
            code(&t.name),
            t.badge,
            t.definition,
            t.rel,
            t.line
        ));
    }
    out
}

/// `s` fit for a Markdown code span or a bold run: backticks dropped (they
/// would end the span), newlines flattened.
fn code(s: &str) -> String {
    s.replace('`', "").replace(['\n', '\r'], " ")
}

impl App {
    /// The export of the open project's reading state, as rendered now;
    /// `None` with no project open.
    pub(crate) fn export_markdown(&self) -> Option<String> {
        let project = self.proj.project.as_ref()?;
        let glossary = self.glossary();
        let name = project_name(&project.root);
        Some(render(&Export {
            project: &name,
            root: &project.root,
            notes: &self.proj.notes,
            bookmarks: &self.proj.bookmarks,
            trail: &self.proj.history.flatten(),
            tours: &self.proj.walk.library,
            glossary: &glossary,
        }))
    }

    /// Handle an [`ExportMsg`]: this feature's share of what `dispatch`
    /// routes.
    pub(crate) fn update_export(&mut self, message: ExportMsg) -> Task<Message> {
        match message {
            ExportMsg::Start => {
                let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                    self.status = "Open a project to export its reading notes".into();
                    return Task::none();
                };
                let file_name = format!("{}-reading-notes.md", project_name(&root));
                save_markdown(self.main_window, file_name)
                    .map(|path| Message::Export(ExportMsg::Picked(path)))
            }
            ExportMsg::Picked(None) => Task::none(),
            ExportMsg::Picked(Some(path)) => {
                let Some(markdown) = self.export_markdown() else {
                    return Task::none();
                };
                let stamp = self.stamp();
                Task::perform(
                    async move {
                        let write_path = path.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            std::fs::write(&write_path, markdown.as_bytes())
                                .map_err(|e| e.to_string())
                        })
                        .await
                        .unwrap_or_else(|e| Err(format!("the write failed unexpectedly: {e}")));
                        (path, result)
                    },
                    move |(path, result)| {
                        Message::Export(ExportMsg::Written {
                            stamp: stamp.clone(),
                            path,
                            result,
                        })
                    },
                )
            }
            ExportMsg::Written { path, result, .. } => {
                self.status = match result {
                    Ok(()) => format!("Exported reading notes to {}", path.display()),
                    Err(e) => format!("Export failed: {e}"),
                };
                Task::none()
            }
        }
    }
}

/// The project's display name: its root folder's name.
fn project_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "project".into())
}

#[cfg(test)]
mod export_tests {
    use super::*;
    use crate::history::{History, Loc};

    #[test]
    fn every_section_is_rendered_with_its_count() {
        let root = PathBuf::from("/p/demo");
        let notes = vec![
            Note {
                rel: "src/lib.rs".into(),
                symbol: "parse".into(),
                understood: true,
                text: "tricky\n\nsecond `line`".into(),
            },
            Note {
                rel: "src/main.rs".into(),
                symbol: "main".into(),
                understood: false,
                text: String::new(),
            },
        ];
        let bookmarks = vec![
            Bookmark {
                rel: "src/lib.rs".into(),
                line: 7,
                preview: "let `x` = 1;".into(),
                note: Some("why?".into()),
            },
            Bookmark {
                rel: "src/lib.rs".into(),
                line: 9,
                preview: "  ".into(),
                note: None,
            },
        ];
        let mut history = History::default();
        history.push(
            Loc {
                path: root.join("src/lib.rs"),
                line: Some(3),
            },
            Some("parse".into()),
        );
        history.push(
            Loc {
                path: root.join("src/util.rs"),
                line: None,
            },
            None,
        );
        let trail = history.flatten();
        let tours = vec![Walkthrough {
            title: "Startup".into(),
            scope: "boot".into(),
            steps: vec![walkthrough::Step {
                title: "Entry".into(),
                file: "src/main.rs".into(),
                symbol: Some("main".into()),
                line: Some(1),
                narration: "Where it starts.\nTwo lines.".into(),
            }],
        }];
        let mut glossary_files = vec![clew_protocol::DocFile {
            rel: "src/lib.rs".into(),
            items: vec![clew_protocol::DocItem {
                name: "Parser".into(),
                kind: "struct".into(),
                signature: "struct Parser".into(),
                doc: "Reads tokens.".into(),
                line: 2,
                public: true,
                children: Vec::new(),
                refs: Vec::new(),
            }],
        }];
        let glossary = Glossary::build(&glossary_files, &explain::Cache::new(), Some(&root));
        let md = render(&Export {
            project: "demo",
            root: &root,
            notes: &notes,
            bookmarks: &bookmarks,
            trail: &trail,
            tours: &tours,
            glossary: &glossary,
        });
        let expected = "\
# demo — reading notes

Exported from clew.

## Notes (2)

- `src/lib.rs` · **parse** ✓ understood
  > tricky
  > second `line`
- `src/main.rs` · **main**

## Bookmarks (2)

- `src/lib.rs:7` — `let x = 1;`
  > why?
- `src/lib.rs:9`

## Reading trail (2)

- `src/lib.rs:3` — parse
  - `src/util.rs` ← here

## Walkthroughs (1)

### Startup

_Scope: boot_

1. **Entry** — `src/main.rs:1` (main)
   Where it starts.
   Two lines.

## Glossary (1)

- **Parser** (struct) — Reads tokens · `src/lib.rs:2`
";
        assert_eq!(md, expected);

        // Empty everything: the sections stay, each saying it is empty.
        glossary_files.clear();
        let empty = Glossary::build(&glossary_files, &explain::Cache::new(), Some(&root));
        let md = render(&Export {
            project: "demo",
            root: &root,
            notes: &[],
            bookmarks: &[],
            trail: &[],
            tours: &[],
            glossary: &empty,
        });
        for section in [
            "## Notes (0)\n\n_No notes._",
            "## Bookmarks (0)\n\n_No bookmarks._",
            "## Reading trail (0)\n\n_No trail._",
            "## Walkthroughs (0)\n\n_No walkthroughs._",
            "## Glossary (0)\n\n_No terms._",
        ] {
            assert!(md.contains(section), "missing {section:?} in:\n{md}");
        }
    }

    #[test]
    fn the_project_is_named_after_its_root_folder() {
        assert_eq!(project_name(Path::new("/tmp/clew")), "clew");
        assert_eq!(project_name(Path::new("/")), "project");
    }
}
