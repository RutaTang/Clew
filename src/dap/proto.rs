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
            name: v.get("name").and_then(Value::as_str).unwrap_or("?").to_string(),
            path: v
                .get("source")
                .and_then(|s| s.get("path"))
                .and_then(Value::as_str)
                .map(PathBuf::from),
            line: v.get("line").and_then(Value::as_u64).unwrap_or(0) as usize,
            column: v.get("column").and_then(Value::as_u64).unwrap_or(0) as usize,
        })
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
            name: v.get("name").and_then(Value::as_str).unwrap_or("?").to_string(),
            variables_reference: v.get("variablesReference").and_then(Value::as_i64).unwrap_or(0),
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
            name: v.get("name").and_then(Value::as_str).unwrap_or("?").to_string(),
            value: v.get("value").and_then(Value::as_str).unwrap_or("").to_string(),
            type_name: v.get("type").and_then(Value::as_str).map(str::to_string),
            variables_reference: v.get("variablesReference").and_then(Value::as_i64).unwrap_or(0),
        })
    }
}

/// A breakpoint's verified state, as returned by `setBreakpoints`.
#[derive(Debug, Clone)]
pub struct Breakpoint {
    pub verified: bool,
    /// The line the adapter actually bound it to (may differ from requested).
    pub line: Option<usize>,
}

impl Breakpoint {
    pub fn from_value(v: &Value) -> Breakpoint {
        Breakpoint {
            verified: v.get("verified").and_then(Value::as_bool).unwrap_or(false),
            line: v.get("line").and_then(Value::as_u64).map(|l| l as usize),
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
    Continued { thread_id: Option<i64>, all_threads: bool },
    Output(Output),
    /// The debuggee process exited with a status code.
    Exited { code: i64 },
    /// The debug session ended.
    Terminated,
    /// The adapter asked clew to start a CHILD debug session (js-debug's
    /// multi-session model) with the given launch configuration.
    StartDebugging(Value),
    /// Any other event, kept by name (module, thread, process…) — mostly noise.
    Other(String),
}

impl DapEvent {
    /// Parse a DAP event by name + `body`.
    pub fn parse(event: &str, body: &Value) -> DapEvent {
        match event {
            "initialized" => DapEvent::Initialized,
            "stopped" => DapEvent::Stopped(Stopped {
                reason: body.get("reason").and_then(Value::as_str).unwrap_or("").to_string(),
                thread_id: body.get("threadId").and_then(Value::as_i64),
                description: body.get("description").and_then(Value::as_str).map(str::to_string),
                text: body.get("text").and_then(Value::as_str).map(str::to_string),
                all_threads: body.get("allThreadsStopped").and_then(Value::as_bool).unwrap_or(false),
            }),
            "continued" => DapEvent::Continued {
                thread_id: body.get("threadId").and_then(Value::as_i64),
                all_threads: body.get("allThreadsContinued").and_then(Value::as_bool).unwrap_or(false),
            },
            "output" => DapEvent::Output(Output {
                category: body.get("category").and_then(Value::as_str).unwrap_or("console").to_string(),
                text: body.get("output").and_then(Value::as_str).unwrap_or("").to_string(),
            }),
            "exited" => DapEvent::Exited {
                code: body.get("exitCode").and_then(Value::as_i64).unwrap_or(0),
            },
            "terminated" => DapEvent::Terminated,
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
        let leaf = Variable::from_value(&json!({"name":"a","value":"10","type":"i32","variablesReference":0})).unwrap();
        assert_eq!((leaf.name.as_str(), leaf.value.as_str()), ("a", "10"));
        assert_eq!(leaf.type_name.as_deref(), Some("i32"));
        assert!(leaf.variables_reference == 0, "leaf not expandable");

        let node = Variable::from_value(&json!({"name":"v","value":"Vec","variablesReference":7})).unwrap();
        assert!(node.variables_reference > 0, "struct/array is expandable");
    }

    #[test]
    fn parses_events() {
        assert!(matches!(DapEvent::parse("initialized", &json!({})), DapEvent::Initialized));
        assert!(matches!(DapEvent::parse("terminated", &json!({})), DapEvent::Terminated));

        let stop = DapEvent::parse("stopped", &json!({
            "reason": "breakpoint", "threadId": 2656993, "allThreadsStopped": true
        }));
        match stop {
            DapEvent::Stopped(s) => {
                assert_eq!(s.reason, "breakpoint");
                assert_eq!(s.thread_id, Some(2656993));
                assert!(s.all_threads);
            }
            _ => panic!("expected Stopped"),
        }

        let out = DapEvent::parse("output", &json!({"category":"stdout","output":"result = 42\n"}));
        match out {
            DapEvent::Output(o) => assert_eq!((o.category.as_str(), o.text.as_str()), ("stdout", "result = 42\n")),
            _ => panic!("expected Output"),
        }

        // Unknown events are kept by name (module/thread/process noise).
        assert!(matches!(DapEvent::parse("module", &json!({})), DapEvent::Other(n) if n == "module"));
    }
}
