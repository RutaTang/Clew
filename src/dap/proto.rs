//! Debug Adapter Protocol (DAP) domain types and the parsing clew needs off the
//! adapter's JSON. DAP frames are `Content-Length`-framed JSON over stdio — the
//! same wire format as LSP (see [`crate::lsp::client`]) — but the payloads carry
//! request / response / **event** semantics rather than JSON-RPC.
//!
//! We keep only the fields clew's debugger uses; everything else on a message is
//! ignored. Lines are 1-based (the DAP default we negotiate in `initialize`).

use std::path::PathBuf;

use serde_json::Value;

/// A frame in the debuggee's call stack.
#[derive(Debug, Clone)]
pub struct StackFrame {
    /// Opaque adapter handle (used to request scopes/evaluate in this frame).
    pub id: i64,
    pub name: String,
    /// Source path, when the frame has debug info for a file.
    pub path: Option<PathBuf>,
    pub line: usize,
    pub column: usize,
}

impl StackFrame {
    pub fn from_value(v: &Value) -> Option<StackFrame> {
        Some(StackFrame {
            id: v.get("id")?.as_i64()?,
            name: v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            path: v
                .get("source")
                .and_then(|s| s.get("path"))
                .and_then(Value::as_str)
                .map(PathBuf::from),
            line: v.get("line").and_then(Value::as_u64).unwrap_or(0) as usize,
            column: v.get("column").and_then(Value::as_u64).unwrap_or(0) as usize,
        })
    }

    /// A marker row standing in for the frames the client refused to keep (see
    /// [`crate::dap::client::DapClient::stack_trace`]). Not a real frame: it
    /// carries no source, which is also what keeps it inert in the call-stack
    /// panel — that panel only makes a row clickable when `path` is `Some`, and
    /// only frames with a path are candidates for the "jump to the innermost
    /// frame" search. `id` is negative so a `scopes`/`evaluate` call against it
    /// would be an obviously invalid handle rather than another frame's.
    ///
    /// `count` is `None` when the adapter never said how deep the thread really
    /// is: the row then says frames are missing without naming a number, rather
    /// than quoting the only number we have (how many arrived past the cap),
    /// which would understate the truth by however much the adapter withheld.
    pub fn elided(count: Option<usize>) -> StackFrame {
        StackFrame {
            id: -1,
            name: match count {
                Some(n) => format!("… {n} more frames not shown"),
                None => "… more frames not shown".to_string(),
            },
            path: None,
            line: 0,
            column: 0,
        }
    }
}

/// A named group of variables within a frame (Locals, Globals, Registers…).
#[derive(Debug, Clone)]
pub struct Scope {
    pub name: String,
    /// Handle to fetch this scope's variables; `0` means none.
    pub variables_reference: i64,
    /// Costly to compute (e.g. Registers) — clew collapses these by default.
    pub expensive: bool,
}

impl Scope {
    pub fn from_value(v: &Value) -> Option<Scope> {
        Some(Scope {
            name: v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            variables_reference: v
                .get("variablesReference")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            expensive: v.get("expensive").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

/// A variable `name = value`, possibly expandable into children.
#[derive(Debug, Clone)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub type_name: Option<String>,
    /// `>0` → has children (struct/array/map) that can be lazily expanded.
    pub variables_reference: i64,
}

impl Variable {
    pub fn from_value(v: &Value) -> Option<Variable> {
        Some(Variable {
            name: v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            value: v
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            type_name: v.get("type").and_then(Value::as_str).map(str::to_string),
            variables_reference: v
                .get("variablesReference")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        })
    }
}

/// What the adapter did with ONE requested breakpoint: its answer paired with
/// the line clew asked for. The pairing is the point — the gutter draws its dots
/// from clew's own breakpoint map, so an answer that is parsed and dropped
/// leaves a solid dot on a line the adapter refused or moved away from, and the
/// program then runs past it while the UI insists the breakpoint is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakpoint {
    /// The adapter's own handle for this breakpoint, when it gave one. It is
    /// the ONLY thing tying a later [`DapEvent::BreakpointChanged`] back to the
    /// line the user set: that event describes where the adapter ended up and
    /// never repeats what was asked for.
    pub id: Option<i64>,
    /// The line clew asked the adapter to break on.
    pub requested_line: usize,
    /// The adapter bound it. `false` means it is not live: a blank line, a
    /// comment, code optimized out of a release build, or a source path that
    /// does not match the one in the binary's debug info.
    pub verified: bool,
    /// The line the adapter actually bound it to. `None` means the adapter did
    /// not report one, which is NOT the same as "bound where requested".
    pub line: Option<usize>,
    /// The adapter's explanation, when it gave one (DAP `Breakpoint.message`).
    pub message: Option<String>,
}

impl Breakpoint {
    /// Parse the adapter's answer for the breakpoint requested at
    /// `requested_line`; the caller supplies that, since the wire object only
    /// describes where the adapter ended up.
    pub fn from_value(requested_line: usize, v: &Value) -> Breakpoint {
        Breakpoint {
            id: v.get("id").and_then(Value::as_i64),
            requested_line,
            verified: v.get("verified").and_then(Value::as_bool).unwrap_or(false),
            line: v.get("line").and_then(Value::as_u64).map(|l| l as usize),
            message: v
                .get("message")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(str::to_string),
        }
    }

    /// The line a *bound* breakpoint actually landed on, when that is not the
    /// line clew asked for. `None` when it stayed put, when the adapter refused
    /// it (a refusal is not a move), or when no line was reported.
    pub fn relocated_to(&self) -> Option<usize> {
        match self.line {
            Some(l) if self.verified && l != self.requested_line => Some(l),
            _ => None,
        }
    }
}

/// Details of a `stopped` event — why and where execution paused.
#[derive(Debug, Clone)]
pub struct Stopped {
    pub reason: String, // "breakpoint" | "step" | "exception" | "entry" | …
    pub thread_id: Option<i64>,
    pub description: Option<String>,
    /// Extra text (e.g. an exception message).
    pub text: Option<String>,
    pub all_threads: bool,
}

/// A chunk of program/adapter output.
#[derive(Debug, Clone)]
pub struct Output {
    pub category: String, // "stdout" | "stderr" | "console" | …
    pub text: String,
}

/// An event pushed from the adapter to clew's update loop.
#[derive(Debug, Clone)]
pub enum DapEvent {
    /// Adapter is ready for configuration (send breakpoints + configurationDone).
    Initialized,
    Stopped(Stopped),
    Continued {
        thread_id: Option<i64>,
        all_threads: bool,
    },
    Output(Output),
    /// The debuggee process exited with a status code.
    Exited {
        code: i64,
    },
    /// The debug session ended.
    Terminated,
    /// The adapter asked clew to start a CHILD debug session (js-debug's
    /// multi-session model) with the given launch configuration.
    StartDebugging(Value),
    /// The adapter revised a breakpoint it had already answered for.
    ///
    /// Binding is lazy: a breakpoint in a shared library or a module that is
    /// not loaded yet comes back from `setBreakpoints` as `verified: false` and
    /// flips to true later, with no second request from us. Without this
    /// channel the gutter would keep showing the first answer for the rest of
    /// the session — a live breakpoint drawn as refused.
    ///
    /// The event says where the adapter ended up, never what was asked for, so
    /// `id` is the only way back to the user's line: see [`Breakpoint::id`].
    BreakpointChanged {
        id: Option<i64>,
        verified: bool,
        line: Option<usize>,
        message: Option<String>,
    },
    /// Any other event, kept by name (module, thread, process…) — mostly noise.
    Other(String),
}

impl DapEvent {
    /// Parse a DAP event by name + `body`.
    pub fn parse(event: &str, body: &Value) -> DapEvent {
        match event {
            "initialized" => DapEvent::Initialized,
            "stopped" => DapEvent::Stopped(Stopped {
                reason: body
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                thread_id: body.get("threadId").and_then(Value::as_i64),
                description: body
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                text: body.get("text").and_then(Value::as_str).map(str::to_string),
                all_threads: body
                    .get("allThreadsStopped")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            "continued" => DapEvent::Continued {
                thread_id: body.get("threadId").and_then(Value::as_i64),
                all_threads: body
                    .get("allThreadsContinued")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
            "output" => DapEvent::Output(Output {
                category: body
                    .get("category")
                    .and_then(Value::as_str)
                    .unwrap_or("console")
                    .to_string(),
                text: body
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            }),
            "exited" => DapEvent::Exited {
                code: body.get("exitCode").and_then(Value::as_i64).unwrap_or(0),
            },
            "terminated" => DapEvent::Terminated,
            "breakpoint" => {
                // `reason` is "changed" | "new" | "removed"; all three are the
                // same thing here — the adapter's current answer for this
                // handle. A "removed" breakpoint reports `verified: false`,
                // which is exactly how it should be drawn.
                let bp = body.get("breakpoint").unwrap_or(&Value::Null);
                DapEvent::BreakpointChanged {
                    id: bp.get("id").and_then(Value::as_i64),
                    verified: bp.get("verified").and_then(Value::as_bool).unwrap_or(false),
                    line: bp.get("line").and_then(Value::as_u64).map(|l| l as usize),
                    message: bp
                        .get("message")
                        .and_then(Value::as_str)
                        .filter(|m| !m.is_empty())
                        .map(str::to_string),
                }
            }
            other => DapEvent::Other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_stack_frame_with_source() {
        let v = json!({
            "id": 524288, "name": "spike::add",
            "source": { "path": "/p/spike.rs" }, "line": 2, "column": 15
        });
        let f = StackFrame::from_value(&v).unwrap();
        assert_eq!(f.id, 524288);
        assert_eq!(f.name, "spike::add");
        assert_eq!(f.path, Some(PathBuf::from("/p/spike.rs")));
        assert_eq!((f.line, f.column), (2, 15));
    }

    #[test]
    fn parses_variable_and_expandability() {
        let leaf = Variable::from_value(
            &json!({"name":"a","value":"10","type":"i32","variablesReference":0}),
        )
        .unwrap();
        assert_eq!((leaf.name.as_str(), leaf.value.as_str()), ("a", "10"));
        assert_eq!(leaf.type_name.as_deref(), Some("i32"));
        assert!(leaf.variables_reference == 0, "leaf not expandable");

        let node = Variable::from_value(&json!({"name":"v","value":"Vec","variablesReference":7}))
            .unwrap();
        assert!(node.variables_reference > 0, "struct/array is expandable");
    }

    /// A breakpoint answer is only actionable next to the line clew asked for,
    /// and an adapter that omits `verified` has NOT confirmed anything — the
    /// default has to be the pessimistic one, or a silent adapter would leave
    /// the gutter claiming a live breakpoint.
    #[test]
    fn breakpoint_answer_keeps_the_requested_line() {
        let silent = Breakpoint::from_value(12, &json!({}));
        assert!(!silent.verified);
        assert_eq!(silent.requested_line, 12);
        assert_eq!(silent.line, None);
        assert_eq!(silent.relocated_to(), None);

        // An empty `message` is no message: it must not become an explanation
        // the UI would print as if the adapter had said something.
        let refused = Breakpoint::from_value(12, &json!({"verified": false, "message": ""}));
        assert_eq!(refused.message, None);

        // Bound elsewhere, and bound where asked.
        assert_eq!(
            Breakpoint::from_value(12, &json!({"verified": true, "line": 15})).relocated_to(),
            Some(15)
        );
        assert_eq!(
            Breakpoint::from_value(12, &json!({"verified": true, "line": 12})).relocated_to(),
            None
        );
        // A refusal that still names a line is not a relocation to show.
        assert_eq!(
            Breakpoint::from_value(12, &json!({"verified": false, "line": 15})).relocated_to(),
            None
        );
    }

    /// The elision row must never look like a frame the user can jump to, and
    /// must not invent a count the adapter never gave.
    #[test]
    fn elided_row_is_inert_and_only_counts_what_is_known() {
        let known = StackFrame::elided(Some(90));
        assert!(known.name.contains("90"));
        assert!(known.path.is_none());
        assert!(known.id < 0);
        assert!(
            !StackFrame::elided(None)
                .name
                .chars()
                .any(|c| c.is_ascii_digit())
        );
    }

    #[test]
    fn parses_events() {
        assert!(matches!(
            DapEvent::parse("initialized", &json!({})),
            DapEvent::Initialized
        ));
        assert!(matches!(
            DapEvent::parse("terminated", &json!({})),
            DapEvent::Terminated
        ));

        let stop = DapEvent::parse(
            "stopped",
            &json!({
                "reason": "breakpoint", "threadId": 2656993, "allThreadsStopped": true
            }),
        );
        match stop {
            DapEvent::Stopped(s) => {
                assert_eq!(s.reason, "breakpoint");
                assert_eq!(s.thread_id, Some(2656993));
                assert!(s.all_threads);
            }
            _ => panic!("expected Stopped"),
        }

        let out = DapEvent::parse(
            "output",
            &json!({"category":"stdout","output":"result = 42\n"}),
        );
        match out {
            DapEvent::Output(o) => assert_eq!(
                (o.category.as_str(), o.text.as_str()),
                ("stdout", "result = 42\n")
            ),
            _ => panic!("expected Output"),
        }

        // Unknown events are kept by name (module/thread/process noise).
        assert!(
            matches!(DapEvent::parse("module", &json!({})), DapEvent::Other(n) if n == "module")
        );
    }
}
