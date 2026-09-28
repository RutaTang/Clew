//! The wire-shape snapshot: a serialized sample of every `Request` and `Event`
//! variant — and of every variant of every enum nested inside them — compared
//! against a golden file whose NAME carries the protocol version
//! (`tests/snapshots/wire-v<N>.snap`).
//!
//! This is the review-time half of the compatibility guard (the build-time
//! [`clew_protocol::SCHEMA_FINGERPRINT`] is the runtime half): any change to
//! what actually crosses the wire fails this test until the snapshot is
//! regenerated — and regenerating it under the OLD version number is a
//! deliberate act a reviewer can see, while the intended flow renames the
//! file, which forces bumping `PROTOCOL_VERSION` in the same change.
//!
//! Coverage is enforced, not hoped for. Each enum has an exhaustive `*_index`
//! function (a new variant does not compile until it is given an index), and
//! the samples must hit every index from 0 to the enum's variant count — a
//! count READ FROM THE SOURCE (`declared_enums`), never kept by hand, so a new
//! variant cannot pass on an index nobody samples. Every enum the source
//! declares must be in that check, so a new enum cannot slip past it either.
//!
//! Regenerate with:
//!   UPDATE_WIRE_SNAPSHOT=1 cargo test -p clew-protocol --test wire_snapshot

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use clew_protocol::*;
use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};

// -- the declared enums ---------------------------------------------------------

/// Every `enum` the protocol source declares — outside `#[cfg(test)]` items,
/// which reach neither peer — with its variant names, read with a real Rust
/// lexer (the one the build fingerprint uses).
fn declared_enums() -> BTreeMap<String, Vec<String>> {
    let mut enums = BTreeMap::new();
    let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                let tokens = TokenStream::from_str(&text)
                    .unwrap_or_else(|e| panic!("{} does not tokenize: {e}", path.display()));
                collect_enums(tokens, &mut enums);
            }
        }
    }
    enums
}

/// The enums among one file's top-level items.
fn collect_enums(stream: TokenStream, out: &mut BTreeMap<String, Vec<String>>) {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut i = 0;
    while i < tokens.len() {
        if is_cfg_test(&tokens[i..]) {
            // The attribute, then the item up to its body or its `;`.
            i += 2;
            while i < tokens.len() {
                let end = match &tokens[i] {
                    TokenTree::Group(g) => g.delimiter() == Delimiter::Brace,
                    TokenTree::Punct(p) => p.as_char() == ';',
                    _ => false,
                };
                i += 1;
                if end {
                    break;
                }
            }
            continue;
        }
        if let (TokenTree::Ident(keyword), Some(TokenTree::Ident(name))) =
            (&tokens[i], tokens.get(i + 1))
            && keyword == "enum"
        {
            // Past any generics, the first brace group is the body.
            let body = tokens[i + 2..].iter().find_map(|t| match t {
                TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => Some(g.stream()),
                _ => None,
            });
            let body = body.unwrap_or_else(|| panic!("enum {name} has no body"));
            out.insert(name.to_string(), variant_names(body));
        }
        i += 1;
    }
}

/// `#[cfg(test)]` at the start of `tokens`.
fn is_cfg_test(tokens: &[TokenTree]) -> bool {
    matches!(
        (tokens.first(), tokens.get(1)),
        (Some(TokenTree::Punct(hash)), Some(TokenTree::Group(attr)))
            if hash.as_char() == '#'
                && attr.delimiter() == Delimiter::Bracket
                && attr.stream().to_string().replace(' ', "") == "cfg(test)"
    )
}

/// The variant names of an enum body: the first identifier of each
/// comma-separated entry, past its attributes (doc comments reach the token
/// stream as `#[doc = …]`). Commas inside a variant's fields sit in a group of
/// their own and never split an entry.
fn variant_names(body: TokenStream) -> Vec<String> {
    let mut names = Vec::new();
    let mut expecting = true;
    let mut tokens = body.into_iter().peekable();
    while let Some(token) = tokens.next() {
        match token {
            TokenTree::Punct(p) if p.as_char() == ',' && p.spacing() == Spacing::Alone => {
                expecting = true;
            }
            // An attribute: `#` and its bracket group.
            TokenTree::Punct(p) if p.as_char() == '#' => {
                tokens.next();
            }
            TokenTree::Ident(ident) if expecting => {
                names.push(ident.to_string());
                expecting = false;
            }
            _ => {}
        }
    }
    names
}

/// How many variants the source gives enum `name`.
fn variant_count(name: &str) -> usize {
    declared_enums()
        .get(name)
        .unwrap_or_else(|| panic!("the protocol source declares no enum {name}"))
        .len()
}

// -- coverage -----------------------------------------------------------------

/// Assert `samples` hit every variant of enum `what`, as numbered by `index`:
/// every index below the enum's variant count in the source, and no other.
fn assert_covers<T>(what: &str, samples: &[T], index: fn(&T) -> usize) {
    assert_covers_n(what, samples, index, variant_count(what));
}

/// [`assert_covers`] against an explicit number of indices.
fn assert_covers_n<T>(what: &str, samples: &[T], index: fn(&T) -> usize, count: usize) {
    let seen: BTreeSet<usize> = samples.iter().map(index).collect();
    assert!(
        seen.iter().all(|&i| i < count),
        "{what}: an index is out of range — there are {count}, so the index function must \
         number them 0..{count}"
    );
    let missing: Vec<usize> = (0..count).filter(|i| !seen.contains(i)).collect();
    assert!(
        missing.is_empty(),
        "{what}: no sample for variant index(es) {missing:?} — add one to the snapshot"
    );
}

/// The enums the coverage test checks. A new enum in the protocol source
/// fails `every_protocol_enum_is_covered` until it is added here (with an
/// index function and samples).
const COVERED: &[&str] = &[
    "ChangeKind",
    "DiffKind",
    "ErrorCode",
    "Event",
    "GitOp",
    "GitResult",
    "LspResolution",
    "NotebookOutput",
    "Patch",
    "ProviderFailure",
    "Refusal",
    "Request",
    "ServerMessage",
    "StateEdit",
    "StreamOutcome",
];

fn request_index(r: &Request) -> usize {
    match r {
        Request::Hello { .. } => 0,
        Request::OpenProject { .. } => 1,
        Request::ReadFile { .. } => 2,
        Request::Search { .. } => 3,
        Request::GitInfo { .. } => 4,
        Request::ReadState { .. } => 5,
        Request::WriteState { .. } => 6,
        Request::EditState { .. } => 7,
        Request::Stats => 8,
        Request::ProjectCalls { .. } => 9,
        Request::ReadSources { .. } => 10,
        Request::Git { .. } => 11,
        Request::SpawnProcess { .. } => 12,
        Request::SpawnLsp { .. } => 13,
        Request::SpawnAdapter { .. } => 14,
        Request::LspResolve { .. } => 15,
        Request::LspInstall { .. } => 16,
        Request::LspApprovals { .. } => 17,
        Request::ProcessInput { .. } => 18,
        Request::ProcessKill { .. } => 19,
        Request::Cancel { .. } => 20,
        Request::SetAiConfig { .. } => 21,
        Request::Chat { .. } => 22,
        Request::Embed { .. } => 23,
        Request::ChatStream { .. } => 24,
        Request::AgentAsk { .. } => 25,
        Request::ListDir { .. } => 26,
        Request::BuildDocs => 27,
    }
}

fn event_index(e: &Event) -> usize {
    match e {
        Event::Ready { .. } => 0,
        Event::Tree { .. } => 1,
        Event::FileContent { .. } => 2,
        Event::NotebookContent { .. } => 3,
        Event::GitInfo { .. } => 4,
        Event::Stats { .. } => 5,
        Event::ProjectCalls { .. } => 6,
        Event::Sources { .. } => 7,
        Event::GitResult { .. } => 8,
        Event::StateContent { .. } => 9,
        Event::StateWritten { .. } => 10,
        Event::StateEdited { .. } => 11,
        Event::SearchResults { .. } => 12,
        Event::FilesChanged { .. } => 13,
        Event::ProjectSymbols { .. } => 14,
        Event::ProcessOutput { .. } => 15,
        Event::ProcessStarted { .. } => 16,
        Event::ProcessExited { .. } => 17,
        Event::AdapterSpawned { .. } => 18,
        Event::ChatResult { .. } => 19,
        Event::ChatDelta { .. } => 20,
        Event::ChatStreamDone { .. } => 21,
        Event::Embeddings { .. } => 22,
        Event::DirListing { .. } => 23,
        Event::Docs { .. } => 24,
        Event::AgentStep { .. } => 25,
        Event::AgentDelta { .. } => 26,
        Event::AgentDone { .. } => 27,
        Event::Status { .. } => 28,
        Event::LspResolved { .. } => 29,
        Event::Error { .. } => 30,
    }
}

fn git_op_index(op: &GitOp) -> usize {
    match op {
        GitOp::FileHistory { .. } => 0,
        GitOp::SymbolHistory { .. } => 1,
        GitOp::FileAt { .. } => 2,
        GitOp::AddedLines { .. } => 3,
        GitOp::CommitMessage { .. } => 4,
        GitOp::CommitFileDiff { .. } => 5,
        GitOp::DiffLines { .. } => 6,
        GitOp::ReviewBase => 7,
        GitOp::CommitSubjects { .. } => 8,
        GitOp::ChangedFiles { .. } => 9,
        GitOp::RangePatch { .. } => 10,
    }
}

fn git_result_index(r: &GitResult) -> usize {
    match r {
        GitResult::FileHistory(_) => 0,
        GitResult::SymbolHistory(_) => 1,
        GitResult::FileAt(_) => 2,
        GitResult::AddedLines(_) => 3,
        GitResult::CommitMessage(_) => 4,
        GitResult::CommitFileDiff(_) => 5,
        GitResult::DiffLines(_) => 6,
        GitResult::ReviewBase(_) => 7,
        GitResult::CommitSubjects(_) => 8,
        GitResult::ChangedFiles(_) => 9,
        GitResult::RangePatch(_) => 10,
    }
}

fn state_edit_index(e: &StateEdit) -> usize {
    match e {
        StateEdit::Upsert(_) => 0,
        StateEdit::Remove => 1,
        StateEdit::Toggle(_) => 2,
        StateEdit::Patch { .. } => 3,
    }
}

fn notebook_output_index(o: &NotebookOutput) -> usize {
    match o {
        NotebookOutput::Text { .. } => 0,
        NotebookOutput::Image { .. } => 1,
        NotebookOutput::Svg(_) => 2,
        NotebookOutput::Placeholder(_) => 3,
    }
}

fn lsp_resolution_index(r: &LspResolution) -> usize {
    match r {
        LspResolution::Ready { .. } => 0,
        LspResolution::Command(_) => 1,
        LspResolution::NeedsInstall { .. } => 2,
        LspResolution::Unsupported { .. } => 3,
    }
}

fn error_code_index(c: &ErrorCode) -> usize {
    match c {
        ErrorCode::Failed => 0,
        ErrorCode::Refused => 1,
        ErrorCode::Handshake => 2,
        ErrorCode::NotReady => 3,
        ErrorCode::Cancelled => 4,
        ErrorCode::Provider(_) => 5,
    }
}

fn provider_failure_index(f: &ProviderFailure) -> usize {
    match f {
        ProviderFailure::Status { .. } => 0,
        ProviderFailure::Unreached => 1,
        ProviderFailure::Broken => 2,
        ProviderFailure::Stream => 3,
        ProviderFailure::Unusable => 4,
        ProviderFailure::Settings => 5,
    }
}

fn stream_outcome_index(o: &StreamOutcome) -> usize {
    match o {
        StreamOutcome::Done => 0,
        StreamOutcome::Stopped => 1,
        StreamOutcome::Failed(_) => 2,
    }
}

fn refusal_index(r: &Refusal) -> usize {
    match r {
        Refusal::NotUtf8 => 0,
        Refusal::NotPlainFile => 1,
        Refusal::OutsideProject => 2,
    }
}

fn server_message_index(m: &ServerMessage) -> usize {
    match m {
        ServerMessage::Reply { .. } => 0,
        ServerMessage::Notification { .. } => 1,
    }
}

fn diff_kind_index(k: &DiffKind) -> usize {
    match k {
        DiffKind::Header => 0,
        DiffKind::Hunk => 1,
        DiffKind::Context => 2,
        DiffKind::Add => 3,
        DiffKind::Remove => 4,
    }
}

fn change_kind_index(k: &ChangeKind) -> usize {
    match k {
        ChangeKind::Added => 0,
        ChangeKind::Modified => 1,
    }
}

/// `Patch` is sampled by its three wire STATES, not its two variants:
/// `Set(None)` is its reason to exist. Its variant count is still read from
/// the source (a third variant must fail here), then mapped onto the states.
fn patch_index<T>(p: &Patch<T>) -> usize {
    match p {
        Patch::Unchanged => 0,
        Patch::Set(Some(_)) => 1,
        Patch::Set(None) => 2,
    }
}

/// The wire states of [`patch_index`], per variant: `Set` carries two.
const PATCH_STATES_PER_VARIANT: &[(&str, usize)] = &[("Unchanged", 1), ("Set", 2)];

// -- samples ------------------------------------------------------------------

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

fn git_ops() -> Vec<GitOp> {
    vec![
        GitOp::FileHistory {
            rel: "a.rs".into(),
            limit: 10,
        },
        GitOp::SymbolHistory {
            rel: "a.rs".into(),
            start: 1,
            end: 9,
            limit: 10,
        },
        GitOp::FileAt {
            sha: "abc".into(),
            rel: "a.rs".into(),
        },
        GitOp::AddedLines {
            sha: "abc".into(),
            rel: "a.rs".into(),
        },
        GitOp::CommitMessage { sha: "abc".into() },
        GitOp::CommitFileDiff {
            sha: "abc".into(),
            rel: "a.rs".into(),
            max_bytes: 4096,
        },
        GitOp::DiffLines { rel: "a.rs".into() },
        GitOp::ReviewBase,
        GitOp::CommitSubjects {
            base: "main".into(),
        },
        GitOp::ChangedFiles {
            base: "main".into(),
        },
        GitOp::RangePatch {
            base: "main".into(),
            max_bytes: 4096,
        },
    ]
}

fn state_edits() -> Vec<StateEdit> {
    let mut fields = serde_json::Map::new();
    fields.insert("note".into(), "n".into());
    vec![
        StateEdit::Upsert(serde_json::json!({"rel": "a.rs", "line": 1})),
        StateEdit::Remove,
        StateEdit::Toggle(serde_json::json!({"rel": "a.rs", "line": 1})),
        StateEdit::Patch {
            fields,
            insert: Some(serde_json::json!({"rel": "a.rs", "line": 1})),
            empty_when: vec!["note".into()],
        },
    ]
}

fn commit() -> HistCommit {
    HistCommit {
        sha: "abc".into(),
        author: "a".into(),
        time: 1,
        subject: "s".into(),
        path: "a.rs".into(),
    }
}

fn git_results() -> Vec<GitResult> {
    vec![
        GitResult::FileHistory(vec![commit()]),
        GitResult::SymbolHistory(vec![commit()]),
        GitResult::FileAt(Some("fn main() {}".into())),
        // One element: a `HashSet` serializes in no fixed order.
        GitResult::AddedLines(HashSet::from([3])),
        GitResult::CommitMessage(None),
        GitResult::CommitFileDiff("@@ -1 +1 @@".into()),
        GitResult::DiffLines(Some(vec![
            DiffLine {
                kind: DiffKind::Header,
                text: "diff --git a/a.rs b/a.rs".into(),
            },
            DiffLine {
                kind: DiffKind::Hunk,
                text: "@@ -1 +1 @@".into(),
            },
            DiffLine {
                kind: DiffKind::Context,
                text: " x".into(),
            },
            DiffLine {
                kind: DiffKind::Add,
                text: "+y".into(),
            },
            DiffLine {
                kind: DiffKind::Remove,
                text: "-z".into(),
            },
        ])),
        GitResult::ReviewBase(Some(("abc".into(), "main".into()))),
        GitResult::CommitSubjects(vec!["s".into()]),
        GitResult::ChangedFiles(vec![("a.rs".into(), 'M')]),
        GitResult::RangePatch(String::new()),
    ]
}

fn notebook_outputs() -> Vec<NotebookOutput> {
    vec![
        NotebookOutput::Text {
            spans: vec![("1".into(), Some(2))],
            stderr: false,
        },
        NotebookOutput::Image {
            data: vec![0x89, b'P', b'N', b'G'],
        },
        NotebookOutput::Svg("<svg/>".into()),
        NotebookOutput::Placeholder("widget".into()),
    ]
}

fn resolutions() -> Vec<LspResolution> {
    vec![
        LspResolution::Ready {
            init_options: Some(serde_json::json!({"a": 1})),
            withheld: None,
        },
        // The withheld shape is a sample of its own: it is what the client
        // needs to raise the approval modal, so a field silently lost from it
        // takes the only grant path with it.
        LspResolution::Ready {
            init_options: None,
            withheld: Some(LspOptionsSpec {
                server: "rust-analyzer".into(),
                version: "1".into(),
                args: vec![],
                fingerprint: "fp".into(),
                options: serde_json::json!({"a": 1}),
            }),
        },
        LspResolution::Command(LspCommandSpec {
            command: "/p/ra".into(),
            args: vec![],
            server: "rust-analyzer".into(),
            version: "1".into(),
            fingerprint: "fp".into(),
            init_options: None,
        }),
        LspResolution::NeedsInstall {
            server: "rust-analyzer".into(),
            version: "1".into(),
            describe: "d".into(),
            consent: "c0ffee".into(),
        },
        LspResolution::Unsupported {
            message: "no server".into(),
        },
    ]
}

fn error_codes() -> Vec<ErrorCode> {
    let mut codes = vec![
        ErrorCode::Failed,
        ErrorCode::Refused,
        ErrorCode::Handshake,
        ErrorCode::NotReady,
        ErrorCode::Cancelled,
    ];
    codes.extend(provider_failures().into_iter().map(ErrorCode::Provider));
    codes
}

fn provider_failures() -> Vec<ProviderFailure> {
    vec![
        ProviderFailure::Status {
            code: 404,
            kind: Some("model_not_found".into()),
            message: "The model does not exist".into(),
        },
        ProviderFailure::Status {
            code: 502,
            kind: None,
            message: "Bad Gateway".into(),
        },
        ProviderFailure::Unreached,
        ProviderFailure::Broken,
        ProviderFailure::Stream,
        ProviderFailure::Unusable,
        ProviderFailure::Settings,
    ]
}

fn stream_outcomes() -> Vec<StreamOutcome> {
    vec![
        StreamOutcome::Done,
        StreamOutcome::Stopped,
        StreamOutcome::Failed("provider error".into()),
    ]
}

fn stats_report() -> StatsReport {
    StatsReport {
        totals: Totals {
            files: 1,
            code: 2,
            comments: 3,
            blanks: 4,
        },
        langs: vec![LangStat {
            name: "Rust".into(),
            files: 1,
            code: 2,
            comments: 3,
            blanks: 4,
        }],
        top_files: vec![FileStat {
            rel: "a.rs".into(),
            lang: "Rust".into(),
            code: 2,
            lines: 9,
        }],
        skipped: 1,
    }
}

fn structure_index() -> StructureIndex {
    StructureIndex {
        by_type: BTreeMap::from([(
            "Point".into(),
            TypeStructure {
                traits: vec!["Clone".into()],
                methods: vec!["norm".into()],
            },
        )]),
        implementors: BTreeMap::from([("Clone".into(), vec!["Point".into()])]),
    }
}

fn project_symbols(
    seq: u64,
    go_module: Patch<String>,
    dart_package: Patch<String>,
    structure: Patch<StructureIndex>,
) -> Event {
    Event::ProjectSymbols {
        root: "/p".into(),
        seq,
        full: seq == 1,
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
        go_module,
        dart_package,
        structure,
    }
}

fn request_samples() -> Vec<Request> {
    let mut samples = vec![
        Request::Hello {
            protocol: PROTOCOL_VERSION,
            fingerprint: "f".into(),
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
    ];
    // Every `StateEdit` rides in an `EditState`.
    samples.extend(state_edits().into_iter().map(|edit| Request::EditState {
        root: "/p".into(),
        rel: "bookmarks.json".into(),
        merge: StateMerge {
            key_fields: vec!["rel".into(), "line".into()],
            key: vec!["a.rs".into(), 1.into()],
            edit,
            delete_when_empty: true,
        },
        edit_id: "4f1c9a2e7b30d865-12".into(),
    }));
    samples.extend([
        Request::Stats,
        Request::ProjectCalls {
            scope: vec![("a.rs".into(), vec!["b.rs".into()])],
        },
        Request::ReadSources {
            rels: vec!["a.rs".into()],
        },
    ]);
    samples.extend(git_ops().into_iter().map(|op| Request::Git { op }));
    samples.extend([
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
            consent: "c0ffee".into(),
        },
        Request::LspApprovals {
            approvals: vec![("rust".into(), "fp".into())],
        },
        Request::ProcessInput {
            proc: 1,
            data: b"Content-Length: 2\r\n\r\n{}".to_vec(),
        },
        Request::ProcessKill { proc: 1 },
        Request::Cancel { id: 1 },
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
        Request::SetAiConfig {
            chat: None,
            embed: None,
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
        Request::ListDir {
            path: Some("~".into()),
        },
        Request::BuildDocs,
    ]);
    samples
}

/// Every event, each in the envelope the server actually sends it in.
fn reply_samples() -> Vec<Event> {
    let mut samples = vec![
        Event::Ready {
            protocol: PROTOCOL_VERSION,
            fingerprint: "f".into(),
        },
        Event::Tree {
            root: "/p".into(),
            seq: 1,
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
            files: vec!["src/a.rs".into(), "README.md".into()],
            truncated: false,
            tracked_ignored: vec!["README.md".into()],
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
                outputs: notebook_outputs(),
                execution_count: Some(1),
            }],
            symbols: vec![],
            projection: "# %%\nx = 1".into(),
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
                deleted_at: HashSet::from([1]),
            }),
        },
        Event::Stats {
            root: "/p".into(),
            report: stats_report(),
        },
        Event::ProjectCalls {
            root: "/p".into(),
            graph: CallGraph {
                nodes: vec![
                    CallGraphNode {
                        name: "main".into(),
                        kind: "function".into(),
                        file: "a.rs".into(),
                        line: 1,
                        callers: vec![],
                        callees: vec![1],
                    },
                    CallGraphNode {
                        name: "helper".into(),
                        kind: "function".into(),
                        file: "a.rs".into(),
                        line: 5,
                        callers: vec![0],
                        callees: vec![],
                    },
                ],
            },
        },
        Event::Sources {
            root: "/p".into(),
            files: vec![("a.rs".into(), "fn main() {}".into())],
            missing: vec!["gone.rs".into()],
            too_large: vec![("vendor.rs".into(), 2_097_152)],
            refused: vec![
                ("latin1.rs".into(), Refusal::NotUtf8),
                ("pipe.rs".into(), Refusal::NotPlainFile),
                ("leak.rs".into(), Refusal::OutsideProject),
            ],
            unreadable: vec![("secret.rs".into(), "Permission denied (os error 13)".into())],
            deferred: vec!["next.rs".into()],
        },
    ];
    samples.extend(git_results().into_iter().map(|result| Event::GitResult {
        root: "/p".into(),
        result,
    }));
    samples.extend([
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
            skipped: vec![SkippedFile {
                rel: "dump.sql".into(),
                reason: "larger than 64 MB".into(),
            }],
            skipped_total: 1,
        },
        Event::AdapterSpawned {
            proc: 1,
            launch: "{}".into(),
        },
        Event::ChatResult { text: "t".into() },
        Event::Embeddings {
            vecs: vec![vec![0.5]],
        },
        Event::DirListing {
            path: "/home".into(),
            parent: Some("/".into()),
            entries: vec![
                DirEntry {
                    name: "p".into(),
                    is_dir: true,
                },
                DirEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                },
            ],
            omitted: 0,
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
    ]);
    samples.extend(
        resolutions()
            .into_iter()
            .map(|resolution| Event::LspResolved {
                language: "rust".into(),
                root: "/p".into(),
                resolution,
            }),
    );
    samples.extend(
        error_codes()
            .into_iter()
            .map(|code| Event::error(code, "boom")),
    );
    samples
}

/// The events the server sends unsolicited — the `Notification` envelope.
fn notification_samples() -> Vec<Event> {
    let mut samples = vec![
        // A watcher's structural rescan.
        Event::Tree {
            root: "/p".into(),
            seq: 2,
            tree: DirNode::default(),
            files: vec![],
            truncated: true,
            tracked_ignored: vec![],
        },
        Event::FilesChanged {
            root: "/p".into(),
            rels: vec!["a.rs".into()],
        },
        // All three `Patch` states, so the snapshot pins how "not
        // recomputed" and "recomputed to nothing" serialize apart.
        project_symbols(
            1,
            Patch::Set(Some("example.com/m".into())),
            Patch::Unchanged,
            Patch::Set(Some(structure_index())),
        ),
        project_symbols(2, Patch::Unchanged, Patch::Set(None), Patch::Set(None)),
        Event::ProcessOutput {
            proc: 1,
            data: vec![0, 159, 146, 150],
        },
        Event::ProcessStarted { proc: 1 },
        Event::ProcessExited {
            proc: 1,
            code: Some(0),
        },
        Event::ProcessExited {
            proc: 2,
            code: None,
        },
        Event::ChatDelta {
            stream: 1,
            text: "t".into(),
        },
    ];
    // Every way a stream can end, on the chat stream...
    samples.extend(
        stream_outcomes()
            .into_iter()
            .map(|outcome| Event::ChatStreamDone { stream: 1, outcome }),
    );
    samples.extend([
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
        // ...and one on the agent's, which shares the type.
        Event::AgentDone {
            stream: 1,
            outcome: StreamOutcome::Stopped,
        },
        Event::Status {
            message: "file watching is unavailable".into(),
        },
    ]);
    samples
}

/// Every server sample in the envelope the server actually sends it in.
fn server_messages() -> Vec<ServerMessage> {
    reply_samples()
        .into_iter()
        .map(|event| ServerMessage::Reply { id: 1, event })
        .chain(
            notification_samples()
                .into_iter()
                .map(|event| ServerMessage::Notification { event }),
        )
        .collect()
}

fn render() -> String {
    let mut lines = Vec::new();
    for request in request_samples() {
        lines.push(
            serde_json::to_string(&ClientMessage { id: 1, request }).expect("request serializes"),
        );
    }
    for msg in server_messages() {
        lines.push(serde_json::to_string(&msg).expect("server message serializes"));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn snapshot_path(version: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join(format!("wire-v{version}.snap"))
}

// -- tests --------------------------------------------------------------------

#[test]
fn every_variant_has_a_sample() {
    let events: Vec<Event> = reply_samples()
        .into_iter()
        .chain(notification_samples())
        .collect();
    assert_covers("Request", &request_samples(), request_index);
    assert_covers("Event", &events, event_index);
    assert_covers("ServerMessage", &server_messages(), server_message_index);
    assert_covers("GitOp", &git_ops(), git_op_index);
    assert_covers("GitResult", &git_results(), git_result_index);
    assert_covers("StateEdit", &state_edits(), state_edit_index);
    assert_covers("NotebookOutput", &notebook_outputs(), notebook_output_index);
    assert_covers("LspResolution", &resolutions(), lsp_resolution_index);
    assert_covers("ErrorCode", &error_codes(), error_code_index);
    // The nested enums are covered where they actually ride, not only in
    // their own sample lists.
    let outcomes: Vec<StreamOutcome> = events
        .iter()
        .filter_map(|e| match e {
            Event::ChatStreamDone { outcome, .. } | Event::AgentDone { outcome, .. } => {
                Some(outcome.clone())
            }
            _ => None,
        })
        .collect();
    assert_covers("StreamOutcome", &outcomes, stream_outcome_index);
    let codes: Vec<ErrorCode> = events
        .iter()
        .filter_map(|e| match e {
            Event::Error { code, .. } => Some(code.clone()),
            _ => None,
        })
        .collect();
    assert_covers("ErrorCode", &codes, error_code_index);
    let provider_failures: Vec<ProviderFailure> = codes
        .iter()
        .filter_map(|code| match code {
            ErrorCode::Provider(failure) => Some(failure.clone()),
            _ => None,
        })
        .collect();
    assert_covers(
        "ProviderFailure",
        &provider_failures,
        provider_failure_index,
    );
    let diff_kinds: Vec<DiffKind> = git_results()
        .into_iter()
        .filter_map(|r| match r {
            GitResult::DiffLines(Some(lines)) => Some(lines),
            _ => None,
        })
        .flatten()
        .map(|l| l.kind)
        .collect();
    assert_covers("DiffKind", &diff_kinds, diff_kind_index);
    let change_kinds: Vec<ChangeKind> = events
        .iter()
        .filter_map(|e| match e {
            Event::GitInfo {
                info: Some(info), ..
            } => Some(info.status.iter().flatten().copied()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_covers("ChangeKind", &change_kinds, change_kind_index);
    let refusals: Vec<Refusal> = events
        .iter()
        .filter_map(|e| match e {
            Event::Sources { refused, .. } => Some(refused.iter().map(|(_, why)| *why)),
            _ => None,
        })
        .flatten()
        .collect();
    assert_covers("Refusal", &refusals, refusal_index);
    let patches: Vec<Patch<()>> = events
        .iter()
        .filter_map(|e| match e {
            Event::ProjectSymbols {
                go_module,
                dart_package,
                structure,
                ..
            } => Some([shape(go_module), shape(dart_package), shape(structure)]),
            _ => None,
        })
        .flatten()
        .collect();
    let patch_variants: Vec<&str> = PATCH_STATES_PER_VARIANT.iter().map(|(v, _)| *v).collect();
    assert_eq!(
        declared_enums()["Patch"],
        patch_variants,
        "Patch's variants changed — map the new one onto its wire states"
    );
    let states = PATCH_STATES_PER_VARIANT.iter().map(|(_, n)| n).sum();
    assert_covers_n("Patch", &patches, patch_index, states);
}

/// Every enum in the protocol source is checked by the test above. Without
/// this a NEW enum — a new payload type riding inside some variant — could
/// ship with one sample, or none, and nothing would notice.
#[test]
fn every_protocol_enum_is_covered() {
    let declared: Vec<String> = declared_enums().into_keys().collect();
    assert_eq!(
        declared, COVERED,
        "an enum was added to (or removed from) the protocol: give it an index function and \
         samples, and list it in COVERED"
    );
}

/// The source reader itself: it must count what a reader of the code would.
#[test]
fn the_variant_reader_counts_what_the_source_declares() {
    let mut enums = BTreeMap::new();
    collect_enums(
        TokenStream::from_str(
            r#"
            /// Docs.
            pub enum A<T> {
                /// One.
                One,
                #[serde(rename = "zwei")]
                Two(Option<T>, (u8, u8)),
                Three { x: std::collections::HashMap<String, Vec<u8>>, y: u8 },
            }
            #[cfg(test)]
            enum Hidden { X }
            enum B { Only = 1 }
            "#,
        )
        .unwrap(),
        &mut enums,
    );
    assert_eq!(enums["A"], ["One", "Two", "Three"]);
    assert_eq!(enums["B"], ["Only"]);
    assert!(
        !enums.contains_key("Hidden"),
        "test-only enums reach no peer"
    );
    // And against the real thing, spot-checked.
    assert_eq!(variant_count("ErrorCode"), 6);
    assert_eq!(
        declared_enums()["StreamOutcome"],
        ["Done", "Stopped", "Failed"]
    );
}

/// A `Patch<T>` reduced to its state.
fn shape<T>(p: &Patch<T>) -> Patch<()> {
    match p {
        Patch::Unchanged => Patch::Unchanged,
        Patch::Set(v) => Patch::Set(v.as_ref().map(|_| ())),
    }
}

#[test]
fn wire_shape_matches_versioned_snapshot() {
    let path = snapshot_path(PROTOCOL_VERSION);
    let rendered = render();

    // Everything in the snapshot must also come back EXACTLY: a shape that
    // serializes but decodes to something else (or not at all) is broken
    // regardless of the snapshot.
    for line in rendered.lines() {
        let again = match serde_json::from_str::<ClientMessage>(line) {
            Ok(msg) => serde_json::to_string(&msg).unwrap(),
            Err(_) => {
                let msg: ServerMessage = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("sample does not decode: {e}: {line}"));
                serde_json::to_string(&msg).unwrap()
            }
        };
        assert_eq!(again, line, "sample does not round-trip");
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

/// The handshake frames of every OLDER protocol version still parse, so a
/// mismatched peer is told what is wrong ("speaks v9, this clew speaks v12")
/// instead of being hung up on for a frame it cannot decode. The committed
/// snapshots of those versions are the record of what their frames looked
/// like.
#[test]
fn older_handshakes_still_parse() {
    let mut checked = 0;
    for version in 1..PROTOCOL_VERSION {
        let Ok(snapshot) = std::fs::read_to_string(snapshot_path(version)) else {
            continue;
        };
        let hello = snapshot
            .lines()
            .find(|l| l.contains("\"Hello\""))
            .unwrap_or_else(|| panic!("wire-v{version}.snap has no Hello"));
        match serde_json::from_str::<ClientMessage>(hello) {
            Ok(ClientMessage {
                request: Request::Hello { protocol, .. },
                ..
            }) => assert_eq!(protocol, version, "v{version}'s Hello"),
            other => panic!("v{version}'s Hello no longer parses: {other:?}"),
        }
        let ready = snapshot
            .lines()
            .find(|l| l.contains("\"Ready\""))
            .unwrap_or_else(|| panic!("wire-v{version}.snap has no Ready"));
        match serde_json::from_str::<ServerMessage>(ready) {
            Ok(ServerMessage::Reply {
                event: Event::Ready { protocol, .. },
                ..
            }) => assert_eq!(protocol, version, "v{version}'s Ready"),
            other => panic!("v{version}'s Ready no longer parses: {other:?}"),
        }
        checked += 1;
    }
    assert!(checked > 0, "no older snapshot was found to check");
}

/// What each `#[serde(default)]` field decodes to when its frame leaves it out.
/// Defaults exist only where a frame really can arrive without the field: the
/// handshake (a pre-v7 peer has no fingerprint, and must still be told it is
/// the wrong version) and the client's on-disk stats cache (written before
/// `skipped` existed). Everywhere else both peers are the same build, and a
/// missing field is a bug a default would only hide.
#[test]
fn defaulted_fields_decode_as_pinned() {
    let hello: ClientMessage =
        serde_json::from_str(r#"{"id":1,"request":{"Hello":{"protocol":6}}}"#).unwrap();
    assert!(matches!(
        hello.request,
        Request::Hello { protocol: 6, ref fingerprint } if fingerprint.is_empty()
    ));
    // A v5–v11 client's Hello, `ai` field and all: the extra field is ignored.
    let hello: ClientMessage = serde_json::from_str(
        r#"{"id":1,"request":{"Hello":{"protocol":11,"fingerprint":"f","ai":"Server"}}}"#,
    )
    .unwrap();
    assert!(matches!(hello.request, Request::Hello { protocol: 11, .. }));

    let ready: ServerMessage =
        serde_json::from_str(r#"{"Reply":{"id":1,"sub":null,"event":{"Ready":{"protocol":6}}}}"#)
            .unwrap();
    assert!(matches!(
        ready,
        ServerMessage::Reply { event: Event::Ready { protocol: 6, ref fingerprint }, .. }
            if fingerprint.is_empty()
    ));

    let report: StatsReport = serde_json::from_str(
        r#"{"totals":{"files":0,"code":0,"comments":0,"blanks":0},"langs":[],"top_files":[]}"#,
    )
    .unwrap();
    assert_eq!(report.skipped, 0);
}

/// Every `#[serde(default)]` in the protocol source is one the test above
/// pins. A new one fails here until it is pinned (and justified) there.
#[test]
fn every_serde_default_is_pinned() {
    const PINNED: &[&str] = &["fingerprint", "fingerprint", "skipped"];
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !line.trim_start().starts_with("#[serde(default") {
                continue;
            }
            // The field is the next line that is not an attribute or a comment.
            let field = lines[i + 1..]
                .iter()
                .map(|l| l.trim())
                .find(|l| !l.starts_with('#') && !l.starts_with("//"))
                .and_then(|l| l.trim_start_matches("pub ").split(':').next())
                .unwrap_or_default()
                .to_string();
            found.push(field);
        }
    }
    found.sort();
    assert_eq!(found, PINNED, "a #[serde(default)] field is not pinned");
}

/// Which fields a frame may leave out and still decode — all of them, found
/// by removing each object key of every sample in turn.
///
/// `every_serde_default_is_pinned` sees the explicit `#[serde(default)]`s
/// only. serde also fills a missing `Option` field in as `None`, with no
/// attribute anywhere, so a frame whose sender dropped such a field decodes
/// as if the sender had said "none". Both peers are one build (the handshake
/// refuses any other), so only a broken peer can send that — but it is a
/// decision the wire makes, and this pins every place it is made: a new
/// `Option` field, or a new default, fails here until it is listed.
///
/// A defaulted FIELD is told apart from data by the round trip: it comes
/// back when the decoded message is encoded again (as `null`, or its default
/// value), while an entry removed from a map or from a free-form JSON value
/// does not.
#[test]
fn only_the_pinned_fields_may_be_left_out() {
    const MAY_BE_OMITTED: &[&str] = &[
        "Notification.event.AgentStep.refs[].line",
        "Notification.event.ProcessExited.code",
        "Reply.event.DirListing.parent",
        "Reply.event.Error.code.Provider.Status.kind",
        "Reply.event.GitInfo.info",
        "Reply.event.LspResolved.resolution.Command.init_options",
        "Reply.event.LspResolved.resolution.Ready.init_options",
        "Reply.event.LspResolved.resolution.Ready.withheld",
        "Reply.event.NotebookContent.cells[].execution_count",
        "Reply.event.Ready.fingerprint",
        "Reply.event.SearchResults.error",
        "Reply.event.StateContent.text",
        "Reply.event.StateEdited.text",
        "Reply.event.Stats.report.skipped",
        "request.EditState.merge.edit.Patch.insert",
        "request.Hello.fingerprint",
        "request.ListDir.path",
        "request.SetAiConfig.chat",
        "request.SetAiConfig.embed",
        "request.SpawnProcess.cwd",
        "request.WriteState.text",
    ];
    let mut found = BTreeSet::new();
    for line in render().lines() {
        let whole: serde_json::Value = serde_json::from_str(line).unwrap();
        let mut keys = Vec::new();
        object_keys(&whole, "", "", &mut keys);
        for (pointer, key, schema) in keys {
            let mut cut = whole.clone();
            cut.pointer_mut(&pointer)
                .and_then(serde_json::Value::as_object_mut)
                .expect("the parent is an object")
                .remove(&key);
            // Refused without it: a required field, as it should be.
            let Some(again) = reencode(&cut.to_string()) else {
                continue;
            };
            let again: serde_json::Value = serde_json::from_str(&again).unwrap();
            if again
                .pointer(&pointer)
                .and_then(|parent| parent.get(&key))
                .is_some()
            {
                found.insert(schema);
            }
        }
    }
    let pinned: BTreeSet<String> = MAY_BE_OMITTED.iter().map(|f| f.to_string()).collect();
    assert_eq!(
        found, pinned,
        "the fields a frame may omit changed — a new `Option` or default decodes a missing \
         field silently; list it (and mean it) or make it required"
    );
}

/// Every object key under `value`: its parent's JSON pointer, the key, and
/// its schema path (keys joined with `.`, array elements as `[]`).
fn object_keys(
    value: &serde_json::Value,
    pointer: &str,
    schema: &str,
    out: &mut Vec<(String, String, String)>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let path = if schema.is_empty() {
                    key.clone()
                } else {
                    format!("{schema}.{key}")
                };
                out.push((pointer.to_string(), key.clone(), path.clone()));
                let escaped = key.replace('~', "~0").replace('/', "~1");
                object_keys(child, &format!("{pointer}/{escaped}"), &path, out);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                object_keys(
                    child,
                    &format!("{pointer}/{i}"),
                    &format!("{schema}[]"),
                    out,
                );
            }
        }
        _ => {}
    }
}

/// `line` decoded as whichever message it is, and encoded again — `None`
/// when it decodes as neither.
fn reencode(line: &str) -> Option<String> {
    if let Ok(msg) = serde_json::from_str::<ClientMessage>(line) {
        return serde_json::to_string(&msg).ok();
    }
    let msg = serde_json::from_str::<ServerMessage>(line).ok()?;
    serde_json::to_string(&msg).ok()
}

/// The error a refused handshake carries is readable by an OLDER client: the
/// fields it knows (`message`) are where it looks, and the ones it does not
/// (`code`) are ignored by serde.
#[test]
fn a_handshake_refusal_keeps_the_message_where_old_clients_read_it() {
    let json = serde_json::to_string(&ServerMessage::Reply {
        id: 1,
        event: Event::error(ErrorCode::Handshake, "protocol mismatch"),
    })
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        v["Reply"]["event"]["Error"]["message"],
        serde_json::json!("protocol mismatch")
    );
    assert_eq!(v["Reply"]["id"], serde_json::json!(1));
}
