//! The wire-shape snapshot: one serialized sample of every `Request` and
//! `Event` variant, compared against a golden file whose NAME carries the
//! protocol version (`tests/snapshots/wire-v<N>.snap`).
//!
//! This is the review-time half of the compatibility guard (the build-time
//! [`clew_protocol::SCHEMA_FINGERPRINT`] is the runtime half): any change to
//! what actually crosses the wire fails this test until the snapshot is
//! regenerated — and regenerating it under the OLD version number is a
//! deliberate act a reviewer can see, while the intended flow renames the
//! file, which forces bumping `PROTOCOL_VERSION` in the same change.
//!
//! Regenerate with:
//!   UPDATE_WIRE_SNAPSHOT=1 cargo test -p clew-protocol --test wire_snapshot

use clew_protocol::*;

/// Compile-time completeness: adding a `Request` variant breaks this match
/// until a sample for it is added below.
#[allow(dead_code)]
fn every_request_variant_is_sampled(r: &Request) {
    match r {
        Request::Hello { .. }
        | Request::OpenProject { .. }
        | Request::ReadFile { .. }
        | Request::Search { .. }
        | Request::Find { .. }
        | Request::Outline { .. }
        | Request::GitInfo { .. }
        | Request::ReadState { .. }
        | Request::WriteState { .. }
        | Request::EditState { .. }
        | Request::Stats
        | Request::ProjectCalls { .. }
        | Request::ReadSources { .. }
        | Request::Git { .. }
        | Request::Watch
        | Request::SpawnProcess { .. }
        | Request::SpawnLsp { .. }
        | Request::SpawnAdapter { .. }
        | Request::LspResolve { .. }
        | Request::LspInstall { .. }
        | Request::LspApprovals { .. }
        | Request::ProcessInput { .. }
        | Request::ProcessKill { .. }
        | Request::Explain { .. }
        | Request::Cancel { .. }
        | Request::SetAiConfig { .. }
        | Request::Chat { .. }
        | Request::Embed { .. }
        | Request::ChatStream { .. }
        | Request::AgentAsk { .. }
        | Request::AgentStop { .. }
        | Request::ListDir { .. }
        | Request::BuildDocs => {}
    }
}

/// Compile-time completeness for `Event`, same idea.
#[allow(dead_code)]
fn every_event_variant_is_sampled(e: &Event) {
    match e {
        Event::Ready { .. }
        | Event::Tree { .. }
        | Event::FileContent { .. }
        | Event::NotebookContent { .. }
        | Event::SymbolIndexDone
        | Event::Outline { .. }
        | Event::GitInfo { .. }
        | Event::Stats { .. }
        | Event::ProjectCalls { .. }
        | Event::Sources { .. }
        | Event::GitResult { .. }
        | Event::StateContent { .. }
        | Event::StateWritten { .. }
        | Event::StateEdited { .. }
        | Event::SearchResults { .. }
        | Event::FilesChanged { .. }
        | Event::ProjectSymbols { .. }
        | Event::ProcessOutput { .. }
        | Event::ProcessStarted { .. }
        | Event::ProcessExited { .. }
        | Event::AdapterSpawned { .. }
        | Event::Explanation { .. }
        | Event::ChatResult { .. }
        | Event::ChatDelta { .. }
        | Event::ChatStreamDone { .. }
        | Event::Embeddings { .. }
        | Event::DirListing { .. }
        | Event::Docs { .. }
        | Event::AgentStep { .. }
        | Event::AgentDelta { .. }
        | Event::AgentDone { .. }
        | Event::Status { .. }
        | Event::LspResolved { .. }
        | Event::Error { .. } => {}
    }
}

fn target() -> TargetSpec {
    TargetSpec {
        label: "Host (macos)".into(),
        os: "macos".into(),
        arch: "aarch64".into(),
        family: "unix".into(),
    }
}

fn chat_msg() -> AiChatMsg {
    AiChatMsg {
        role: "user".into(),
        content: "q".into(),
    }
}

fn request_samples() -> Vec<Request> {
    vec![
        Request::Hello {
            protocol: PROTOCOL_VERSION,
            fingerprint: "f".into(),
            ai: AiEndpoint::Server,
        },
        Request::OpenProject { root: "/p".into() },
        Request::ReadFile {
            rel: "a.rs".into(),
            target: target(),
        },
        Request::Search {
            query: "q".into(),
            regex: false,
            case_sensitive: true,
            whole_word: false,
            include: "*.rs".into(),
            exclude: "target".into(),
        },
        Request::Find { query: "q".into() },
        Request::Outline { rel: "a.rs".into() },
        Request::GitInfo { rel: "a.rs".into() },
        Request::ReadState {
            root: "/p".into(),
            rel: "bookmarks.json".into(),
        },
        Request::WriteState {
            root: "/p".into(),
            rel: "bookmarks.json".into(),
            text: Some("[]".into()),
        },
        Request::EditState {
            root: "/p".into(),
            rel: "bookmarks.json".into(),
            merge: StateMerge {
                key_fields: vec!["rel".into(), "line".into()],
                key: vec!["a.rs".into(), 1.into()],
                edit: StateEdit::Toggle(serde_json::json!({"rel": "a.rs", "line": 1})),
                delete_when_empty: true,
            },
        },
        Request::Stats,
        Request::ProjectCalls {
            scope: vec![("a.rs".into(), vec!["b.rs".into()])],
        },
        Request::ReadSources {
            rels: vec!["a.rs".into()],
        },
        Request::Git {
            op: GitOp::FileHistory {
                rel: "a.rs".into(),
                limit: 10,
            },
        },
        Request::Git {
            op: GitOp::SymbolHistory {
                rel: "a.rs".into(),
                start: 1,
                end: 9,
                limit: 10,
            },
        },
        Request::Git {
            op: GitOp::FileAt {
                sha: "abc".into(),
                rel: "a.rs".into(),
            },
        },
        Request::Git {
            op: GitOp::AddedLines {
                sha: "abc".into(),
                rel: "a.rs".into(),
            },
        },
        Request::Git {
            op: GitOp::CommitMessage { sha: "abc".into() },
        },
        Request::Git {
            op: GitOp::CommitFileDiff {
                sha: "abc".into(),
                rel: "a.rs".into(),
                max_bytes: 4096,
            },
        },
        Request::Git {
            op: GitOp::DiffLines { rel: "a.rs".into() },
        },
        Request::Git {
            op: GitOp::ReviewBase,
        },
        Request::Git {
            op: GitOp::CommitSubjects {
                base: "main".into(),
            },
        },
        Request::Git {
            op: GitOp::ChangedFiles {
                base: "main".into(),
            },
        },
        Request::Git {
            op: GitOp::RangePatch {
                base: "main".into(),
                max_bytes: 4096,
            },
        },
        Request::Watch,
        Request::SpawnProcess {
            proc: 1,
            cmd: "ls".into(),
            args: vec!["-l".into()],
            cwd: Some("/p".into()),
        },
        Request::SpawnLsp {
            proc: 1,
            language: "rust".into(),
        },
        Request::SpawnAdapter {
            proc: 1,
            lang: "native".into(),
            program: "/p/bin".into(),
            args: vec![],
        },
        Request::LspResolve {
            language: "rust".into(),
        },
        Request::LspInstall {
            language: "rust".into(),
        },
        Request::LspApprovals {
            approvals: vec![("rust".into(), "fp".into())],
        },
        Request::ProcessInput {
            proc: 1,
            data: vec![0, 1],
        },
        Request::ProcessKill { proc: 1 },
        Request::Explain {
            rel: "a.rs".into(),
            symbol: Some("main".into()),
        },
        Request::Cancel { sub: 1 },
        Request::SetAiConfig {
            chat: Some(AiChatConfig {
                provider: "anthropic".into(),
                api_key: "k".into(),
                model: "m".into(),
                base_url: "u".into(),
            }),
            embed: Some(AiEmbedConfig {
                api_key: "k".into(),
                model: "m".into(),
                base_url: "u".into(),
            }),
        },
        Request::Chat {
            system: "s".into(),
            messages: vec![chat_msg()],
            max_tokens: 64,
        },
        Request::Embed {
            texts: vec!["t".into()],
        },
        Request::ChatStream {
            stream: 1,
            system: "s".into(),
            messages: vec![chat_msg()],
            max_tokens: 64,
        },
        Request::AgentAsk {
            stream: 1,
            question: "q".into(),
            history: vec![chat_msg()],
            context: "c".into(),
        },
        Request::AgentStop { stream: 1 },
        Request::ListDir {
            path: Some("~".into()),
        },
        Request::BuildDocs,
    ]
}

fn event_samples() -> Vec<Event> {
    vec![
        Event::Ready {
            protocol: PROTOCOL_VERSION,
            fingerprint: "f".into(),
        },
        Event::Tree {
            root: "/p".into(),
            tree: DirNode {
                dirs: vec![(
                    "src".into(),
                    DirNode {
                        dirs: vec![],
                        files: vec!["a.rs".into()],
                    },
                )],
                files: vec!["README.md".into()],
            },
            files: vec!["src/a.rs".into()],
            truncated: false,
        },
        Event::FileContent {
            rel: "a.rs".into(),
            source: "fn main() {}".into(),
            lines: vec![HlLine {
                spans: vec![("fn".into(), Some(1)), (" main() {}".into(), None)],
            }],
            symbols: vec![Symbol {
                name: "main".into(),
                kind: "function".into(),
                line: 1,
                end_line: 1,
            }],
            docs: vec![(1, "entry".into())],
            inactive: vec![2],
        },
        Event::NotebookContent {
            rel: "n.ipynb".into(),
            language: "python".into(),
            cells: vec![NotebookCell {
                kind: "code".into(),
                source: "x = 1".into(),
                lines: vec![HlLine {
                    spans: vec![("x = 1".into(), None)],
                }],
                proj_line: 1,
                outputs: vec![
                    NotebookOutput::Text {
                        spans: vec![("1".into(), Some(2))],
                        stderr: false,
                    },
                    NotebookOutput::Image { data: vec![0] },
                    NotebookOutput::Svg("<svg/>".into()),
                    NotebookOutput::Placeholder("widget".into()),
                ],
                execution_count: Some(1),
            }],
            symbols: vec![],
            projection: "# %%\nx = 1".into(),
        },
        Event::SymbolIndexDone,
        Event::Outline {
            rel: "a.rs".into(),
            symbols: vec![],
        },
        Event::GitInfo {
            rel: "a.rs".into(),
            info: Some(GitInfo {
                blame: vec![BlameLine {
                    commit: "abc".into(),
                    author: "a".into(),
                    time: 1,
                    summary: "s".into(),
                    uncommitted: false,
                }],
                status: vec![None, Some(ChangeKind::Added), Some(ChangeKind::Modified)],
                deleted_at: std::collections::HashSet::from([1]),
            }),
        },
        Event::Stats {
            root: "/p".into(),
            report: "{}".into(),
        },
        Event::ProjectCalls {
            root: "/p".into(),
            graph: "{}".into(),
        },
        Event::Sources {
            root: "/p".into(),
            files: vec![("a.rs".into(), "fn main() {}".into())],
        },
        Event::GitResult {
            root: "/p".into(),
            result: "[]".into(),
        },
        Event::StateContent {
            root: "/p".into(),
            rel: "bookmarks.json".into(),
            text: None,
        },
        Event::StateWritten {
            root: "/p".into(),
            rel: "bookmarks.json".into(),
        },
        Event::StateEdited {
            root: "/p".into(),
            rel: "bookmarks.json".into(),
            text: Some("[]".into()),
        },
        Event::SearchResults {
            hits: vec![SearchHit {
                rel: "a.rs".into(),
                line: 1,
                preview: "fn main".into(),
            }],
            error: None,
        },
        Event::FilesChanged {
            root: "/p".into(),
            rels: vec!["a.rs".into()],
        },
        Event::ProjectSymbols {
            root: "/p".into(),
            seq: 1,
            full: true,
            files: vec![FileSymbols {
                rel: "a.rs".into(),
                symbols: vec![IndexSymbol {
                    name: "main".into(),
                    kind: "function".into(),
                    line: 1,
                    is_test: false,
                }],
                imports: vec![WireImport {
                    module: "std::io".into(),
                    line: 1,
                    is_mod: false,
                }],
            }],
            go_module: Patch::Set(Some("example.com/m".into())),
            // The three states each appear once, so the snapshot pins how
            // "not recomputed" and "recomputed to nothing" serialize apart.
            dart_package: Patch::Unchanged,
            structure: Patch::Set(None),
        },
        Event::ProcessOutput {
            proc: 1,
            data: vec![0],
        },
        Event::ProcessStarted { proc: 1 },
        Event::ProcessExited {
            proc: 1,
            code: Some(0),
        },
        Event::AdapterSpawned {
            proc: 1,
            launch: "{}".into(),
        },
        Event::Explanation {
            rel: "a.rs".into(),
            symbol: None,
            markdown: "md".into(),
        },
        Event::ChatResult { text: "t".into() },
        Event::ChatDelta {
            stream: 1,
            text: "t".into(),
        },
        Event::ChatStreamDone {
            stream: 1,
            error: None,
        },
        Event::Embeddings {
            vecs: vec![vec![0.5]],
        },
        Event::DirListing {
            path: "/home".into(),
            parent: Some("/".into()),
            entries: vec![DirEntry {
                name: "p".into(),
                is_dir: true,
            }],
        },
        Event::Docs {
            root: "/p".into(),
            files: vec![DocFile {
                rel: "a.rs".into(),
                items: vec![DocItem {
                    name: "main".into(),
                    kind: "function".into(),
                    signature: "fn main()".into(),
                    doc: "entry".into(),
                    line: 1,
                    public: true,
                    children: vec![],
                }],
            }],
        },
        Event::AgentStep {
            stream: 1,
            tool: "search".into(),
            title: "search \"q\" → 1".into(),
            refs: vec![AgentRef {
                rel: "a.rs".into(),
                line: Some(1),
            }],
        },
        Event::AgentDelta {
            stream: 1,
            text: "t".into(),
        },
        Event::AgentDone {
            stream: 1,
            error: None,
        },
        Event::Status {
            message: "ok".into(),
        },
        Event::LspResolved {
            language: "rust".into(),
            root: "/p".into(),
            resolution: LspResolution::Command(LspCommandSpec {
                command: "/p/ra".into(),
                args: vec![],
                server: "rust-analyzer".into(),
                version: "1".into(),
                fingerprint: "fp".into(),
                init_options: None,
            }),
        },
        Event::Error {
            message: "boom".into(),
        },
    ]
}

/// Every LspResolution variant crosses the wire inside `LspResolved`; sample
/// the ones the main list doesn't cover.
fn extra_resolution_samples() -> Vec<Event> {
    vec![
        Event::LspResolved {
            language: "rust".into(),
            root: "/p".into(),
            resolution: LspResolution::Ready {
                init_options: None,
                withheld: None,
            },
        },
        // The withheld shape is a separate sample: it is what the client needs
        // to raise the approval modal, so a field silently lost from it takes
        // the only grant path with it.
        Event::LspResolved {
            language: "rust".into(),
            root: "/p".into(),
            resolution: LspResolution::Ready {
                init_options: None,
                withheld: Some(LspOptionsSpec {
                    server: "rust-analyzer".into(),
                    version: "1".into(),
                    args: vec![],
                    fingerprint: "fp".into(),
                    options: "{\"a\":1}".into(),
                }),
            },
        },
        Event::LspResolved {
            language: "rust".into(),
            root: "/p".into(),
            resolution: LspResolution::NeedsInstall {
                server: "rust-analyzer".into(),
                version: "1".into(),
                describe: "d".into(),
            },
        },
        Event::LspResolved {
            language: "zig".into(),
            root: "/p".into(),
            resolution: LspResolution::Unsupported {
                message: "no server".into(),
            },
        },
    ]
}

fn render() -> String {
    let mut lines = Vec::new();
    for r in request_samples() {
        lines.push(
            serde_json::to_string(&ClientMessage { id: 1, request: r })
                .expect("request serializes"),
        );
    }
    for e in event_samples()
        .into_iter()
        .chain(extra_resolution_samples())
    {
        lines.push(
            serde_json::to_string(&ServerMessage::Reply {
                id: 1,
                sub: None,
                event: e,
            })
            .expect("event serializes"),
        );
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[test]
fn wire_shape_matches_versioned_snapshot() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join(format!("wire-v{PROTOCOL_VERSION}.snap"));
    let rendered = render();

    // Everything in the snapshot must also deserialize — a shape that
    // serializes but cannot round-trip is broken regardless of the snapshot.
    for line in rendered.lines() {
        if serde_json::from_str::<ClientMessage>(line).is_err() {
            let _: ServerMessage = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("sample does not round-trip: {e}: {line}"));
        }
    }

    if std::env::var_os("UPDATE_WIRE_SNAPSHOT").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing wire snapshot {} — the wire shape changed (or the snapshot \
             was never generated for v{PROTOCOL_VERSION}). If the change is \
             intentional, bump PROTOCOL_VERSION and regenerate: \
             UPDATE_WIRE_SNAPSHOT=1 cargo test -p clew-protocol --test wire_snapshot",
            path.display()
        )
    });
    assert!(
        golden == rendered,
        "the serialized wire shape differs from tests/snapshots/wire-v{PROTOCOL_VERSION}.snap.\n\
         A wire change needs a PROTOCOL_VERSION bump (which renames the snapshot) — \
         then regenerate with:\n  UPDATE_WIRE_SNAPSHOT=1 cargo test -p clew-protocol --test wire_snapshot"
    );
}
