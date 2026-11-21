//! Clew's price list: what each model's calls cost, per million tokens.
//!
//! One list, `assets/prices.json`, is built into Clew and kept in the
//! repository, where it is corrected when a provider changes its prices.
//! With the reader's consent to update checks, Clew fetches the
//! repository's copy at most once a day (from the update check's host, when
//! Clew starts and when the reader checks by hand) and keeps it in the data
//! directory, so a new price reaches readers without a new release.
//!
//! A copy is data, never code: it is read as JSON, every row is checked,
//! and it is used only when it was checked more recently than the list in
//! use. Offline, or with update checks off, Clew prices with the built-in
//! list or the last copy it kept. The reader's own prices, set in Settings,
//! come before any of them (`usage::price_at`).

use crate::app::usage::Price;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

/// The list's format. A copy in another format is not read.
const SCHEMA: u64 = 1;

/// The largest copy accepted: the list is a few kilobytes.
const MAX_BYTES: u64 = 256 * 1024;

/// The most models a copy may list.
const MAX_MODELS: usize = 2_000;

/// The highest rate believed, in US dollars per million tokens. The most
/// expensive model listed costs a few hundred.
const MAX_RATE: f64 = 10_000.0;

/// How often the repository's copy is fetched at most.
const REFRESH_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// The list Clew was built with.
const BUILT_IN: &str = include_str!("../../assets/prices.json");

/// The repository's copy: the update check's host, the main branch.
const PATH_IN_REPO: &str = "assets/prices.json";

/// The kept copy, in the data directory.
const KEPT: &str = "prices.json";

#[derive(serde::Deserialize)]
struct Doc {
    schema: u64,
    checked: String,
    models: Vec<Row>,
}

#[derive(serde::Deserialize)]
struct Row {
    id: String,
    input: f64,
    cached: f64,
    output: f64,
    #[serde(default)]
    changes: Vec<Change>,
}

#[derive(serde::Deserialize)]
struct Change {
    from: String,
    input: f64,
    cached: f64,
    output: f64,
}

/// A price list, checked and ready to price with.
#[derive(Debug)]
pub(crate) struct List {
    /// The day its prices were last checked against the providers' pages,
    /// at 00:00 UTC, in seconds since the Unix epoch.
    pub(crate) checked: u64,
    models: Vec<Model>,
}

#[derive(Debug)]
struct Model {
    /// As [`table_id`] gives it.
    id: String,
    price: Price,
    /// Announced changes, earliest first: from the moment given, this price.
    changes: Vec<(u64, Price)>,
}

impl List {
    /// Read a list, every row checked. Any row Clew cannot trust makes the
    /// whole copy unread: a list half right is a list wrong.
    pub(crate) fn parse(bytes: &[u8]) -> Result<List, String> {
        if bytes.len() as u64 > MAX_BYTES {
            return Err(format!("the price list is over {} KiB", MAX_BYTES / 1024));
        }
        let doc: Doc = serde_json::from_slice(bytes)
            .map_err(|e| format!("the price list is not valid: {e}"))?;
        if doc.schema != SCHEMA {
            return Err(format!(
                "the price list is in format {}, and this Clew reads format {SCHEMA}",
                doc.schema
            ));
        }
        let checked = day(&doc.checked)
            .ok_or_else(|| format!("the price list's date {:?} is not a date", doc.checked))?;
        if doc.models.is_empty() || doc.models.len() > MAX_MODELS {
            return Err(format!(
                "the price list has {} models, not between 1 and {MAX_MODELS}",
                doc.models.len()
            ));
        }
        let mut models: Vec<Model> = Vec::with_capacity(doc.models.len());
        for row in doc.models {
            if !id_ok(&row.id) {
                return Err(format!("the price list names a model {:?}", row.id));
            }
            let id = table_id(&row.id);
            if models.iter().any(|m| m.id == id) {
                return Err(format!("the price list names {} twice", row.id));
            }
            let price = rates(&row.id, row.input, row.cached, row.output)?;
            let mut changes = Vec::with_capacity(row.changes.len());
            for change in row.changes {
                let from = day(&change.from).ok_or_else(|| {
                    format!(
                        "{}'s price change date {:?} is not a date",
                        row.id, change.from
                    )
                })?;
                if changes.last().is_some_and(|(before, _)| *before >= from) {
                    return Err(format!("{}'s price changes are not in date order", row.id));
                }
                changes.push((
                    from,
                    rates(&row.id, change.input, change.cached, change.output)?,
                ));
            }
            models.push(Model { id, price, changes });
        }
        Ok(List { checked, models })
    }

    /// `model`'s price at the moment `at` (seconds since the Unix epoch):
    /// its row's, or that of the last change to it begun by then. A dated
    /// snapshot of a model (`claude-sonnet-4-20250514`, `gpt-4o-2024-08-06`,
    /// `-latest`) is priced as the model; any other id the list does not
    /// name is unpriced. Matched by its start, `gpt-5.5` was read as `gpt-5`,
    /// at a quarter of its input rate: a release the list does not know is
    /// better unpriced, where the reader is asked for its price, than priced
    /// as an older one.
    pub(crate) fn price_at(&self, model: &str, at: u64) -> Option<Price> {
        let id = table_id(model);
        let model = self.models.iter().find(|m| names(&id, &m.id))?;
        Some(
            model
                .changes
                .iter()
                .rev()
                .find(|(from, _)| *from <= at)
                .map_or(model.price, |(_, later)| *later),
        )
    }
}

/// A row's rates, if each is a price: not negative, not past
/// [`MAX_RATE`], and a cached rate no higher than the input rate.
fn rates(id: &str, input: f64, cached: f64, output: f64) -> Result<Price, String> {
    let fine = |r: f64| r.is_finite() && (0.0..=MAX_RATE).contains(&r);
    if !(fine(input) && fine(cached) && fine(output)) || cached > input {
        return Err(format!("{id}'s price in the price list is not a price"));
    }
    Ok(Price {
        input,
        cached,
        output,
    })
}

/// Whether `id` reads as a model id: letters, digits, `.`, `-`, `_`, `/`,
/// `:` and `@`, at most 128 of them.
fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/:@".contains(&b))
}

/// `model` as the list names it, in lower case: without the maker
/// OpenRouter puts first or the variant it puts last, its version's dots as
/// dashes (`anthropic/claude-opus-4.5:beta` is `claude-opus-4-5`).
fn table_id(model: &str) -> String {
    let model = model.trim().to_ascii_lowercase();
    let model = model.rsplit('/').next().unwrap_or(&model);
    model.split(':').next().unwrap_or(model).replace('.', "-")
}

/// Whether `id` names the model `name` (both as [`table_id`] gives them):
/// the model itself, or a dated snapshot of it (`-20250514`,
/// `-2024-08-06`, `-latest`). `gpt-5-5` is not `gpt-5`, nor `claude-opus-4-1`
/// `claude-opus-4`.
fn names(id: &str, name: &str) -> bool {
    let Some(rest) = id.strip_prefix(name) else {
        return false;
    };
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    match rest.strip_prefix('-') {
        None => rest.is_empty(),
        Some("latest") => true,
        Some(date) => {
            digits(date, 8)
                || matches!(
                    date.split('-').collect::<Vec<_>>()[..],
                    [y, m, d] if digits(y, 4) && digits(m, 2) && digits(d, 2)
                )
        }
    }
}

/// A `YYYY-MM-DD` day's start, in seconds since the Unix epoch (UTC).
fn day(text: &str) -> Option<u64> {
    let mut parts = text.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let days_in = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => return None,
    };
    ((2000..=2200).contains(&y) && (1..=days_in).contains(&d)).then(|| utc_day(y, m, d))
}

/// The start of a day (UTC), in seconds since the Unix epoch (Howard
/// Hinnant's `days_from_civil`).
pub(crate) fn utc_day(year: i64, month: i64, day: i64) -> u64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    ((era * 146_097 + doe - 719_468) * 86_400) as u64
}

/// The list Clew was built with, read once.
fn built_in() -> Arc<List> {
    static LIST: OnceLock<Arc<List>> = OnceLock::new();
    LIST.get_or_init(|| {
        Arc::new(List::parse(BUILT_IN.as_bytes()).expect("the built-in price list is valid"))
    })
    .clone()
}

/// A copy newer than the built-in list, once one is taken.
type Newer = RwLock<Option<Arc<List>>>;
static NEWER: Newer = RwLock::new(None);

/// The list in use: the newest Clew has, the built-in one at least.
pub(crate) fn list() -> Arc<List> {
    in_use(&NEWER)
}

fn in_use(newer: &Newer) -> Arc<List> {
    newer
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(built_in)
}

/// Use `copy` from now on, if it was checked more recently than the list
/// in use. Whether it was.
fn adopt(newer: &Newer, copy: List) -> bool {
    let mut newer = newer.write().unwrap_or_else(|e| e.into_inner());
    let current = newer.clone().unwrap_or_else(built_in);
    if copy.checked <= current.checked {
        return false;
    }
    *newer = Some(Arc::new(copy));
    true
}

fn kept_path() -> Option<std::path::PathBuf> {
    Some(clew_core::lsp::store::data_root()?.join(KEPT))
}

/// Take the copy kept by an earlier fetch, when it is newer than the
/// built-in list. Read once, as Clew starts; a copy that does not read is
/// left for the next fetch to replace.
pub(crate) fn load_kept() {
    let Some(path) = kept_path() else {
        return;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    if let Ok(copy) = List::parse(&bytes) {
        adopt(&NEWER, copy);
    }
}

/// Whether the repository's copy is due a fetch: none kept, or the kept
/// one fetched a day ago or more. Its file's time is when it was fetched.
pub(crate) fn refresh_due() -> bool {
    let fetched = kept_path()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());
    fetched
        .and_then(|t| t.elapsed().ok())
        .is_none_or(|age| age >= REFRESH_EVERY)
}

/// Fetch the repository's copy, keep it, and use it when it is newer than
/// the list in use. Blocking; run off the UI thread. Whether the prices in
/// use changed.
pub(crate) fn fetch() -> Result<bool, String> {
    let url = format!(
        "{}/contents/{PATH_IN_REPO}?ref=main",
        clew_core::update::api_base()
    );
    let body = clew_core::net::get(
        &url,
        &[("Accept", "application/vnd.github.raw+json")],
        clew_core::net::Limits {
            max_bytes: MAX_BYTES,
            deadline: Duration::from_secs(30),
        },
    )?;
    let copy = List::parse(&body)?;
    // Kept even when not newer: the file's time says the day's fetch was
    // made.
    if let Some(path) = kept_path() {
        keep(&path, &body)?;
    }
    Ok(adopt(&NEWER, copy))
}

/// Write `body` to `path` whole or not at all.
fn keep(path: &std::path::Path, body: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("the data directory has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let part = path.with_extension("json.part");
    std::fs::write(&part, body).map_err(|e| e.to_string())?;
    std::fs::rename(&part, path).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        e.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(json: &str) -> Result<List, String> {
        List::parse(json.as_bytes())
    }

    /// The list Clew ships reads, and prices what it lists.
    #[test]
    fn the_built_in_list_reads() {
        let list = built_in();
        assert!(list.models.len() > 50);
        assert_eq!(list.checked, utc_day(2026, 11, 21));
        let gpt = list.price_at("gpt-5.5", list.checked).unwrap();
        assert_eq!((gpt.input, gpt.cached, gpt.output), (5.0, 0.5, 30.0));
    }

    /// A copy is read only when every row of it is a price Clew can trust.
    #[test]
    fn a_copy_that_is_not_right_is_not_read() {
        let row = r#"{"id": "m", "input": 1, "cached": 0.1, "output": 2}"#;
        let with = |models: &str| {
            format!(r#"{{"schema": 1, "checked": "2026-11-21", "models": [{models}]}}"#)
        };
        assert!(list(&with(row)).is_ok());
        for bad in [
            // Not the format this Clew reads.
            r#"{"schema": 2, "checked": "2026-11-21", "models": [{"id": "m", "input": 1, "cached": 0.1, "output": 2}]}"#.to_string(),
            // Not a date.
            r#"{"schema": 1, "checked": "2026-02-30", "models": [{"id": "m", "input": 1, "cached": 0.1, "output": 2}]}"#.to_string(),
            // No models.
            with(""),
            // A negative rate, a rate past belief, a cache dearer than input.
            with(r#"{"id": "m", "input": -1, "cached": 0, "output": 2}"#),
            with(r#"{"id": "m", "input": 1, "cached": 0.1, "output": 20000}"#),
            with(r#"{"id": "m", "input": 1, "cached": 2, "output": 2}"#),
            // A name that is not a model id, and one named twice.
            with(r#"{"id": "m <script>", "input": 1, "cached": 0.1, "output": 2}"#),
            with(&format!("{row}, {row}")),
            // Changes out of order.
            with(r#"{"id": "m", "input": 1, "cached": 0.1, "output": 2, "changes": [
                {"from": "2027-01-01", "input": 2, "cached": 0.2, "output": 4},
                {"from": "2026-12-01", "input": 3, "cached": 0.3, "output": 6}]}"#),
            // Not JSON at all.
            "<html>rate limited</html>".to_string(),
        ] {
            assert!(list(&bad).is_err(), "{bad}");
        }
        let huge = with(row) + &" ".repeat(MAX_BYTES as usize);
        assert!(list(&huge).is_err());
    }

    /// A model matches its own id or a dated snapshot of it, and nothing
    /// that only starts like it.
    #[test]
    fn a_model_is_its_id_or_a_snapshot_of_it() {
        assert!(names("gpt-5", "gpt-5"));
        assert!(names("gpt-5-2025-08-07", "gpt-5"));
        assert!(names("claude-sonnet-4-20250514", "claude-sonnet-4"));
        assert!(names("claude-3-5-haiku-latest", "claude-3-5-haiku"));
        assert!(!names("gpt-5-5", "gpt-5"));
        assert!(!names("gpt-5-mini", "gpt-5"));
        assert!(!names("claude-opus-4-1-20250805", "claude-opus-4"));
        assert!(!names("gpt-50", "gpt-5"));
        assert_eq!(
            table_id("anthropic/claude-opus-4.5:beta"),
            "claude-opus-4-5"
        );
        assert_eq!(table_id(" Models/Gemini-2.5-Pro "), "gemini-2-5-pro");
    }

    /// A newer copy is used, an older one is not, and a change starts on
    /// its day.
    #[test]
    fn only_a_newer_copy_is_used() {
        let at = |date: &str, input: u32| {
            list(&format!(
                r#"{{"schema": 1, "checked": "{date}", "models": [
                    {{"id": "only-in-copy", "input": {input}, "cached": 0, "output": 1,
                      "changes": [{{"from": "2031-01-01", "input": 9, "cached": 0, "output": 9}}]}}]}}"#
            ))
            .unwrap()
        };
        // Its own slot: the process's is the one every other test prices by.
        let newer: Newer = RwLock::new(None);
        let has = |model: &str| in_use(&newer).price_at(model, 0).map(|p| p.input);
        assert!(
            !adopt(&newer, at("2020-01-01", 1)),
            "older than the built-in list"
        );
        assert!(has("only-in-copy").is_none());
        assert!(adopt(&newer, at("2030-01-01", 2)));
        assert_eq!(has("only-in-copy"), Some(2.0));
        assert!(
            !adopt(&newer, at("2029-01-01", 3)),
            "older than the copy in use"
        );
        assert_eq!(has("only-in-copy"), Some(2.0));
        let copy = in_use(&newer);
        assert_eq!(
            copy.price_at("only-in-copy", utc_day(2031, 1, 1) - 1)
                .unwrap()
                .input,
            2.0
        );
        assert_eq!(
            copy.price_at("only-in-copy", utc_day(2031, 1, 1))
                .unwrap()
                .input,
            9.0
        );
    }

    /// The repository's copy is fetched from the update check's host, kept
    /// in the data directory, and used when it is newer; a day passes before
    /// the next fetch.
    #[test]
    fn a_fetched_copy_is_kept_and_used() {
        use crate::app::tests::{EnvVars, test_dir};
        use std::io::Write;
        let root = test_dir("prices-fetch");
        // The built-in list, checked later: the same prices, so the other
        // tests price the same while it is in use.
        let newer = BUILT_IN.replacen(
            "\"checked\": \"2026-11-21\"",
            "\"checked\": \"2099-01-01\"",
            1,
        );
        assert_ne!(newer, BUILT_IN);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (asked_tx, asked) = std::sync::mpsc::channel();
        let body = newer.clone();
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let request = clew_core::testutil::read_http(&mut conn);
                let _ = asked_tx.send(request.head);
                let _ = write!(
                    conn,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let _env = EnvVars::new()
            .set("CLEW_DATA_DIR", &root)
            .set("CLEW_UPDATE_API", format!("http://{addr}/repos/o/r"));
        assert!(refresh_due(), "nothing kept yet");
        let fetched = fetch();
        let in_use = super::list().checked;
        // Back to the built-in list before anything can fail.
        *NEWER.write().unwrap() = None;
        assert_eq!(fetched, Ok(true));
        assert_eq!(in_use, utc_day(2099, 1, 1));
        let head = asked
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert!(
            head.starts_with("GET /repos/o/r/contents/assets/prices.json?ref=main "),
            "{head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("accept: application/vnd.github.raw+json"),
            "{head}"
        );
        assert_eq!(std::fs::read_to_string(root.join(KEPT)).unwrap(), newer);
        assert!(!refresh_due(), "fetched today");
    }

    #[test]
    fn days_read_as_dates() {
        assert_eq!(day("2026-12-01"), Some(1_796_083_200));
        assert_eq!(day("2028-02-29"), Some(utc_day(2028, 2, 29)));
        for bad in [
            "2027-02-29",
            "2026-13-01",
            "2026-1-01",
            "26-01-01",
            "2026-01-01-01",
            "",
        ] {
            assert_eq!(day(bad), None, "{bad}");
        }
    }
}
