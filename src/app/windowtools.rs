//! The window's own tools, lent to clew's Ask agent beside its code tools
//! and the user's MCP servers': with them it can do in the window what it
//! talks about — open a file at a line, go to a symbol's definition, and run
//! any of the window's commands (the command palette's), so "show me the
//! call graph of this" or "take me to where the config is parsed", said or
//! typed, is done rather than described.
//!
//! They run here, in the client, as the MCP tools do (`AgentToolCall` in,
//! `AgentToolResult` out). Opening and showing need no say-so: they only
//! change what is on screen. A command that costs something — a paid pass,
//! starting or stepping the program, another folder opened — or changes
//! what the microphone takes in or where code is sent
//! ([`PaletteCommand::asks_the_agent`]) waits for the user's approval on a
//! card, as an MCP call does.
//!
//! [`PaletteCommand::asks_the_agent`]: crate::app::palette::PaletteCommand::asks_the_agent

use clew_protocol::ExternalTool;
use serde_json::{Value, json};

use crate::app::integrations::call_deadline;
use crate::app::prelude::*;
use crate::*;

/// The tools' names, as the model knows them: unlike any of the server's
/// own, and of any MCP server's (those begin `mcp__`).
pub(crate) const OPEN_FILE: &str = "window_open_file";
pub(crate) const GO_TO_SYMBOL: &str = "window_go_to_symbol";
pub(crate) const LIST_COMMANDS: &str = "window_list_commands";
pub(crate) const RUN_COMMAND: &str = "window_run_command";

/// Most symbols a go-to lists when the name is ambiguous.
const MAX_CANDIDATES: usize = 8;

/// A window command the agent asked to run, waiting for the user's
/// approval.
#[derive(Debug, Clone)]
pub struct PendingCommand {
    pub title: String,
    /// When the turn stops waiting for its result.
    pub deadline: std::time::Instant,
}

/// Whether `name` is one of the window's tools.
pub(crate) fn is_window_tool(name: &str) -> bool {
    matches!(name, OPEN_FILE | GO_TO_SYMBOL | LIST_COMMANDS | RUN_COMMAND)
}

/// The window's tools, as an agent turn is offered them.
pub(crate) fn window_tools() -> Vec<ExternalTool> {
    vec![
        ExternalTool {
            name: OPEN_FILE.into(),
            description: "[The user's Clew window] Open a file of the project in the user's \
                          editor — at a line, when given — so they see it. Use it when the \
                          user asks you to open, show or take them to code; it shows the code \
                          to them, while `read` shows it to you."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file, relative to the project root."
                    },
                    "line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The 1-based line to show and mark."
                    }
                },
                "required": ["path"]
            }),
        },
        ExternalTool {
            name: GO_TO_SYMBOL.into(),
            description: "[The user's Clew window] Open the definition of a function, type, \
                          method or other symbol of the project, by name, in the user's \
                          editor. When several share the name, the first is opened and the \
                          rest are listed — call it with `file` to pick one."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The symbol's name, as written in the code."
                    },
                    "file": {
                        "type": "string",
                        "description": "Only a definition in this file (relative path)."
                    }
                },
                "required": ["name"]
            }),
        },
        ExternalTool {
            name: LIST_COMMANDS.into(),
            description: "[The user's Clew window] List the commands the user's window \
                          offers right now — its command palette: views and graphs, panels, \
                          sidebar tabs, what can be done at the caret, the debugger's \
                          controls, the theme — with their shortcuts. Call it before \
                          `window_run_command`."
                .into(),
            parameters: json!({ "type": "object", "properties": {} }),
        },
        ExternalTool {
            name: RUN_COMMAND.into(),
            description: "[The user's Clew window] Run one of the window's commands, by its \
                          exact title from `window_list_commands` — to show the user a view \
                          (\"Call Graph\", \"Overview\"), a panel or a sidebar tab, change \
                          what is shown, or act at the caret. Commands that cost something \
                          (a paid explain pass, starting or stepping the program, opening \
                          another folder) ask the user first."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "The command's title, exactly as listed."
                    }
                },
                "required": ["title"]
            }),
        },
    ]
}

/// A string argument of a tool call.
fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

impl App {
    /// The agent turn on `stream` calls window tool `tool`.
    pub(crate) fn on_window_tool(
        &mut self,
        stream: u64,
        call: u64,
        tool: String,
        args: Value,
    ) -> Task<Message> {
        // Only the turn running now: a call of a stopped one crossed the
        // Cancel on its way, and is done nothing about.
        let live = self.proj.link.agent_stream == Some(stream)
            && self
                .proj
                .ask_turns
                .iter()
                .any(|t| t.stream == stream && t.streaming);
        if !live {
            self.window_tool_result(stream, call, "This turn has ended.".into(), true);
            return Task::none();
        }
        match tool.as_str() {
            OPEN_FILE => self.tool_open_file(stream, call, &args),
            GO_TO_SYMBOL => self.tool_go_to_symbol(stream, call, &args),
            LIST_COMMANDS => {
                let listed = self
                    .palette_commands()
                    .iter()
                    .map(|c| {
                        let mut line = format!("- {}", c.title);
                        if let Some(keys) = &c.keys {
                            line.push_str(&format!(" ({keys})"));
                        }
                        if c.asks_the_agent() {
                            line.push_str(" — asks the user first");
                        }
                        line
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                self.push_window_step(stream, tr!("Listed the window's commands"), None);
                self.window_tool_result(
                    stream,
                    call,
                    format!("The window's commands now:\n{listed}"),
                    false,
                );
                Task::none()
            }
            RUN_COMMAND => self.tool_run_command(stream, call, &args),
            _ => {
                self.window_tool_result(
                    stream,
                    call,
                    format!("`{tool}` is not a tool this turn was given."),
                    true,
                );
                Task::none()
            }
        }
    }

    /// The result of window call `call`, back to the turn.
    fn window_tool_result(&mut self, stream: u64, call: u64, content: String, is_error: bool) {
        let _ = self.send_to_server(clew_protocol::Request::AgentToolResult {
            stream,
            call,
            content,
            is_error,
        });
    }

    /// A step chip for what a window tool did, pointing where it went.
    fn push_window_step(&mut self, stream: u64, title: &str, at: Option<(String, usize)>) {
        if let Some(turn) = self.proj.ask_turns.iter_mut().find(|t| t.stream == stream) {
            turn.steps.push(AgentStep {
                tool: "window".into(),
                title: title.to_string(),
                refs: at
                    .map(|(rel, line)| (rel, Some(line)))
                    .into_iter()
                    .collect(),
                state: StepState::Done,
            });
        }
    }

    fn tool_open_file(&mut self, stream: u64, call: u64, args: &Value) -> Task<Message> {
        let Some(path) = arg(args, "path") else {
            self.window_tool_result(stream, call, "Give the file's `path`.".into(), true);
            return Task::none();
        };
        let line = args
            .get("line")
            .and_then(Value::as_u64)
            .map(|l| (l as usize).max(1));
        // Only a file the project lists: never a path climbing out of it.
        let rel = path.trim_start_matches("./").replace('\\', "/");
        let found = self
            .proj
            .project
            .as_ref()
            .and_then(|p| p.files.iter().find(|f| f.rel == rel))
            .map(|f| f.abs.clone());
        let Some(abs) = found else {
            self.window_tool_result(
                stream,
                call,
                format!(
                    "`{rel}` is not a file of the project. Give a path relative to the \
                     project root, as `search` and `read` show them."
                ),
                true,
            );
            return Task::none();
        };
        let at = match line {
            Some(l) => format!("{rel}:{l}"),
            None => rel.clone(),
        };
        self.push_window_step(
            stream,
            &tr!("Opened `{file}`", file = at),
            Some((rel, line.unwrap_or(1))),
        );
        self.window_tool_result(stream, call, format!("Opened {at} for the user."), false);
        self.open_file(abs, line, true)
    }

    fn tool_go_to_symbol(&mut self, stream: u64, call: u64, args: &Value) -> Task<Message> {
        let Some(name) = arg(args, "name").map(str::to_string) else {
            self.window_tool_result(stream, call, "Give the symbol's `name`.".into(), true);
            return Task::none();
        };
        let file = arg(args, "file").map(|f| f.trim_start_matches("./").replace('\\', "/"));
        let symbols = self.fresh_symbol_index();
        let in_file = |s: &&index::SymbolEntry| file.as_ref().is_none_or(|f| s.rel == *f);
        // Exact first, then regardless of case; definitions in tests last.
        let mut found: Vec<&index::SymbolEntry> = symbols
            .iter()
            .filter(|s| s.name == name)
            .filter(in_file)
            .collect();
        if found.is_empty() {
            found = symbols
                .iter()
                .filter(|s| s.name.eq_ignore_ascii_case(&name))
                .filter(in_file)
                .collect();
        }
        found.sort_by_key(|s| s.is_test);
        let Some(first) = found.first().copied() else {
            self.window_tool_result(
                stream,
                call,
                format!(
                    "No symbol named `{name}`{} in the project's index. `search` for it \
                     instead, then open the file with `{OPEN_FILE}`.",
                    file.as_deref()
                        .map(|f| format!(" in `{f}`"))
                        .unwrap_or_default()
                ),
                true,
            );
            return Task::none();
        };
        let (abs, rel, line) = (first.abs.clone(), first.rel.clone(), first.line);
        let mut content = format!("Opened `{}` ({}) at {rel}:{line}.", first.name, first.kind);
        if found.len() > 1 {
            let others: Vec<String> = found[1..]
                .iter()
                .take(MAX_CANDIDATES)
                .map(|s| format!("- {} ({}) at {}:{}", s.name, s.kind, s.rel, s.line))
                .collect();
            content.push_str(&format!(
                "\nOthers by that name{}:\n{}",
                if found.len() - 1 > MAX_CANDIDATES {
                    format!(" ({} in all)", found.len() - 1)
                } else {
                    String::new()
                },
                others.join("\n")
            ));
        }
        self.push_window_step(
            stream,
            &tr!("Went to `{name}`", name = name),
            Some((rel, line)),
        );
        self.window_tool_result(stream, call, content, false);
        self.open_file(abs, Some(line), true)
    }

    fn tool_run_command(&mut self, stream: u64, call: u64, args: &Value) -> Task<Message> {
        let Some(title) = arg(args, "title").map(str::to_string) else {
            self.window_tool_result(stream, call, "Give the command's `title`.".into(), true);
            return Task::none();
        };
        let commands = self.palette_commands();
        // Its exact title; or, failing that, one the words plainly name.
        let command = commands
            .iter()
            .find(|c| c.title == title)
            .or_else(|| crate::app::palette::command_said(&title, &commands).map(|i| &commands[i]));
        let Some(command) = command else {
            self.window_tool_result(
                stream,
                call,
                format!(
                    "The window has no command “{title}” now. Call `{LIST_COMMANDS}` for \
                     the titles it offers."
                ),
                true,
            );
            return Task::none();
        };
        let title = command.title.clone();
        // What the user reads: the title as the palette shows it.
        let shown = command.shown().to_string();
        let (mark, does) = command.cost();
        if !command.asks_the_agent() {
            self.push_window_step(stream, &tr!("Ran {command}", command = shown), None);
            self.window_tool_result(stream, call, format!("Ran “{title}”."), false);
            return self.run_palette(&title);
        }
        // Costly: the user says so first, on a card.
        let Some(turn) = self
            .proj
            .ask_turns
            .iter_mut()
            .find(|t| t.stream == stream && t.streaming)
        else {
            return Task::none();
        };
        turn.extras
            .step_ids
            .insert(format!("window:{call}"), turn.steps.len());
        turn.steps.push(AgentStep {
            tool: "window".into(),
            title: tr!("Run {command}", command = shown),
            refs: Vec::new(),
            state: StepState::Waiting,
        });
        turn.extras.approvals.push(Approval {
            key: ApprovalKey::Window { call },
            mark: Some(mark),
            title: tr!("Run “{command}”?", command = shown),
            detail: tr!(
                "{effect} Clew asks before anything that runs code or costs money.",
                effect = does
            ),
            options: vec![
                ApprovalOption {
                    id: "once".into(),
                    label: tr!("Run it").into(),
                    allows: true,
                    always: false,
                },
                ApprovalOption {
                    id: "deny".into(),
                    label: tr!("Don't run").into(),
                    allows: false,
                    always: false,
                },
            ],
        });
        self.proj.link.window_pending.insert(
            (stream, call),
            PendingCommand {
                title: title.clone(),
                deadline: call_deadline(),
            },
        );
        self.status = tr!(
            "The agent wants to run “{command}” — answer it in Ask",
            command = shown
        );
        self.reveal_ask();
        Task::batch([self.settle_waiting_answers(), self.ask_to_bottom()])
    }

    /// The user answered the card of window call `call`.
    pub(crate) fn on_window_command_answered(
        &mut self,
        stream: u64,
        call: u64,
        allowed: bool,
    ) -> Task<Message> {
        let Some(pending) = self.proj.link.window_pending.remove(&(stream, call)) else {
            return Task::none();
        };
        let id = format!("window:{call}");
        if std::time::Instant::now() >= pending.deadline {
            self.set_step_state(stream, &id, StepState::Failed);
            self.status = tr!("That command waited too long — the agent went on without it").into();
            return Task::none();
        }
        if !allowed {
            self.set_step_state(stream, &id, StepState::Failed);
            self.window_tool_result(
                stream,
                call,
                "The user declined to run this command. Do not ask for it again in this \
                 answer; go on without it."
                    .into(),
                true,
            );
            return Task::none();
        }
        self.set_step_state(stream, &id, StepState::Done);
        let ran = self
            .palette_commands()
            .iter()
            .any(|c| c.title == pending.title);
        if !ran {
            self.set_step_state(stream, &id, StepState::Failed);
            self.window_tool_result(
                stream,
                call,
                format!(
                    "“{}” can no longer be run: the window changed while it waited.",
                    pending.title
                ),
                true,
            );
            return Task::none();
        }
        self.window_tool_result(stream, call, format!("Ran “{}”.", pending.title), false);
        self.run_palette(&pending.title)
    }
}
