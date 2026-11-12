//! Embeddings made on this computer (`crate::app::embedding`): the choice
//! between them and a service, kept apart from the service's own settings;
//! EmbeddingGemma's verified download and its removal; and where the
//! vectors are made.

use super::*;
use clew_core::embed::local::{self, MODEL};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

/// Feed `task`'s messages back, and theirs, until there are none.
fn settle(app: &mut App, task: Task<Message>) {
    let mut pending = run_task(task);
    while !pending.is_empty() {
        let mut next = Vec::new();
        for message in pending {
            next.extend(run_task(app.update(message)));
        }
        pending = next;
    }
}

/// A private data directory for one test.
fn data(tag: &str) -> (PathBuf, DataDirOverride) {
    let dir = test_dir(tag);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let guard = data_dir_override(&dir);
    (dir, guard)
}

fn section() -> toml::Table {
    clew_core::globalconfig::section("embedding").unwrap_or_default()
}

fn field(table: &toml::Table, key: &str) -> Option<String> {
    table.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// "On this computer" is saved as the choice alone: the service's key,
/// model and endpoint stay as they were, and choosing the service again
/// finds them — in the form and in what is in effect.
#[test]
fn choosing_this_computer_keeps_the_service_as_it_was() {
    let (_dir, _env) = data("embed-choice");
    embed::Config::from_parts(
        "sk-kept".into(),
        "m-kept".into(),
        "http://emb.test/v1".into(),
    )
    .save()
    .unwrap();
    let mut app = blank_app();
    let _ = app.update(Message::Settings(SettingsMsg::OpenAt(
        SettingsPage::Embeddings,
    )));
    assert!(!app.settings.embed_here);
    assert_eq!(app.settings.embed_model, "m-kept");
    let _ = app.update(Message::Settings(SettingsMsg::EmbedHere(true)));
    assert!(app.settings_drafts_changed(), "a change to save");
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    assert!(!app.settings.open);
    let stored = section();
    assert_eq!(field(&stored, "source").as_deref(), Some("local"));
    assert_eq!(field(&stored, "api_key").as_deref(), Some("sk-kept"));
    assert_eq!(field(&stored, "model").as_deref(), Some("m-kept"));
    assert_eq!(
        field(&stored, "base_url").as_deref(),
        Some("http://emb.test/v1")
    );
    let now = embed::Config::current_or_default();
    assert!(now.is_local());
    assert_eq!(now.space(), embed::stored_space());
    assert_eq!(embed::stored_space().model, MODEL.key());
    // Not downloaded: nothing to make vectors with yet.
    assert!(!app.embed_available);
    // Opened again: the choice, and the service's fields under it.
    let _ = app.update(Message::Settings(SettingsMsg::Open));
    assert!(app.settings.embed_here);
    assert_eq!(app.settings.embed_key, "sk-kept");
    assert_eq!(app.settings.embed_base_url, "http://emb.test/v1");
    // Nothing changed: nothing written.
    assert!(!app.settings_drafts_changed());
    let _ = app.update(Message::Settings(SettingsMsg::EmbedHere(false)));
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    let now = embed::Config::current_or_default();
    assert!(!now.is_local());
    assert_eq!(
        (
            now.api_key.as_str(),
            now.model.as_str(),
            now.base_url.as_str()
        ),
        ("sk-kept", "m-kept", "http://emb.test/v1")
    );
    assert!(app.embed_available, "the service, with its key");
}

/// A `source` written by hand in other words ("Local") is read as what it
/// says, and changed from Settings like any other — not kept as another
/// window's; one that names neither is the service, and is changed too.
#[test]
fn a_source_written_by_hand_is_read_and_changed() {
    let (_dir, _env) = data("embed-hand");
    for (written, here) in [("Local", true), (" local ", true), ("lokal", false)] {
        let mut emb = toml::Table::new();
        emb.insert("source".into(), written.into());
        clew_core::globalconfig::update("embedding", emb).unwrap();
        let mut app = blank_app();
        let _ = app.update(Message::Settings(SettingsMsg::OpenAt(
            SettingsPage::Embeddings,
        )));
        assert_eq!(app.settings.embed_here, here, "{written}");
        let _ = app.update(Message::Settings(SettingsMsg::EmbedHere(!here)));
        let _ = app.update(Message::Settings(SettingsMsg::Close));
        assert!(!app.status.contains("another window"), "{}", app.status);
        assert_eq!(embed::Config::stored_here(), !here, "{written}");
    }
}

/// Moving to vectors made here is a move to another space: the index held
/// in memory, made with the service, is let go.
#[test]
fn moving_to_this_computer_lets_the_old_index_go() {
    let (_dir, _env) = data("embed-space");
    embed::Config::from_parts("sk".into(), "m".into(), "http://emb.test/v1".into())
        .save()
        .unwrap();
    let mut app = blank_app();
    app.proj.embed_index = embed::Index {
        model: "m".into(),
        base_url: "http://emb.test/v1".into(),
        entries: vec![embed::Entry {
            node: clew_core::explain::Node::File("cart.py".into()),
            hash: 1,
            vec: vec![1.0, 0.0],
        }],
    };
    let _ = app.update(Message::Settings(SettingsMsg::OpenAt(
        SettingsPage::Embeddings,
    )));
    let _ = app.update(Message::Settings(SettingsMsg::EmbedHere(true)));
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    assert!(app.proj.embed_index.entries.is_empty());
    assert!(app.status.contains("cleared"), "{}", app.status);
}

/// Every file a fetch was asked for, and the most it might be.
static FETCHED: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

fn junk_fetch(
    url: &str,
    max_bytes: u64,
    _cancel: &AtomicBool,
    sink: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    FETCHED.lock().unwrap().push((url.to_string(), max_bytes));
    std::thread::sleep(std::time::Duration::from_millis(200));
    sink.write_all(b"not the file").map_err(|e| e.to_string())?;
    progress(12, None);
    Ok(12)
}

/// Each of the model's files written, as if downloaded.
fn install_all(dir: &Path) {
    for file in MODEL.files {
        let path = dir.join(file.path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }
}

/// EmbeddingGemma is downloaded file by file — only those not here yet,
/// each from its pinned revision and capped at its published size — and
/// each is verified: one that is not as published is refused and leaves
/// nothing. Cancel stops it; Remove deletes it, and while it is being
/// downloaded it is not removed.
#[test]
fn the_model_downloads_verified_and_is_removed() {
    let (_data, _env) = data("embed-download");
    let dir = local::dir().unwrap();
    install_all(&dir);
    let last = MODEL.files.last().unwrap();
    std::fs::remove_file(dir.join(last.path)).unwrap();
    let mut app = blank_app();
    assert!(!app.embed_model_installed());
    app.voice.fetch = junk_fetch;
    FETCHED.lock().unwrap().clear();
    let task = app.update(Message::Settings(SettingsMsg::DownloadEmbedModel));
    assert!(app.local_embedding.download.is_some());
    // Twice is once.
    let again = app.update(Message::Settings(SettingsMsg::DownloadEmbedModel));
    settle(&mut app, again);
    // Not removed while it downloads.
    let removing = app.update(Message::Settings(SettingsMsg::RemoveEmbedModel));
    settle(&mut app, removing);
    assert!(app.status.contains("already downloading"), "{}", app.status);
    assert!(dir.join(MODEL.files[0].path).exists());
    settle(&mut app, task);
    assert_eq!(
        FETCHED.lock().unwrap().as_slice(),
        [(MODEL.url(last), last.bytes)]
    );
    assert!(MODEL.url(last).contains(MODEL.revision));
    let error = app.local_embedding.error.clone().unwrap();
    assert!(error.contains("does not match the published"), "{error}");
    assert!(app.local_embedding.download.is_none());
    assert!(!dir.join(last.path).exists());
    let leftovers: Vec<_> = walk(&dir)
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty(), "nothing half-written: {leftovers:?}");
    // Cancelled: whatever it says after is nobody's.
    let task = app.update(Message::Settings(SettingsMsg::DownloadEmbedModel));
    assert!(
        app.local_embedding.error.is_none(),
        "a new try says nothing yet"
    );
    let _ = app.update(Message::Settings(SettingsMsg::CancelEmbedDownload));
    assert!(app.local_embedding.download.is_none());
    let before = app.status.clone();
    settle(&mut app, task);
    assert_eq!(app.status, before);
    // All here: it is installed, and removed.
    install_all(&dir);
    assert!(app.embed_model_installed());
    let task = app.update(Message::Settings(SettingsMsg::RemoveEmbedModel));
    settle(&mut app, task);
    assert!(!dir.exists());
    assert!(!app.embed_model_installed());
    assert!(app.status.contains("Removed"), "{}", app.status);
}

/// Every file under `dir`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// Vectors made on this computer are made by its model: a server on this
/// computer is handed the config (its agent embeds its searches with it),
/// one on another computer is not — it has no model. A service's config
/// goes to either.
#[test]
fn a_server_elsewhere_is_not_handed_vectors_made_here() {
    let here = embed::Config::local();
    let service = embed::Config::from_parts("sk".into(), "m".into(), "http://emb.test/v1".into());
    let on = |cfg: &embed::Config, remote| {
        crate::app::server::embed_for_server(Some(cfg.clone()), remote).map(|c| c.base_url)
    };
    assert_eq!(on(&here, false).as_deref(), Some(local::BASE_URL));
    assert_eq!(on(&here, true), None);
    assert_eq!(on(&service, true).as_deref(), Some("http://emb.test/v1"));
    assert_eq!(on(&service, false).as_deref(), Some("http://emb.test/v1"));
    assert!(crate::app::server::embed_for_server(None, false).is_none());
}

/// `PATH` with `bin` before what it was, and without any folder that holds
/// a `clew-speak` — for as long as the guard lives.
#[cfg(unix)]
fn path_with(bin: Option<&Path>) -> clew_core::testutil::EnvVars {
    let old = std::env::var_os("PATH").unwrap_or_default();
    let kept = std::env::split_paths(&old).filter(|d| !d.join("clew-speak").exists());
    let dirs: Vec<PathBuf> = bin.map(Path::to_path_buf).into_iter().chain(kept).collect();
    clew_core::testutil::EnvVars::new().set("PATH", std::env::join_paths(dirs).unwrap())
}

/// Why no embeddings can be made is said as what to do about it — the
/// model to download, then the program to put beside Clew — where they are
/// asked for: a search, a build of the index, a download just finished.
#[cfg(unix)]
#[test]
fn what_is_missing_to_make_embeddings_is_said() {
    let (_dir, _env) = data("embed-missing");
    let _path = path_with(None);
    // OpenAI's endpoint, below, takes its key from here too.
    let _key = clew_core::testutil::EnvVars::new().remove("OPENAI_API_KEY");
    assert!(local::program().is_none(), "no clew-speak to be found");
    embed::Config::local().save().unwrap();
    let mut app = blank_app();
    app.proj.semantic_query = "where is the total".into();
    let download = format!("Download {} in Settings → Embeddings", MODEL.name);
    let _ = app.update(Message::Semantic(SemanticMsg::Search));
    assert_eq!(app.status, download);
    app.status.clear();
    let _ = app.update(Message::Semantic(SemanticMsg::BuildIndex));
    assert_eq!(app.status, download);
    // A question asked with nothing else to answer from: Ask's card says
    // so too, known to be about the embeddings (its Settings button).
    let _ = app.on_ask_submit_rag("where is the total".into(), false);
    assert_eq!(app.proj.ask_notice.as_deref(), Some(download.as_str()));
    assert!(app.proj.ask_notice_embeddings);
    // Downloaded: the program is what is missing.
    let program =
        "clew-speak, which makes embeddings on this computer, is not installed beside Clew";
    install_all(&local::dir().unwrap());
    let _ = app.update(Message::Semantic(SemanticMsg::Search));
    assert_eq!(app.status, program);
    app.status.clear();
    let _ = app.update(Message::Semantic(SemanticMsg::BuildIndex));
    assert_eq!(app.status, program);
    // A download with nothing left to fetch ends at once.
    let task = app.update(Message::Settings(SettingsMsg::DownloadEmbedModel));
    settle(&mut app, task);
    assert_eq!(app.status, program, "not that it is ready");
    // A service without its endpoint: that is what to set.
    embed::Config::from_parts(String::new(), "m".into(), String::new())
        .save()
        .unwrap();
    let _ = app.update(Message::Semantic(SemanticMsg::Search));
    assert_eq!(app.status, "Configure an embedding endpoint in Settings");
    // This computer chosen in Settings, not yet saved: a download that
    // ends says what this computer is missing, not the service's.
    let _ = app.update(Message::Settings(SettingsMsg::OpenAt(
        SettingsPage::Embeddings,
    )));
    let _ = app.update(Message::Settings(SettingsMsg::EmbedHere(true)));
    let task = app.update(Message::Settings(SettingsMsg::DownloadEmbedModel));
    settle(&mut app, task);
    assert_eq!(app.status, program);
}

/// Vectors no longer made here — a service chosen — and the model this
/// window holds in memory goes now, not once it has been idle a while.
#[cfg(unix)]
#[test]
fn choosing_a_service_lets_the_model_go() {
    let (dir, _env) = data("embed-let-go");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    clew_core::testutil::fake_embedder(&bin.join("clew-speak"), "vectors");
    let _path = path_with(Some(&bin));
    install_all(&local::dir().unwrap());
    embed::Config::from_parts("sk".into(), "m".into(), "http://emb.test/v1".into())
        .save()
        .unwrap();
    embed::Config::local().save().unwrap();
    let held = local::embed(&["the total".into()], local::Purpose::Query, &|| false);
    assert_eq!(held.unwrap().len(), 1);
    let mut app = blank_app();
    let _ = app.update(Message::Settings(SettingsMsg::OpenAt(
        SettingsPage::Embeddings,
    )));
    let _ = app.update(Message::Settings(SettingsMsg::EmbedHere(false)));
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    let still_held = local::let_go();
    local::unload();
    assert!(
        !still_held,
        "the model was kept after the service was chosen"
    );
}

/// From a service to this computer and back: the service's vectors are
/// found as they were, and a build reuses them rather than paying the
/// service for every one again.
#[test]
fn going_back_to_the_service_finds_its_index() {
    // Nothing listens on port 1: a vector asked of the service fails the build.
    let endpoint = "http://127.0.0.1:1/v1";
    let _env = embeddings_at("embed-round-trip-data", endpoint);
    let mut app = scanned_app("embed-round-trip");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let store = app.proj.derived_dir.clone().unwrap();
    app.proj.explain.cache.insert(
        explain::Node::File(root.join("src/lib.rs")),
        explain::Cached {
            summary: "Defines a point and its origin.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    let nodes = app.gather_embed_nodes();
    let index = |cfg: &embed::Config, vec: Vec<f32>| embed::Index {
        model: cfg.model.clone(),
        base_url: cfg.base_url.clone(),
        entries: nodes
            .iter()
            .map(|(node, _, hash)| embed::Entry {
                node: node.clone(),
                hash: *hash,
                vec: vec.clone(),
            })
            .collect(),
    };
    let service = embed::Config::current_or_default();
    let service_index = index(&service, vec![1.0, 0.0]);
    embed::save(&store, &service_index).unwrap();
    app.proj.embed_index = service_index;
    // To this computer, where an index is built and saved…
    let _ = app.update(Message::Settings(SettingsMsg::OpenAt(
        SettingsPage::Embeddings,
    )));
    let _ = app.update(Message::Settings(SettingsMsg::EmbedHere(true)));
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    assert!(app.proj.embed_index.entries.is_empty());
    let (_, saved) = embed::merge_built(
        &store,
        &root,
        &index(&embed::Config::local(), vec![0.0, 1.0, 0.0]),
    );
    saved.unwrap();
    // …and back.
    let _ = app.update(Message::Settings(SettingsMsg::OpenAt(
        SettingsPage::Embeddings,
    )));
    let _ = app.update(Message::Settings(SettingsMsg::EmbedHere(false)));
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    assert!(app.proj.embed_index.entries.is_empty());
    let build = app.update(Message::Semantic(SemanticMsg::BuildIndex));
    settle(&mut app, build);
    assert!(!app.proj.building_embeddings);
    let vecs: Vec<Vec<f32>> = app
        .proj
        .embed_index
        .entries
        .iter()
        .map(|e| e.vec.clone())
        .collect();
    assert_eq!(vecs, [vec![1.0, 0.0]], "{}", app.status);
    assert_eq!(app.proj.embed_index.space(), service.space());
}

/// A build running when another source is chosen stops at once, at the
/// endpoint — every batch still to come would be paid for and thrown away,
/// its vectors of the space left — not left running to its end.
#[test]
fn choosing_another_source_stops_the_build_running() {
    use std::time::{Duration, Instant};
    let (base, arrived, closed) = clew_core::testutil::silent_http_endpoint();
    let _env = embeddings_at("embed-build-switch-data", &base);
    let mut app = scanned_app("embed-build-switch");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    app.proj.explain.cache.insert(
        explain::Node::File(root.join("src/lib.rs")),
        explain::Cached {
            summary: "Defines a point and its origin.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    let build = app.update(Message::Semantic(SemanticMsg::BuildIndex));
    assert!(app.proj.building_embeddings, "{}", app.status);
    let running = std::thread::spawn(move || run_task(build));
    arrived
        .recv_timeout(Duration::from_secs(10))
        .expect("the build never reached the endpoint");
    let switched_at = Instant::now();
    let _ = app.update(Message::Settings(SettingsMsg::OpenAt(
        SettingsPage::Embeddings,
    )));
    let _ = app.update(Message::Settings(SettingsMsg::EmbedHere(true)));
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    assert!(!app.proj.building_embeddings);
    assert!(app.status.contains("cleared"), "{}", app.status);
    let (gone, at) = closed
        .recv_timeout(Duration::from_secs(5))
        .expect("the build kept its connection");
    assert!(gone);
    assert!(
        at.saturating_duration_since(switched_at) < Duration::from_secs(2),
        "the connection closed {:?} after the switch",
        at.saturating_duration_since(switched_at)
    );
    assert!(
        running.join().expect("the build's task").is_empty(),
        "the stopped build still delivered a result"
    );
    // Its answer, had it been on its way already, is nobody's.
    let late = app.update(Message::Semantic(SemanticMsg::IndexBuilt {
        stamp: app.stamp(),
        result: Ok(embed::Index {
            model: "m".into(),
            base_url: base.clone(),
            entries: vec![embed::Entry {
                node: explain::Node::File(root.join("src/lib.rs")),
                hash: 1,
                vec: vec![1.0, 0.0],
            }],
        }),
    }));
    assert!(run_task(late).is_empty(), "merged, though stopped");
    assert!(app.proj.embed_index.entries.is_empty());
    assert!(!app.proj.building_embeddings);
}

/// A build whose vectors are all made, being merged into its own space's
/// file, is let finish when another source is chosen — what it paid for is
/// kept there — and a build asked for meanwhile waits for it, as any does.
#[test]
fn a_build_being_merged_is_let_finish_when_the_source_changes() {
    let _env = embeddings_at("embed-merge-switch-data", "http://127.0.0.1:1/v1");
    let mut app = scanned_app("embed-merge-switch");
    // Its vectors made: no task left to stop, the merge on its way.
    app.proj.building_embeddings = true;
    assert!(app.proj.inflight.embed_build.is_none());
    let _ = app.update(Message::Settings(SettingsMsg::OpenAt(
        SettingsPage::Embeddings,
    )));
    let _ = app.update(Message::Settings(SettingsMsg::EmbedHere(true)));
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    assert!(app.proj.building_embeddings, "a merge is not stopped");
    let _ = app.update(Message::Semantic(SemanticMsg::BuildIndex));
    assert!(app.proj.embed_build_pending, "a build waits for the merge");
    let landed = app.update(Message::Semantic(SemanticMsg::IndexMerged {
        stamp: app.stamp(),
        index: Handoff::new(embed::Index::default()),
        saved: Ok(()),
    }));
    assert!(!app.proj.building_embeddings);
    assert!(
        run_task(landed)
            .iter()
            .any(|m| matches!(m, Message::Semantic(SemanticMsg::BuildIndex))),
        "the waiting build goes once the merge lands"
    );
}

/// Another window chose another source: this one tells its server when it
/// comes to the front, so the server's agent does not go on making vectors
/// with the one left — paying a service, or holding a model.
#[test]
fn the_server_hears_of_another_windows_source_on_focus() {
    let _env = embeddings_at("embed-focus-server-data", "http://emb-a.test/v1");
    let mut app = scanned_app("embed-focus-server");
    let mut rx = attach_server(&mut app);
    embed::Config::from_parts("sk".into(), "m".into(), "http://emb-b.test/v1".into())
        .save()
        .unwrap();
    while rx.try_recv().is_ok() {}
    let _ = app.update(Message::Window(WindowMsg::FocusChanged(true)));
    let told = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|m| match m.request {
        clew_protocol::Request::SetAiConfig { embed, .. } => Some(embed),
        _ => None,
    });
    let told = told.expect("the server was not told");
    assert_eq!(
        told.map(|e| e.base_url).as_deref(),
        Some("http://emb-b.test/v1")
    );
}

/// Asked with no way to make embeddings — the model not here — Ask says
/// what is missing, not to build an index it may well have.
#[cfg(unix)]
#[test]
fn ask_says_what_is_missing_to_make_embeddings() {
    let (_dir, _env) = data("embed-ask");
    let _path = path_with(None);
    embed::Config::local().save().unwrap();
    let mut app = blank_app();
    app.proj.embed_index = embed::Index {
        model: MODEL.key(),
        base_url: local::BASE_URL.into(),
        entries: vec![embed::Entry {
            node: clew_core::explain::Node::File("cart.py".into()),
            hash: 1,
            vec: vec![1.0, 0.0],
        }],
    };
    let _ = app.on_ask_submit_rag("where is the total?".into(), false);
    assert_eq!(
        app.status,
        format!("Download {} in Settings → Embeddings", MODEL.name)
    );
}

/// The model downloaded in another window: this one sees it when it comes
/// to the front — embeddings can be made, and none are said to be missing.
#[cfg(unix)]
#[test]
fn a_model_another_window_downloaded_is_seen_on_focus() {
    let (dir, _env) = data("embed-focus");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    clew_core::testutil::fake_embedder(&bin.join("clew-speak"), "vectors");
    let _path = path_with(Some(&bin));
    embed::Config::local().save().unwrap();
    let mut app = blank_app();
    app.refresh_embeddings();
    assert!(!app.embed_available);
    install_all(&local::dir().unwrap());
    let _ = app.update(Message::Window(WindowMsg::FocusChanged(true)));
    assert!(app.embed_available, "{}", app.embed_missing);
}

/// Cancelled, a download can be asked for again at once — not refused as
/// "already downloading in another window" while the cancelled transfer
/// winds down. So for every model downloaded here.
#[test]
fn a_cancelled_download_can_be_asked_for_again_at_once() {
    let (dir, _env) = data("embed-again");
    let mut app = blank_app();
    app.voice.store = Some(clew_voice::models::Store::new(dir.join("voice")));
    app.voice.fetch = junk_fetch;
    // Each first transfer is started and not yet run: what it holds, it
    // holds still, as one winding down does.
    let first = app.update(Message::Settings(SettingsMsg::DownloadEmbedModel));
    let _ = app.update(Message::Settings(SettingsMsg::CancelEmbedDownload));
    let again = app.update(Message::Settings(SettingsMsg::DownloadEmbedModel));
    assert!(app.local_embedding.download.is_some(), "{}", app.status);
    let _ = app.update(Message::Settings(SettingsMsg::CancelEmbedDownload));
    drop((first, again));

    let id = clew_voice::models::MODELS[0].id;
    let first = app.update(Message::Voice(VoiceMsg::Download(id)));
    let _ = app.update(Message::Voice(VoiceMsg::CancelDownload(id)));
    let again = app.update(Message::Voice(VoiceMsg::Download(id)));
    assert!(app.voice.downloads.contains_key(id), "{}", app.status);
    let _ = app.update(Message::Voice(VoiceMsg::CancelDownload(id)));
    drop((first, again));

    let id = clew_voice::speech::VOICE_MODELS[0].id;
    let first = app.update(Message::Voice(VoiceMsg::DownloadSpeech(id)));
    let _ = app.update(Message::Voice(VoiceMsg::CancelSpeechDownload(id)));
    let again = app.update(Message::Voice(VoiceMsg::DownloadSpeech(id)));
    assert!(
        app.voice.speech.downloads.contains_key(id),
        "{}",
        app.status
    );
    let _ = app.update(Message::Voice(VoiceMsg::CancelSpeechDownload(id)));
    drop((first, again));
}

/// A transfer cancelled as it ended is not put in place — the model may be
/// being removed by then — so it is never checked or renamed in.
#[test]
fn a_transfer_cancelled_as_it_ended_is_not_put_in_place() {
    fn cancels_as_it_ends(
        _url: &str,
        _max_bytes: u64,
        cancel: &AtomicBool,
        sink: &mut dyn std::io::Write,
        _progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<u64, String> {
        sink.write_all(b"not the file").map_err(|e| e.to_string())?;
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(12)
    }
    let (_data, _env) = data("embed-cancel-ends");
    let mut app = blank_app();
    app.voice.fetch = cancels_as_it_ends;
    let task = app.update(Message::Settings(SettingsMsg::DownloadEmbedModel));
    settle(&mut app, task);
    let error = app.local_embedding.error.clone().unwrap_or_default();
    assert!(
        !error.contains("does not match the published"),
        "it was checked: {error}"
    );
    let dir = local::dir().unwrap();
    assert!(!MODEL.files.iter().any(|f| dir.join(f.path).exists()));
}
