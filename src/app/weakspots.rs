//! Weak spots: the project's functions that ask for attention first (see
//! [`clew_core::weakspots`]) — measured here for a local project, on the
//! files read from its host for a remote one — ranked with how often each
//! file changes, and shown as a page over the code: each spot a way to its
//! code, and a question to Ask about it.
//!
//! Its messages, [`WeakMsg`], arrive through `App::update_weak`.

use std::path::Path;
use std::sync::Arc;

use clew_core::weakspots::{self, Spot};

use crate::app::prelude::*;
use crate::*;

/// The most spots the page lists.
pub(crate) const SHOWN: usize = 150;
/// The most source files one pass measures.
const MAX_FILES: usize = 6_000;
/// A file larger than this is not measured: generated code, a vendored
/// bundle.
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// The weak spots page.
#[derive(Debug, Default)]
pub struct WeakSpots {
    /// The page covers the code.
    pub(crate) showing: bool,
    /// The spots, ranked, once measured.
    pub(crate) spots: Option<Arc<Vec<Spot>>>,
    /// How many files they were measured in.
    pub(crate) files: usize,
    /// A pass is running.
    pub(crate) building: bool,
    /// Why the last pass brought nothing.
    pub(crate) failed: Option<String>,
    /// The file registry's revision the spots were measured at: a pass is
    /// run again once it moves.
    pub(crate) rev: u64,
}

/// The files of `files` worth measuring: code, not tests, not data — each
/// with its language — the first [`MAX_FILES`] of them.
fn code_files(
    files: &[clew_core::fs_scan::FileEntry],
) -> Vec<(String, std::path::PathBuf, &'static str)> {
    files
        .iter()
        .filter(|f| !weakspots::is_test_path(&f.rel))
        .filter_map(|f| {
            let lang = highlight::detect(Path::new(&f.rel))?;
            (!matches!(
                lang,
                "markdown" | "json" | "toml" | "yaml" | "css" | "html" | "text" | "xml" | "sql"
            ))
            .then(|| (f.rel.clone(), f.abs.clone(), lang))
        })
        .take(MAX_FILES)
        .collect()
}

/// Measure each `(rel, text, lang)` on a few threads.
fn measure_all(files: Vec<(String, String, &'static str)>) -> Vec<Spot> {
    let threads = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .clamp(1, 8);
    let chunk = files.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .flat_map(|(rel, text, lang)| weakspots::scan_file(rel, text, lang))
                        .collect::<Vec<Spot>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

impl App {
    /// Show the page, measuring the project when it has not been measured
    /// since its files changed.
    fn on_weak_show(&mut self) -> Task<Message> {
        self.proj.weak.showing = true;
        self.proj.overview.showing = false;
        self.proj.stats.showing = false;
        self.proj.glossary.showing = false;
        self.proj.docs.page = None;
        let fresh =
            self.proj.weak.spots.is_some() && self.proj.weak.rev == self.proj.registry.revision();
        if fresh {
            return Task::none();
        }
        self.start_weak()
    }

    /// Measure the project's files: read here, or from the host of a
    /// remote project; then rank them with each file's churn.
    pub(crate) fn start_weak(&mut self) -> Task<Message> {
        if self.proj.weak.building {
            return Task::none();
        }
        let Some(project) = self.proj.project.as_ref() else {
            return Task::none();
        };
        let root = project.root.clone();
        let files = code_files(&project.files);
        let rev = self.proj.registry.revision();
        self.proj.weak.building = true;
        self.proj.weak.failed = None;
        self.proj.weak.rev = rev;
        self.status = tr!("Looking for weak spots…").into();
        let churn: Vec<(String, u32)> = self
            .proj
            .churn
            .as_ref()
            .map(|c| {
                c.by_file
                    .iter()
                    .filter_map(|(abs, (commits, _))| {
                        Some((
                            clew_core::rel_string(abs.strip_prefix(&root).ok()?),
                            *commits,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let rank = move |spots: Vec<Spot>| {
            let churn: std::collections::HashMap<String, u32> = churn.into_iter().collect();
            weakspots::rank(spots, |rel| churn.get(rel).copied(), SHOWN)
        };
        let stamp = self.stamp();
        let count = files.len();
        let done = move |result| {
            Message::Weak(WeakMsg::Built {
                stamp: stamp.clone(),
                rev,
                files: count,
                result,
            })
        };
        if !self.local_project_state() {
            let ai = self.ai_client();
            let rels: Vec<String> = files.iter().map(|(rel, ..)| rel.clone()).collect();
            let langs: std::collections::HashMap<String, &'static str> = files
                .into_iter()
                .map(|(rel, _, lang)| (rel, lang))
                .collect();
            return Task::perform(
                async move {
                    let ai = &ai;
                    let ask = move |rels| ai.request(clew_protocol::Request::ReadSources { rels });
                    let host = crate::sources::read(rels, ask).await;
                    if let (Some((rel, why)), true) = (host.unread.first(), host.files.is_empty()) {
                        return Err(match why {
                            Some(why) => tr!(
                                "could not read {file} from the remote host: {reason}",
                                file = rel,
                                reason = why
                            ),
                            None => tr!("could not read {file} from the remote host", file = rel),
                        });
                    }
                    let texts: Vec<(String, String, &'static str)> = host
                        .files
                        .into_iter()
                        .filter_map(|(rel, text)| {
                            let lang = *langs.get(&rel)?;
                            Some((rel, text, lang))
                        })
                        .collect();
                    tokio::task::spawn_blocking(move || rank(measure_all(texts)))
                        .await
                        .map_err(|_| tr!("the weak spots pass failed unexpectedly").to_string())
                },
                done,
            );
        }
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let texts: Vec<(String, String, &'static str)> = files
                        .into_iter()
                        .filter_map(|(rel, abs, lang)| {
                            let bytes = clew_core::fs_scan::read_confined_bytes_capped(
                                &root,
                                &abs,
                                MAX_FILE_BYTES,
                            )?;
                            Some((rel, String::from_utf8(bytes).ok()?, lang))
                        })
                        .collect();
                    rank(measure_all(texts))
                })
                .await
                .map_err(|_| tr!("the weak spots pass failed unexpectedly").to_string())
            },
            done,
        )
    }

    /// A pass's spots, for the registry revision `rev` it measured.
    fn on_weak_built(
        &mut self,
        rev: u64,
        files: usize,
        result: Result<Vec<Spot>, String>,
    ) -> Task<Message> {
        if rev != self.proj.weak.rev {
            return Task::none();
        }
        self.proj.weak.building = false;
        match result {
            Ok(spots) => {
                // "0 weak spots found" read as a count of nothing.
                self.status = if spots.is_empty() {
                    tr!("No weak spots").into()
                } else {
                    trn!("1 weak spot found", "{n} weak spots found", spots.len())
                };
                self.proj.weak.files = files;
                self.proj.weak.spots = Some(Arc::new(spots));
            }
            Err(why) => {
                self.status = tr!("No weak spots: {why}", why = why.clone());
                self.proj.weak.failed = Some(why);
            }
        }
        Task::none()
    }

    /// The `i`th spot.
    fn weak_spot(&self, i: usize) -> Option<Spot> {
        self.proj.weak.spots.as_ref()?.get(i).cloned()
    }

    /// Handle a [`WeakMsg`].
    pub(crate) fn update_weak(&mut self, message: WeakMsg) -> Task<Message> {
        match message {
            WeakMsg::Show => self.on_weak_show(),
            WeakMsg::Refresh => self.start_weak(),
            WeakMsg::Built {
                rev, files, result, ..
            } => self.on_weak_built(rev, files, result),
            WeakMsg::Open(i) => {
                let Some(spot) = self.weak_spot(i) else {
                    return Task::none();
                };
                let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                // Opening the file puts the page away, as it does every page.
                self.open_file(root.join(&spot.rel), Some(spot.line), true)
            }
            WeakMsg::AskWhy(i) => {
                let Some(spot) = self.weak_spot(i) else {
                    return Task::none();
                };
                let place = format!("{}:{}", spot.rel, spot.line);
                let reasons: Vec<String> =
                    spot.reasons.iter().map(crate::ui::reason_words).collect();
                self.proj.ask_input = if spot.name.is_empty() {
                    tr!(
                        "Review the notes left in {place} ({reasons}): what do they say is wrong, and does the code bear it out?",
                        place = place,
                        reasons = reasons.join(", ")
                    )
                } else {
                    tr!(
                        "Review {name} at {place} ({reasons}): is it risky, what could go wrong, and how could it be made safer or simpler?",
                        name = spot.name,
                        place = place,
                        reasons = reasons.join(", ")
                    )
                };
                self.reveal_ask();
                self.on_ask_submit()
            }
            WeakMsg::Close => {
                self.proj.weak.showing = false;
                Task::none()
            }
        }
    }
}
