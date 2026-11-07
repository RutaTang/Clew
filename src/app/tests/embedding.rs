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
    assert_eq!(embed::stored_space().model, MODEL.id);
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
    assert!(error.contains("as published"), "{error}");
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
