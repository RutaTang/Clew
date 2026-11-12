//! The context menu's language action: what the language itself can say
//! about the code under the cursor that no other feature does — a macro's
//! expansion (rust-analyzer's for Rust; Clew's own reading of the
//! `#define`s in effect for C and C++, on the project's host for a remote
//! one), the goals of a Lean proof there, the type of a Haskell name —
//! shown in a popup, with a way to the macro's definition.
//!
//! Its messages are [`HoverMsg`]'s `Insight*`, through `App::update_hover`.

use clew_core::lsp::client::{ExpandedMacro, LeanGoal, MemoryLayout, RelatedTest, hover_parts};

use crate::app::prelude::*;
use crate::*;

impl App {
    /// The context menu's language action — its own (`None`), or `kind`,
    /// one of its others — at the menu's position.
    pub(crate) fn on_insight_from_menu(&mut self, kind: Option<InsightKind>) -> Task<Message> {
        let Some(menu) = self.proj.context_menu.take() else {
            return Task::none();
        };
        let Some(kind) = kind.or(menu.insight) else {
            return Task::none();
        };
        let task = self.start_insight(kind, menu.pane, menu.line, menu.col);
        // Opened where it was asked from.
        if let Some(insight) = self.proj.insight.as_mut() {
            insight.anchor = menu.by_pointer.then_some(menu.y);
        }
        task
    }

    /// The same at the active pane's caret (`ge`), for its language.
    pub(crate) fn insight_at_cursor(&mut self) -> Task<Message> {
        let pane = self.proj.active;
        let Some((kind, (line, col))) = self
            .active_viewer()
            .and_then(|v| Some((InsightKind::for_lang(v.lang_key?)?, v.caret?)))
        else {
            return Task::none();
        };
        self.start_insight(kind, pane, line, col)
    }

    /// Ask the language about display column `col` of 0-based `line` in
    /// `pane`, opening the popup to wait for the answer.
    pub(crate) fn start_insight(
        &mut self,
        kind: InsightKind,
        pane: usize,
        line: usize,
        col: usize,
    ) -> Task<Message> {
        let Some(v) = self.proj.panes.get(pane).and_then(Option::as_ref) else {
            return Task::none();
        };
        let Some(lang) = v.lang_key else {
            return Task::none();
        };
        let abs = v.abs.clone();
        let rel = v.rel.clone();
        let source_line = v.source_line(line).unwrap_or("").to_string();
        let word = analyze::word_at(&v.lines, line, col);
        self.insight_seq += 1;
        let token = self.insight_seq;
        let title = match (kind, word) {
            (InsightKind::ExpandMacro, Some(w)) => tr!("Expansion of {name}", name = w),
            (InsightKind::ExpandMacro, None) => tr!("Line {line} expanded", line = line + 1),
            (InsightKind::GoalState, _) => tr!("Goal at line {line}", line = line + 1),
            (InsightKind::TypeAtPoint, Some(w)) => tr!("Type of {name}", name = w),
            (InsightKind::TypeAtPoint, None) => tr!("Type at line {line}", line = line + 1),
            (InsightKind::MemoryLayout, Some(w)) => tr!("Memory layout of {name}", name = w),
            (InsightKind::MemoryLayout, None) => {
                tr!("Memory layout at line {line}", line = line + 1)
            }
            (InsightKind::RelatedTests, Some(w)) => tr!("Tests of {name}", name = w),
            (InsightKind::RelatedTests, None) => tr!("Tests at line {line}", line = line + 1),
        };
        self.proj.insight = Some(Insight {
            token,
            kind,
            title,
            loading: true,
            prepared: Vec::new(),
            defined: None,
            anchor: None,
        });
        let stamp = self.stamp();
        let done = move |result| {
            Message::Hover(HoverMsg::InsightAnswered {
                stamp: stamp.clone(),
                token,
                result,
            })
        };
        // C and C++: Clew's own reading of the `#define`s, where the
        // headers are.
        if kind == InsightKind::ExpandMacro && matches!(lang, "c" | "cpp") {
            let column =
                viewer::Col(col).to_offset(&source_line, lsp::client::PositionEncoding::Utf8);
            let target = self.target_spec();
            if !self.local_project_state() {
                let ai = self.ai_client();
                let request = clew_protocol::Request::ExpandMacro {
                    rel,
                    line,
                    column,
                    target,
                };
                return Task::perform(
                    async move {
                        match ai.request(request).await? {
                            clew_protocol::Event::MacroExpansion { expansion, .. } => {
                                Ok(expansion.map(c_answer))
                            }
                            other => Err(format!(
                                "unexpected reply to ExpandMacro: {}",
                                crate::app::rpc::event_name(&other)
                            )),
                        }
                    },
                    done,
                );
            }
            let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                return Task::none();
            };
            let target: clew_core::inactive::Target = target.into();
            return Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        clew_core::cmacro::expand_in_project(&root, &rel, line, column, &target)
                    })
                    .await
                    .map_err(|_| tr!("expanding the macro failed unexpectedly").to_string())?
                    .map(|e| e.map(c_answer))
                },
                done,
            );
        }
        // The rest is the language server's to say.
        let server = match kind {
            InsightKind::ExpandMacro | InsightKind::MemoryLayout | InsightKind::RelatedTests => {
                "rust-analyzer"
            }
            InsightKind::GoalState => tr!("The Lean server"),
            InsightKind::TypeAtPoint => "haskell-language-server",
        };
        let client = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            Some(LspSlot::Starting | LspSlot::AwaitingConsent) => {
                return self.on_insight_answered(
                    token,
                    Err(tr!(
                        "{server} is still starting — ask again once it is ready",
                        server = server
                    )),
                );
            }
            _ => {
                return self.on_insight_answered(
                    token,
                    Err(tr!(
                        "{server} is not running — start it from the language servers panel",
                        server = server
                    )),
                );
            }
        };
        let character = viewer::Col(col).to_offset(&source_line, client.encoding);
        // Where the tests' paths are read from, to link them.
        let root = self.proj.project.as_ref().map(|p| p.root.clone());
        Task::perform(
            async move {
                match kind {
                    InsightKind::ExpandMacro => client
                        .expand_macro(&abs, line, character)
                        .await
                        .map(|m| m.map(rust_answer)),
                    InsightKind::GoalState => client
                        .lean_goal(&abs, line, character)
                        .await
                        .map(|g| g.map(lean_answer)),
                    InsightKind::TypeAtPoint => client
                        .hover(&abs, line, character)
                        .await
                        .map(|h| h.as_deref().and_then(haskell_answer)),
                    InsightKind::MemoryLayout => client
                        .memory_layout(&abs, line, character)
                        .await
                        .map(|m| m.map(|m| layout_answer(&m))),
                    InsightKind::RelatedTests => client
                        .related_tests(&abs, line, character)
                        .await
                        .map(|t| tests_answer(&t, root.as_deref())),
                }
            },
            done,
        )
    }

    /// The language's answer to request `token`: shown when the popup still
    /// waits for it, dropped when it was closed or asked again since.
    pub(crate) fn on_insight_answered(
        &mut self,
        token: u64,
        result: Result<Option<InsightAnswer>, String>,
    ) -> Task<Message> {
        let Some(kind) = self
            .proj
            .insight
            .as_ref()
            .filter(|i| i.token == token)
            .map(|i| i.kind)
        else {
            return Task::none();
        };
        let (markdown, defined) = match result {
            Ok(Some(answer)) => (answer.markdown, answer.defined),
            Ok(None) => (format!("*{}*", kind.nothing()), None),
            Err(e) => (
                format!("*{}*", tr!("Couldn't ask: {error}", error = e)),
                None,
            ),
        };
        let (prepared, task) = self.prepare_segments(&markdown);
        if let Some(open) = self.proj.insight.as_mut() {
            open.loading = false;
            open.prepared = prepared;
            open.defined = defined;
        }
        task
    }

    /// Open where the popup's answer points, closing it.
    pub(crate) fn on_insight_open_definition(&mut self) -> Task<Message> {
        let Some((rel, line)) = self.proj.insight.take().and_then(|i| i.defined) else {
            return Task::none();
        };
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        self.open_file(root.join(rel), Some(line.max(1)), true)
    }
}

/// rust-analyzer's expansion, as the popup shows it.
pub(crate) fn rust_answer(m: ExpandedMacro) -> InsightAnswer {
    InsightAnswer {
        // As rust-analyzer names it: `twice!` for a call, the derive's
        // name for a derive.
        markdown: format!(
            "{}\n\n```rust\n{}\n```",
            tr!("{name} expands to", name = format!("`{}`", m.name)),
            m.expansion.trim_end()
        ),
        defined: None,
    }
}

/// rust-analyzer's memory layout, as the popup shows it: the type's size
/// and alignment and the padding in it, then each field where rustc put it
/// — not where the source declares it — and the gaps between them.
pub(crate) fn layout_answer(layout: &MemoryLayout) -> InsightAnswer {
    let Some(root) = layout.nodes.first() else {
        return InsightAnswer {
            markdown: String::new(),
            defined: None,
        };
    };
    let padding = layout.padding();
    let mut md = format!(
        "**`{}`** — {}",
        root.typename,
        trn!(
            "1 byte, aligned to {align}",
            "{n} bytes, aligned to {align}",
            root.size as usize,
            align = root.alignment
        )
    );
    if padding > 0 {
        md.push_str(", ");
        md.push_str(&trn!(
            "1 byte of padding",
            "{n} bytes of padding",
            padding as usize
        ));
    }
    let fields = layout.fields();
    if fields.is_empty() {
        return InsightAnswer {
            markdown: md,
            defined: None,
        };
    }
    // A table in a code block: its columns stay lined up in any font the
    // popup's text has.
    let name_w = fields
        .iter()
        .map(|f| f.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(tr!("(padding)").chars().count());
    let row = |offset: String, size: String, name: &str, ty: &str| {
        format!(
            "{offset:>6}  {size:>4}  {name:<width$}  {ty}",
            width = name_w
        )
        .trim_end()
        .to_string()
    };
    let mut lines = vec![row(
        tr!("offset").into(),
        tr!("size").into(),
        tr!("field"),
        tr!("type"),
    )];
    let mut end = 0;
    for field in &fields {
        if field.offset > end {
            lines.push(row(
                end.to_string(),
                (field.offset - end).to_string(),
                tr!("(padding)"),
                "",
            ));
        }
        lines.push(row(
            field.offset.to_string(),
            field.size.to_string(),
            &field.name,
            &field.typename,
        ));
        end = end.max(field.offset + field.size);
    }
    if root.size > end {
        lines.push(row(
            end.to_string(),
            (root.size - end).to_string(),
            tr!("(padding)"),
            "",
        ));
    }
    md.push_str(&format!("\n\n```text\n{}\n```", lines.join("\n")));
    InsightAnswer {
        markdown: md,
        defined: None,
    }
}

/// rust-analyzer's related tests, as the popup shows them: each a link to
/// where it is in the project; `None` when there are none.
pub(crate) fn tests_answer(tests: &[RelatedTest], root: Option<&Path>) -> Option<InsightAnswer> {
    if tests.is_empty() {
        return None;
    }
    let lines: Vec<String> = tests
        .iter()
        .map(|t| {
            match root.and_then(|r| t.path.strip_prefix(r).ok()) {
                Some(rel) => format!(
                    "- [`{}`](clew:{}:{})",
                    t.name,
                    clew_core::rel_string(rel),
                    t.line + 1
                ),
                // Out of the project (a dependency's): named, not linked.
                None => format!("- `{}`", t.name),
            }
        })
        .collect();
    Some(InsightAnswer {
        markdown: format!(
            "{}\n\n{}",
            trn!(
                "1 test exercises this:",
                "{n} tests exercise this:",
                tests.len()
            ),
            lines.join("\n")
        ),
        defined: None,
    })
}

/// A C or C++ expansion: what was expanded, what it became, the
/// `#define` and where it is.
pub(crate) fn c_answer(e: clew_protocol::MacroExpansion) -> InsightAnswer {
    let mut md = format!(
        "```c\n{}\n```\n\n{}\n\n```c\n{}\n```",
        e.invocation,
        tr!("expands to"),
        e.expanded
    );
    if !e.definition.is_empty() {
        md.push_str(&format!(
            "\n\n{}\n\n```c\n{}\n```",
            tr!("from this definition"),
            e.definition
        ));
    }
    if !e.file.is_empty() {
        md.push_str(&format!(
            "\n\n{}",
            tr!(
                "Defined at {location}.",
                location = format!("`{}:{}`", e.file, e.line)
            )
        ));
    } else if !e.name.is_empty() {
        md.push_str(&format!("\n\n{}", tr!("Defined by the compile command.")));
    }
    if e.truncated {
        md.push_str(&format!(
            "\n\n*{}*",
            tr!("Cut short: the expansion grew past Clew's limit.")
        ));
    }
    InsightAnswer {
        markdown: md,
        defined: (!e.file.is_empty()).then_some((e.file, e.line)),
    }
}

/// The Lean server's goals, each in its own block.
pub(crate) fn lean_answer(goal: LeanGoal) -> InsightAnswer {
    let markdown = match goal {
        LeanGoal::Tactic(goals) if goals.is_empty() => {
            format!("*{}*", tr!("No goals: the proof is complete here."))
        }
        LeanGoal::Tactic(goals) => {
            let n = goals.len();
            goals
                .iter()
                .enumerate()
                .map(|(i, g)| match n {
                    1 => format!("```lean\n{g}\n```"),
                    _ => format!(
                        "**{}**\n\n```lean\n{g}\n```",
                        tr!("Goal {i} of {n}", i = i + 1, n = n)
                    ),
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        LeanGoal::Term(goal) => format!("{}\n\n```lean\n{goal}\n```", tr!("Expected type")),
    };
    InsightAnswer {
        markdown,
        defined: None,
    }
}

/// haskell-language-server's hover: the declared type in a block, the
/// type at this use when it is another (`print` given an `Int`), then what
/// else it says (where the name is defined, its docs).
pub(crate) fn haskell_answer(hover: &str) -> Option<InsightAnswer> {
    let parts = hover_parts(hover)?;
    let mut markdown = format!("```haskell\n{}\n```", parts.signature);
    if let Some(here) = parts
        .here
        .filter(|h| !parts.signature.ends_with(&format!(":: {h}")))
    {
        markdown.push_str(&format!(
            "\n\n{}",
            tr!("At this use: {here}", here = format!("`{here}`"))
        ));
    }
    if !parts.prose.is_empty() {
        markdown.push_str("\n\n");
        markdown.push_str(&parts.prose);
    }
    Some(InsightAnswer {
        markdown,
        defined: None,
    })
}
