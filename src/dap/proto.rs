//! Debug Adapter Protocol (DAP) domain types and the parsing clew needs off the
//! adapter's JSON. DAP frames are `Content-Length`-framed JSON over stdio — the
//! same wire format as LSP (see [`crate::lsp::client`]) — but the payloads carry
//! request / response / **event** semantics rather than JSON-RPC.
//!
//! We keep only the fields clew's debugger uses; everything else on a message is
//! ignored. Lines are 1-based (the DAP default we negotiate in `initialize`).

use std::path::PathBuf;

use serde_json::Value;

use crate::{tr, trn};

/// Why an expression is being evaluated — the `context` of a DAP `evaluate`
/// request. Adapters decide from it what an evaluation may do, so it is part
/// of the request's meaning, not a hint: an adapter that advertises
/// `supportsEvaluateForHovers` promises that a `"hover"` evaluation has no
/// side effects, and that promise covers nothing sent under another context.
///
/// A debug console (the DAP `"repl"` context, where anything goes) would be a
/// third variant; clew has no console, so there is nothing to send it from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalContext {
    /// A data hover over an identifier in the paused program.
    Hover,
    /// A watch expression, re-evaluated at every stop.
    Watch,
}

impl EvalContext {
    /// The wire value of the request's `context` argument.
    pub fn as_str(self) -> &'static str {
        match self {
            EvalContext::Hover => "hover",
            EvalContext::Watch => "watch",
        }
    }
}

/// Whether hovering an identifier in the paused program may evaluate it.
///
/// A hover is not a request to run anything, yet an evaluation can run the
/// program's own code (property getters, `Debug`/`toString` impls). So it
/// happens by default only when the adapter promised a side-effect-free
/// `"hover"` evaluation (`supportsEvaluateForHovers`); for any other adapter
/// the reader has to opt in.
///
/// The rule is `clew_rules::dap::hover_eval_allowed`, proved of the code
/// itself (Aeneas).
pub fn hover_eval_allowed(adapter_promises_no_side_effects: bool, opted_in: bool) -> bool {
    clew_rules::dap::hover_eval_allowed(adapter_promises_no_side_effects, opted_in)
}

/// Whether an adapter's `initialize` capabilities promise a side-effect-free
/// `"hover"` evaluation (`supportsEvaluateForHovers`, DAP spec). Read per
/// session: js-debug's child session answers its own `initialize`, and the
/// hovers go to the child.
pub fn promises_hover_eval(capabilities: &Value) -> bool {
    capabilities
        .get("supportsEvaluateForHovers")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The exception filters an adapter's `initialize` capabilities mark as on
/// by default (`exceptionBreakpointFilters[].default`, DAP spec): what an
/// editor turns on unless the reader chose otherwise. Each filter id as the
/// adapter gave it, bounded: a filter list is a handful of short ids.
pub fn default_exception_filters(capabilities: &Value) -> Vec<String> {
    exception_filters(capabilities, true)
}

/// Every exception filter the adapter offers, on by default or not.
pub fn offered_exception_filters(capabilities: &Value) -> Vec<String> {
    exception_filters(capabilities, false)
}

fn exception_filters(capabilities: &Value, defaults_only: bool) -> Vec<String> {
    capabilities
        .get("exceptionBreakpointFilters")
        .and_then(Value::as_array)
        .map(|filters| {
            filters
                .iter()
                .filter(|f| {
                    !defaults_only || f.get("default").and_then(Value::as_bool) == Some(true)
                })
                .filter_map(|f| f.get("filter").and_then(Value::as_str))
                .filter(|id| !id.is_empty() && id.len() <= 64)
                .take(16)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

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

/// A frame's name without the hash Rust's legacy symbol names end in
/// (`app::main::h0123456789abcdef`), which lldb leaves on: what the code
/// calls it, `app::main`.
fn without_rust_hash(name: &str) -> &str {
    match name.rsplit_once("::h") {
        Some((head, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            head
        }
        _ => name,
    }
}

/// Whether a frame is Rust's panic machinery starting to unwind — where the
/// debugger stops a Rust program that panics (`rust_panic`, the symbol it
/// is told to stop at).
pub fn is_rust_panic(name: &str) -> bool {
    name == "rust_panic" || name.ends_with("::rust_panic")
}

impl StackFrame {
    pub fn from_value(v: &Value) -> Option<StackFrame> {
        Some(StackFrame {
            id: v.get("id")?.as_i64()?,
            name: without_rust_hash(v.get("name").and_then(Value::as_str).unwrap_or("?"))
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
                Some(n) => trn!("… 1 more frame not shown", "… {n} more frames not shown", n),
                None => tr!("… more frames not shown").to_string(),
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

    /// lldb's Rust frame names lose their symbol hash, and keep everything
    /// else; the panic's frame is known by its name.
    #[test]
    fn rust_frame_names_are_read_as_the_code_names_them() {
        assert_eq!(
            without_rust_hash("app::main::hd698b321c1151f52"),
            "app::main"
        );
        assert_eq!(
            without_rust_hash("<T as app::Shape>::area::h0123456789abcdef"),
            "<T as app::Shape>::area"
        );
        assert_eq!(without_rust_hash("app::hash"), "app::hash");
        assert_eq!(without_rust_hash("app::h0123"), "app::h0123");
        assert_eq!(without_rust_hash("ns::helper"), "ns::helper");
        assert!(is_rust_panic("__rustc::rust_panic"));
        assert!(is_rust_panic("rust_panic"));
        assert!(!is_rust_panic("app::not_rust_panic_here"));
    }

    /// debugpy's filters as its `initialize` lists them: only the one on
    /// by default (uncaught exceptions) is asked for; lldb-dap's C++ and
    /// Objective-C filters, none of them default, ask for nothing.
    #[test]
    fn the_exceptions_an_adapter_stops_at_by_default_are_read_from_its_capabilities() {
        let debugpy = json!({ "exceptionBreakpointFilters": [
            { "filter": "raised", "label": "Raised Exceptions", "default": false },
            { "filter": "uncaught", "label": "Uncaught Exceptions", "default": true },
            { "filter": "userUnhandled", "label": "User Uncaught Exceptions" },
        ]});
        assert_eq!(default_exception_filters(&debugpy), vec!["uncaught"]);
        assert_eq!(
            offered_exception_filters(&debugpy),
            vec!["raised", "uncaught", "userUnhandled"]
        );
        let lldb = json!({ "exceptionBreakpointFilters": [
            { "filter": "cpp_catch", "label": "C++ Catch", "default": false },
            { "filter": "cpp_throw", "label": "C++ Throw", "default": false },
        ]});
        assert!(default_exception_filters(&lldb).is_empty());
        assert!(default_exception_filters(&json!({})).is_empty());
        let odd = json!({ "exceptionBreakpointFilters": [
            { "filter": "", "default": true },
            { "filter": "x".repeat(65), "default": true },
            { "default": true },
        ]});
        assert!(default_exception_filters(&odd).is_empty());
    }

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

    /// D1-10: a hover evaluates by default only against an adapter that
    /// promised side-effect-free hovers; any other adapter needs the opt-in.
    #[test]
    fn hover_evaluation_needs_a_promise_or_an_opt_in() {
        assert!(
            hover_eval_allowed(true, false),
            "the adapter's promise suffices"
        );
        assert!(hover_eval_allowed(false, true), "the reader opted in");
        assert!(hover_eval_allowed(true, true));
        assert!(
            !hover_eval_allowed(false, false),
            "no promise and no opt-in: hovering must not run program code"
        );
        assert_eq!(EvalContext::Hover.as_str(), "hover");
        assert_eq!(EvalContext::Watch.as_str(), "watch");
    }
}
