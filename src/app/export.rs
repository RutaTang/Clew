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
/// export covers). Identifiers are code spans and free text is escaped, so a
/// `__init__` or a `*ptr` reads as written.
pub(crate) fn render(x: &Export<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {} — reading notes\n\n", md(x.project)));
    out.push_str("Exported from clew.\n\n");

    out.push_str(&format!("## Notes ({})\n\n", x.notes.len()));
    if x.notes.is_empty() {
        out.push_str("_No notes._\n\n");
    }
    for n in x.notes {
        let mark = if n.understood { " ✓ understood" } else { "" };
        out.push_str(&format!(
            "- `{}` · `{}`{mark}\n",
            code(&n.rel),
            code(&n.symbol)
        ));
        for line in n.text.lines().filter(|l| !l.trim().is_empty()) {
            out.push_str(&format!("  > {}\n", md(line.trim_end())));
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
            out.push_str(&format!("- `{}:{}`\n", code(&b.rel), b.line));
        } else {
            out.push_str(&format!("- `{}:{}` — `{preview}`\n", code(&b.rel), b.line));
        }
        if let Some(note) = b.note.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            for line in note.lines() {
                out.push_str(&format!("  > {}\n", md(line.trim_end())));
            }
        }
    }
    out.push('\n');

    out.push_str(&format!("## Reading trail ({})\n\n", x.trail.len()));
    if x.trail.is_empty() {
        out.push_str("_No trail._\n\n");
    }
    // A straight run of visits is one list; it nests only where the trail
    // forks — where the reader went back and took another way — and each
    // way taken from a fork starts with `↳`.
    let mut ancestors: Vec<(usize, bool)> = Vec::new();
    for v in x.trail {
        while ancestors.last().is_some_and(|&(depth, _)| depth >= v.depth) {
            ancestors.pop();
        }
        let indent = ancestors.iter().filter(|&&(_, forks)| forks).count();
        let branch = if ancestors.last().is_some_and(|&(_, forks)| forks) {
            "↳ "
        } else {
            ""
        };
        ancestors.push((v.depth, v.forks));
        let rel = v
            .loc
            .path
            .strip_prefix(x.root)
            .unwrap_or(&v.loc.path)
            .to_string_lossy()
            .replace('\\', "/");
        let at = match v.loc.line {
            Some(line) => format!("{}:{line}", code(&rel)),
            None => code(&rel),
        };
        let label = v
            .label
            .as_deref()
            .map(|l| format!(" — `{}`", code(l)))
            .unwrap_or_default();
        let here = if v.is_current { " ← here" } else { "" };
        out.push_str(&format!(
            "{}- {branch}`{at}`{label}{here}\n",
            "  ".repeat(indent)
        ));
    }
    out.push('\n');

    out.push_str(&format!("## Walkthroughs ({})\n\n", x.tours.len()));
    if x.tours.is_empty() {
        out.push_str("_No walkthroughs._\n\n");
    }
    for t in x.tours {
        out.push_str(&format!("### {}\n\n", md(t.title.trim())));
        let scope = t.scope.trim();
        if !scope.is_empty() {
            out.push_str(&format!("_Scope: {}_\n\n", md(scope)));
        }
        for (i, s) in t.steps.iter().enumerate() {
            let mut at = code(&s.file);
            if let Some(line) = s.line {
                at.push_str(&format!(":{line}"));
            }
            let title = s.title.trim();
            // The step's symbol, unless the title already is it.
            let symbol = s
                .symbol
                .as_deref()
                .filter(|sym| !sym.is_empty() && *sym != title)
                .map(|sym| format!(" (`{}`)", code(sym)))
                .unwrap_or_default();
            out.push_str(&format!("{}. **{}** — `{at}`{symbol}\n", i + 1, md(title)));
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
            "- **`{}`** ({}) — {} · `{}:{}`\n",
            code(&t.name),
            t.badge,
            md(&t.definition),
            code(&t.rel),
            t.line
        ));
    }
    out
}

/// `s` fit for a Markdown code span: backticks dropped (they would end the
/// span), newlines flattened.
fn code(s: &str) -> String {
    s.replace('`', "").replace(['\n', '\r'], " ")
}

/// `s` as plain text in Markdown: every character Markdown reads as markup
/// (emphasis, links, headings, tables, HTML) escaped, newlines flattened.
fn md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' | '~' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// How long an export waits for the docs index the glossary is built from,
/// per check, and how many checks before it writes without it.
const DOCS_WAIT_STEP: std::time::Duration = std::time::Duration::from_millis(200);
pub(crate) const DOCS_WAITS: u32 = 50;

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
                // The glossary is built from the docs index, which is only
                // built on demand: start it while the reader picks a file.
                self.ensure_docs();
                let file_name = format!("{}-reading-notes.md", project_name(&root));
                // A local project's own folder to start in; a remote one's
                // root is a path on another machine.
                let folder = self.local_project_state().then_some(root);
                save_markdown(self.main_window, file_name, folder)
                    .map(|path| Message::Export(ExportMsg::Picked(path)))
            }
            ExportMsg::Picked(None) => Task::none(),
            ExportMsg::Picked(Some(path)) => self.export_when_ready(path, 0),
            ExportMsg::Ready { path, waited, .. } => self.export_when_ready(path, waited),
            ExportMsg::Written { path, result, .. } => {
                self.status = match result {
                    Ok(()) => format!("Exported reading notes to {}", path.display()),
                    Err(e) => format!("Export failed: {e}"),
                };
                Task::none()
            }
        }
    }

    /// Write the export to `path` once the docs index is in (its glossary
    /// section is built from it), checking again every [`DOCS_WAIT_STEP`];
    /// after [`DOCS_WAITS`] checks it is written with what there is.
    fn export_when_ready(&mut self, path: PathBuf, waited: u32) -> Task<Message> {
        if waited == 0 {
            self.ensure_docs();
        }
        if self.docs_loading() && waited < DOCS_WAITS {
            if waited == 0 {
                self.status = "Exporting — waiting for the docs index…".into();
            }
            let stamp = self.stamp();
            // Created where it is polled: a timer needs the runtime's context.
            return Task::perform(
                async { tokio::time::sleep(DOCS_WAIT_STEP).await },
                move |()| {
                    Message::Export(ExportMsg::Ready {
                        stamp: stamp.clone(),
                        path: path.clone(),
                        waited: waited + 1,
                    })
                },
            );
        }
        let Some(markdown) = self.export_markdown() else {
            return Task::none();
        };
        let stamp = self.stamp();
        Task::perform(
            async move {
                let write_path = path.clone();
                let result = tokio::task::spawn_blocking(move || {
                    std::fs::write(&write_path, markdown.as_bytes()).map_err(|e| e.to_string())
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

- `src/lib.rs` · `parse` ✓ understood
  > tricky
  > second \\`line\\`
- `src/main.rs` · `main`

## Bookmarks (2)

- `src/lib.rs:7` — `let x = 1;`
  > why?
- `src/lib.rs:9`

## Reading trail (2)

- `src/lib.rs:3` — `parse`
- `src/util.rs` ← here

## Walkthroughs (1)

### Startup

_Scope: boot_

1. **Entry** — `src/main.rs:1` (`main`)
   Where it starts.
   Two lines.

## Glossary (1)

- **`Parser`** (struct) — Reads tokens · `src/lib.rs:2`
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

    /// A straight run of visits stays one list; the trail nests only where
    /// the reader went back and took another way, each way marked `↳`. A
    /// title with Markdown in it reads as written, and a step's symbol is
    /// not repeated when it is the title.
    #[test]
    fn the_trail_nests_at_forks_and_text_is_escaped() {
        let root = PathBuf::from("/p");
        let at = |rel: &str| Loc {
            path: root.join(rel),
            line: Some(1),
        };
        let mut history = History::default();
        history.push(at("a.rs"), None);
        history.push(at("b.rs"), None);
        history.push(at("d.rs"), None);
        history.back();
        history.back();
        history.push(at("c.rs"), Some("__init__".into()));
        let trail = history.flatten();
        let tours = vec![Walkthrough {
            title: "__init__ and *ptr".into(),
            scope: String::new(),
            steps: vec![walkthrough::Step {
                title: "__init__".into(),
                file: "a.rs".into(),
                symbol: Some("__init__".into()),
                line: Some(1),
                narration: String::new(),
            }],
        }];
        let glossary = Glossary::default();
        let md = render(&Export {
            project: "p",
            root: &root,
            notes: &[],
            bookmarks: &[],
            trail: &trail,
            tours: &tours,
            glossary: &glossary,
        });
        assert!(
            md.contains(
                "- `a.rs:1`\n  - ↳ `b.rs:1`\n  - `d.rs:1`\n  - ↳ `c.rs:1` — `__init__` ← here\n"
            ),
            "{md}"
        );
        assert!(md.contains("### \\_\\_init\\_\\_ and \\*ptr\n"), "{md}");
        assert!(md.contains("1. **\\_\\_init\\_\\_** — `a.rs:1`\n"), "{md}");
    }

    #[test]
    fn the_project_is_named_after_its_root_folder() {
        assert_eq!(project_name(Path::new("/tmp/clew")), "clew");
        assert_eq!(project_name(Path::new("/")), "project");
    }
}
