//! What the AI key spends: a ledger of every model call Clew makes for the
//! reader, the prices that read it in money, estimates before a spend, and
//! the provider's balance where it tells one.
//!
//! Each model call reports what it used (`llm::metered`): on clew-server as
//! an `Event::Usage`, on this machine straight to the ledger. The ledger is
//! one JSON line a call in the data directory (`usage.jsonl`), the reader's
//! own: it is never sent anywhere. Money is worked out as it is shown, from
//! the price table ([`price_of`]) — Clew's own for the models it knows, the
//! reader's where they set one — so a price corrected later corrects the
//! history too.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::app::prelude::*;
use crate::*;

/// The ledger's file, in the data directory.
pub(crate) fn ledger_path() -> Option<PathBuf> {
    Some(clew_core::lsp::store::data_root()?.join("usage.jsonl"))
}

/// The most of the ledger read back: its last lines, a year's heavy use.
const LEDGER_READ_MAX: u64 = 8 << 20;

/// One model call, as the ledger keeps it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// When it ended, in seconds since the Unix epoch.
    pub at: u64,
    /// What it was for: one of this module's purposes ([`ASK`], [`EXPLAIN`],
    /// …), or another a newer Clew wrote.
    pub purpose: String,
    pub model: String,
    pub input: u64,
    #[serde(default)]
    pub cached: u64,
    /// Of `input`, those written to the prompt cache (a line kept before
    /// they were counted has none).
    #[serde(default)]
    pub written: u64,
    pub output: u64,
}

impl Entry {
    /// A call that ended now.
    pub(crate) fn now(purpose: &str, model: &str, usage: llm::Usage) -> Entry {
        Entry {
            at: now_secs(),
            purpose: purpose.to_string(),
            model: model.to_string(),
            input: usage.input,
            cached: usage.cached.min(usage.input),
            written: usage.written.min(usage.input),
            output: usage.output,
        }
    }

    /// Its cost in US dollars, at the price its model has now.
    pub(crate) fn cost(&self) -> Option<f64> {
        price_of(&self.model).map(|p| p.cost(self.input, self.cached, self.written, self.output))
    }
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What a call can be for, as the ledger names it, and as the reader reads
/// it.
pub(crate) const ASK: &str = "ask";
pub(crate) const EXPLAIN: &str = "explain";
pub(crate) const OVERVIEW: &str = "overview";
pub(crate) const WALKTHROUGH: &str = "walkthrough";
pub(crate) const TIME_TRAVEL: &str = "time_travel";
pub(crate) const FAKE_RUN: &str = "fake_run";
pub(crate) const SUGGEST: &str = "suggest";
pub(crate) const KEY_TEST: &str = "key_test";

/// The purpose `key` as the reader reads it.
pub(crate) fn purpose_label(key: &str) -> &'static str {
    match key {
        ASK => tr!("Ask"),
        EXPLAIN => tr!("Explanations"),
        OVERVIEW => tr!("Architecture overview"),
        WALKTHROUGH => tr!("Walkthroughs"),
        TIME_TRAVEL => tr!("Time Travel"),
        FAKE_RUN => tr!("Fake run"),
        SUGGEST => tr!("Suggested questions"),
        KEY_TEST => tr!("Key test"),
        _ => tr!("Other"),
    }
}

/// Append `entry` to the ledger. Blocking (a line's write).
pub(crate) fn append(entry: &Entry) -> std::io::Result<()> {
    let Some(path) = ledger_path() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    line.push('\n');
    // One write of the whole line, appended: two windows writing at once
    // interleave lines, never parts of them.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

/// The ledger, oldest first: its last [`LEDGER_READ_MAX`] bytes, a line that
/// does not read skipped. Blocking.
pub(crate) fn load() -> Vec<Entry> {
    let Some(path) = ledger_path() else {
        return Vec::new();
    };
    let Ok(text) = read_tail(&path, LEDGER_READ_MAX) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .collect()
}

/// The last `max` bytes of `path`, from the first whole line in them.
fn read_tail(path: &std::path::Path, max: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let from = len.saturating_sub(max);
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(if from > 0 {
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default()
    } else {
        text
    })
}

/// A model's price, in US dollars a million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Price {
    pub input: f64,
    /// A prompt token read from the provider's cache.
    pub cached: f64,
    pub output: f64,
}

/// What writing a token to the prompt cache costs, against reading it as
/// input: Anthropic's five-minute cache writes, the only ones a provider
/// bills apart (OpenAI's and DeepSeek's caches cost nothing to fill).
const CACHE_WRITE: f64 = 1.25;

impl Price {
    /// What `input` prompt tokens — `cached` of them read from the prompt
    /// cache, `written` to it — and `output` tokens cost, in US dollars.
    pub(crate) fn cost(&self, input: u64, cached: u64, written: u64, output: u64) -> f64 {
        let cached = cached.min(input);
        let written = written.min(input - cached);
        ((input - cached - written) as f64 * self.input
            + cached as f64 * self.cached
            + written as f64 * self.input * CACHE_WRITE
            + output as f64 * self.output)
            / 1e6
    }
}

/// When Clew's own prices were last looked up: they change, and the page
/// that shows them says so.
pub(crate) const PRICES_AS_OF: &str = "November 2026";

/// Clew's own prices, by the model id's start (a dated id is its model's):
/// the longer start first where one is another's.
const PRICES: &[(&str, Price)] = &[
    // Anthropic: its API reference.
    ("claude-fable-5-2", p(8.0, 0.20, 40.0)),
    ("claude-mythos-5-2", p(8.0, 0.20, 40.0)),
    ("claude-fable-5", p(10.0, 1.0, 50.0)),
    ("claude-mythos-5", p(10.0, 1.0, 50.0)),
    ("claude-opus-5-5", p(4.0, 0.20, 20.0)),
    ("claude-opus-5", p(5.0, 0.50, 25.0)),
    ("claude-opus-4-8", p(5.0, 0.50, 25.0)),
    ("claude-opus-4-7", p(5.0, 0.50, 25.0)),
    ("claude-opus-4-6", p(5.0, 0.50, 25.0)),
    ("claude-opus-4-5", p(5.0, 0.50, 25.0)),
    ("claude-opus-4", p(15.0, 1.50, 75.0)),
    ("claude-sonnet-5-5", p(2.0, 0.10, 10.0)),
    ("claude-sonnet-5", p(2.0, 0.20, 10.0)),
    ("claude-sonnet-4", p(3.0, 0.30, 15.0)),
    ("claude-haiku-5-5", p(0.10, 0.01, 0.50)),
    ("claude-haiku-4-5", p(1.0, 0.10, 5.0)),
    ("claude-3-5-haiku", p(0.80, 0.08, 4.0)),
    // OpenAI.
    ("gpt-4o-mini", p(0.15, 0.075, 0.60)),
    ("gpt-4o", p(2.50, 1.25, 10.0)),
    ("gpt-4.1-nano", p(0.10, 0.025, 0.40)),
    ("gpt-4.1-mini", p(0.40, 0.10, 1.60)),
    ("gpt-4.1", p(2.0, 0.50, 8.0)),
    ("gpt-5-nano", p(0.05, 0.005, 0.40)),
    ("gpt-5-mini", p(0.25, 0.025, 2.0)),
    ("gpt-5", p(1.25, 0.125, 10.0)),
    // DeepSeek, at its daytime (peak) rates.
    ("deepseek-v4-pro", p(1.32, 0.044, 3.96)),
    ("deepseek-v4-flash", p(0.30, 0.006, 1.20)),
    ("deepseek-flash", p(0.30, 0.006, 1.20)),
    ("deepseek-chat", p(0.28, 0.028, 0.42)),
    ("deepseek-reasoner", p(0.28, 0.028, 0.42)),
];

const fn p(input: f64, cached: f64, output: f64) -> Price {
    Price {
        input,
        cached,
        output,
    }
}

/// Clew's own price for `model`, if it knows one.
pub(crate) fn builtin_price(model: &str) -> Option<Price> {
    let model = model.trim().to_ascii_lowercase();
    // OpenRouter's ids name their maker first, write a version with dots,
    // and may carry a variant: `anthropic/claude-opus-4.5:beta`. Dotted, it
    // was priced as `claude-opus-4`, three times over, and `claude-haiku-4.5`
    // not at all.
    let model = model.rsplit('/').next().unwrap_or(&model);
    let model = model.split(':').next().unwrap_or(model).replace('.', "-");
    PRICES
        .iter()
        .find(|(start, _)| model.starts_with(&start.replace('.', "-")))
        .map(|(_, price)| *price)
}

/// The reader's own prices, `config.toml`'s `[prices]`, as last read: read
/// once, and again after [`save_own_price`] — never by a frame drawing.
static OWN_PRICES: std::sync::Mutex<Option<toml::Table>> = std::sync::Mutex::new(None);

/// The reader's own price for `model`, as Settings keeps it in
/// `config.toml`'s `[prices."<model>"]`.
pub(crate) fn own_price(model: &str) -> Option<Price> {
    let mut cached = OWN_PRICES.lock().unwrap_or_else(|e| e.into_inner());
    let table = cached
        .get_or_insert_with(|| clew_core::globalconfig::section("prices").unwrap_or_default());
    let entry = table.get(model.trim())?.as_table()?;
    let n = |k: &str| {
        entry
            .get(k)
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    let input = n("input")?;
    Some(Price {
        input,
        cached: n("cached").unwrap_or(input),
        output: n("output")?,
    })
}

/// The price `model`'s calls are read at: the reader's, else Clew's.
pub(crate) fn price_of(model: &str) -> Option<Price> {
    own_price(model).or_else(|| builtin_price(model))
}

/// Keep `price` as the reader's for `model` (`None`: back to Clew's).
pub(crate) fn save_own_price(model: &str, price: Option<Price>) -> Result<(), String> {
    let turn = PRICE_SAVES.fetch_add(1, Ordering::SeqCst) + 1;
    save_own_price_if_latest(model, price, turn)
}

/// The price the form's three fields say, once they read as one: Input and
/// Output prices, and a Cached one — the Input's where it is left blank, as
/// a price kept without one reads. Input and Output typed alone were never
/// saved, and nothing said so.
pub(crate) fn draft_price(draft: &[String; 3]) -> Option<Price> {
    let field = |f: PriceField| {
        let v = draft[f as usize].trim();
        (!v.is_empty()).then(|| v.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0))
    };
    let input = field(PriceField::Input)??;
    let output = field(PriceField::Output)??;
    let cached = match field(PriceField::Cached) {
        None => input,
        Some(cached) => cached?,
    };
    Some(Price {
        input,
        cached,
        output,
    })
}

/// What this process's own model calls spent, for its window to count
/// ([`App::take_local_spent`]).
static LOCAL_SPENT: std::sync::Mutex<Vec<llm::Spent>> = std::sync::Mutex::new(Vec::new());

/// The latest price save asked for: one asked before it, still writing,
/// writes nothing — the saves run off the window's thread, and an older
/// keystroke's would otherwise land last.
static PRICE_SAVES: AtomicU64 = AtomicU64::new(0);

/// Forget the reader's own prices as last read: read again on the next look
/// (the config was changed, maybe in another window).
pub(crate) fn forget_own_prices() {
    *OWN_PRICES.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// [`save_own_price`], as save number `turn`: kept only while no later one
/// was asked for.
fn save_own_price_if_latest(model: &str, price: Option<Price>, turn: u64) -> Result<(), String> {
    let model = model.trim().to_string();
    let written = clew_core::globalconfig::edit(|config| {
        if PRICE_SAVES.load(Ordering::SeqCst) != turn {
            return;
        }
        let prices = config
            .entry("prices")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(prices) = prices.as_table_mut() else {
            return;
        };
        match price {
            Some(price) => {
                let mut entry = toml::Table::new();
                entry.insert("input".into(), price.input.into());
                entry.insert("cached".into(), price.cached.into());
                entry.insert("output".into(), price.output.into());
                prices.insert(model, toml::Value::Table(entry));
            }
            None => {
                prices.remove(&model);
            }
        }
    });
    // Read again on the next look, whatever the write did.
    forget_own_prices();
    written
}

/// Calls and tokens added up, with what they cost where every one of them
/// has a price.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Totals {
    pub calls: u64,
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    /// What the priced calls cost, in US dollars.
    pub cost: f64,
    /// Calls whose model has no price: their cost is not in `cost`.
    pub unpriced: u64,
}

impl Totals {
    /// Count `e`, at `price` (its model's).
    pub(crate) fn add(&mut self, e: &Entry, price: Option<Price>) {
        self.calls += 1;
        self.input += e.input;
        self.cached += e.cached;
        self.output += e.output;
        match price {
            Some(p) => self.cost += p.cost(e.input, e.cached, e.written, e.output),
            None => self.unpriced += 1,
        }
    }

    pub(crate) fn tokens(&self) -> u64 {
        self.input + self.output
    }
}

/// The ledger read for its page: today, this month, the days before it,
/// what each feature spent, and the latest calls.
#[derive(Debug, Clone, Default)]
pub(crate) struct Summary {
    pub today: Totals,
    pub month: Totals,
    /// The last [`DAYS`] days, oldest first, today last.
    pub days: Vec<Totals>,
    /// This month, by what the calls were for, the most spent first.
    pub by_purpose: Vec<(String, Totals)>,
    /// The latest calls, newest first.
    pub recent: Vec<Entry>,
}

/// How many days the page's chart shows.
pub(crate) const DAYS: usize = 30;
/// How many of the latest calls the page lists.
pub(crate) const RECENT: usize = 8;

/// `entries` summed up as of `now` (days and months in UTC).
pub(crate) fn summarize(entries: &[Entry], now: u64) -> Summary {
    summarize_in(entries, now, |t| local_secs(t as i64).max(0) as u64)
}

/// `t` (seconds since the epoch) as the reader's clock reads it: moved by
/// their time zone's offset at that moment. Days were cut at UTC midnight,
/// "Today" running 09:00 to 09:00 in Tokyo.
pub(crate) fn local_secs(t: i64) -> i64 {
    use chrono::TimeZone;
    let offset = chrono::Local
        .timestamp_opt(t, 0)
        .single()
        .map_or(0, |d| d.offset().local_minus_utc());
    t + i64::from(offset)
}

/// [`summarize`], its days and months those of `local` (a moment as the
/// reader's clock reads it).
fn summarize_in(entries: &[Entry], now: u64, local: impl Fn(u64) -> u64) -> Summary {
    let day = |t: u64| local(t) / 86_400;
    let today = day(now);
    let month_of = |t: u64| {
        let (y, m, _) = civil(day(t) as i64);
        (y, m)
    };
    let this_month = month_of(now);
    let mut s = Summary {
        days: vec![Totals::default(); DAYS],
        ..Summary::default()
    };
    let mut by: HashMap<&str, Totals> = HashMap::new();
    // Each model's price looked up once.
    let mut prices: HashMap<&str, Option<Price>> = HashMap::new();
    for e in entries {
        let price = *prices
            .entry(e.model.as_str())
            .or_insert_with(|| price_of(&e.model));
        let d = day(e.at);
        if d == today {
            s.today.add(e, price);
        }
        if month_of(e.at) == this_month && d <= today {
            s.month.add(e, price);
            by.entry(e.purpose.as_str()).or_default().add(e, price);
        }
        if d <= today && today - d < DAYS as u64 {
            s.days[DAYS - 1 - (today - d) as usize].add(e, price);
        }
    }
    s.by_purpose = by.into_iter().map(|(k, t)| (k.to_string(), t)).collect();
    s.by_purpose.sort_by(|a, b| {
        b.1.cost
            .total_cmp(&a.1.cost)
            .then(b.1.tokens().cmp(&a.1.tokens()))
            .then(a.0.cmp(&b.0))
    });
    s.recent = entries.iter().rev().take(RECENT).cloned().collect();
    s
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// What `calls` more calls for `purpose` to `model` are likely to use: the
/// tokens a call for it has used lately (the last [`ESTIMATE_FROM`] in the
/// ledger, of any model), else a typical call's, and what that costs at
/// `model`'s price.
pub(crate) fn estimate(entries: &[Entry], purpose: &str, calls: u64, model: &str) -> Estimate {
    let lately: Vec<&Entry> = entries
        .iter()
        .rev()
        .filter(|e| e.purpose == purpose)
        .take(ESTIMATE_FROM)
        .collect();
    let from_history = lately.len() >= ESTIMATE_MIN;
    let (input, cached, written, output) = if from_history {
        let n = lately.len() as u64;
        let sum = |f: fn(&Entry) -> u64| lately.iter().map(|e| f(e)).sum::<u64>() / n;
        (
            sum(|e| e.input),
            sum(|e| e.cached),
            sum(|e| e.written),
            sum(|e| e.output),
        )
    } else {
        let (input, cached, output) = typical_call(purpose);
        (input, cached, 0, output)
    };
    Estimate {
        tokens: calls * (input + output),
        cost: price_of(model).map(|p| {
            p.cost(
                calls * input,
                calls * cached,
                calls * written,
                calls * output,
            )
        }),
        from_history,
    }
}

/// How many of the latest calls an estimate is drawn from, and the fewest
/// it trusts over a typical call.
const ESTIMATE_FROM: usize = 200;
const ESTIMATE_MIN: usize = 5;

/// A typical call's `(input, cached, output)` tokens, for a purpose the
/// ledger has too few of yet.
fn typical_call(purpose: &str) -> (u64, u64, u64) {
    match purpose {
        EXPLAIN => (1_400, 0, 300),
        WALKTHROUGH => (6_000, 0, 1_500),
        ASK => (12_000, 6_000, 700),
        _ => (2_000, 0, 500),
    }
}

/// What some calls are likely to use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Estimate {
    pub tokens: u64,
    /// In US dollars, when the model has a price.
    pub cost: Option<f64>,
    /// Drawn from the reader's own recent calls for it, not a typical one.
    pub from_history: bool,
}

impl Estimate {
    /// `calls` of them, in one sentence of one weight: "About 44 AI calls:
    /// 75k tokens, $0.03." The count stood bold over a dim line of the
    /// tokens and money — the figure that matters most said least.
    pub(crate) fn said(&self, calls: usize) -> String {
        let count = crate::app::setup::about(calls);
        let tokens = tokens(self.tokens);
        match self.cost {
            Some(cost) => trn!(
                "About {count} AI call: {tokens} tokens, {cost}.",
                "About {count} AI calls: {tokens} tokens, {cost}.",
                calls,
                count = count,
                tokens = tokens,
                cost = money(cost)
            ),
            None => trn!(
                "About {count} AI call: {tokens} tokens.",
                "About {count} AI calls: {tokens} tokens.",
                calls,
                count = count,
                tokens = tokens
            ),
        }
    }

    /// Where the figures come from, and what they are worth.
    pub(crate) fn basis(&self) -> String {
        if self.from_history {
            tr!("Estimated from your recent explanations.").to_string()
        } else {
            tr!("Estimated from a typical explanation.").to_string()
        }
    }
}

/// A count of tokens as a reader reads it: 950, 12k, 1.4M.
pub(crate) fn tokens(n: u64) -> String {
    if n < 1_000 {
        crate::i18n::number(n)
    } else if n < 999_500 {
        // Below what rounds to a thousand thousand: 999,500 read "1000k".
        let k = n as f64 / 1_000.0;
        if k < 10.0 {
            format!("{:.1}k", k).replace(".0k", "k")
        } else {
            format!("{}k", k.round() as u64)
        }
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0).replace(".0M", "M")
    }
}

/// Dollars as a reader reads them: a cent and up to the cent, less in the
/// figures that tell it ($0.0031), nothing as $0.
pub(crate) fn money(dollars: f64) -> String {
    if dollars <= 0.0 {
        "$0".into()
    } else if dollars >= 0.01 {
        format!("${dollars:.2}")
    } else if dollars >= 0.0001 {
        format!("${dollars:.4}")
    } else {
        "<$0.0001".into()
    }
}

/// Where a provider's own billing page is, for one that tells an API key
/// nothing of its balance.
pub(crate) fn billing_page(provider: llm::Provider) -> Option<&'static str> {
    match provider {
        llm::Provider::Anthropic => Some("https://console.anthropic.com/settings/billing"),
        llm::Provider::OpenAI => {
            Some("https://platform.openai.com/settings/organization/billing/overview")
        }
        llm::Provider::DeepSeek => Some("https://platform.deepseek.com/usage"),
        llm::Provider::Custom => None,
    }
}

/// What the page knows of the account's balance.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Balance {
    /// Not read yet (or the provider changed since).
    #[default]
    Unread,
    Reading,
    /// What is left, an amount a currency, and when it was read.
    Read(Vec<(String, String)>, u64),
    /// The provider tells an ordinary API key nothing of it.
    NotTold,
    Failed(String),
}

/// What Settings → Usage holds: the ledger as last read, the balance, and
/// the prices being typed.
#[derive(Debug, Default)]
pub struct UsageState {
    /// The model calls are made to, and its provider: as last read from
    /// the config (at start, as the page opens, as Settings saves) — a
    /// frame drawing reads no file.
    pub model: String,
    pub provider: Option<llm::Provider>,
    /// The provider tells the key its balance ([`llm::balance_url`]).
    pub tells_balance: bool,
    /// `None` until the ledger is first read.
    pub entries: Option<Vec<Entry>>,
    pub balance: Balance,
    /// The price fields as typed, for the model in use: `[input, cached,
    /// output]`, `None` until one is typed in.
    pub price_draft: Option<[String; 3]>,
    /// The cost of each Ask answer the server reported on, by its stream.
    pub by_stream: std::collections::HashMap<u64, (llm::Usage, String)>,
}

/// One price field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceField {
    Input,
    Cached,
    Output,
}

impl App {
    /// Read again which model calls go to: as the app starts, as the Usage
    /// page opens, as Settings saves.
    pub(crate) fn refresh_usage_model(&mut self) {
        let cfg = llm::Config::current_or_default();
        self.usage.tells_balance = llm::balance_url(&cfg).is_some();
        self.usage.provider = Some(cfg.provider);
        self.usage.model = cfg.model;
    }

    /// The model calls this window makes itself — when it has no
    /// clew-server, or the server may not hold the key — reported straight
    /// to the ledger.
    ///
    /// Kept in the file at once, and handed to the window too
    /// ([`Self::take_local_spent`]): it went to the file alone, and the
    /// Usage page, the estimates and a local Ask answer's cost knew nothing
    /// of it until the ledger was read again.
    pub(crate) fn local_meter() -> llm::Meter {
        std::sync::Arc::new(|spent: llm::Spent| {
            if spent.usage.is_empty() {
                return;
            }
            let _ = append(&Entry::now(&spent.purpose, &spent.model, spent.usage));
            LOCAL_SPENT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(spent);
        })
    }

    /// What this window's own calls spent since it last looked, counted as
    /// a server's are — already in the file.
    pub(crate) fn take_local_spent(&mut self) {
        let spent = std::mem::take(&mut *LOCAL_SPENT.lock().unwrap_or_else(|e| e.into_inner()));
        for s in spent {
            self.count_spent(s.work, &s.purpose, &s.model, s.usage);
        }
    }

    /// A model call the server made for this window used this much: kept in
    /// the ledger, shown on the Usage page if it is read, and counted to the
    /// Ask answer it was for.
    pub(crate) fn on_spent(
        &mut self,
        work: u64,
        purpose: String,
        model: String,
        usage: llm::Usage,
    ) -> Task<Message> {
        if usage.is_empty() {
            return Task::none();
        }
        let entry = self.count_spent(work, &purpose, &model, usage);
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || append(&entry))
                    .await
                    .ok()
                    .and_then(Result::err)
                    .map(|e| e.to_string())
            },
            |failed| Message::Usage(UsageMsg::Written(failed)),
        )
    }

    /// What a call spent, counted where this window shows it: the ledger
    /// as read, and the Ask answer it was for.
    fn count_spent(&mut self, work: u64, purpose: &str, model: &str, usage: llm::Usage) -> Entry {
        let entry = Entry::now(purpose, model, usage);
        if purpose == ASK {
            let (spent, at) = self
                .usage
                .by_stream
                .entry(work)
                .or_insert_with(|| (llm::Usage::default(), model.to_string()));
            *spent += usage;
            model.clone_into(at);
        }
        if let Some(entries) = self.usage.entries.as_mut() {
            entries.push(entry.clone());
        }
        entry
    }

    /// Read the ledger again, for the Usage page.
    pub(crate) fn read_ledger(&mut self) -> Task<Message> {
        Task::perform(
            async { tokio::task::spawn_blocking(load).await.unwrap_or_default() },
            |entries| Message::Usage(UsageMsg::Read(entries)),
        )
    }

    /// Ask the provider what is left on the account, if it tells.
    pub(crate) fn read_balance(&mut self) -> Task<Message> {
        let cfg = llm::Config::current_or_default();
        if llm::balance_url(&cfg).is_none() {
            self.usage.balance = Balance::NotTold;
            return Task::none();
        }
        if cfg.api_key.is_empty() {
            self.usage.balance = Balance::Unread;
            return Task::none();
        }
        self.usage.balance = Balance::Reading;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || llm::balance(&cfg).map_err(|e| e.to_string()))
                    .await
                    .unwrap_or_else(|_| Err(tr!("an internal task failed").into()))
            },
            |read| Message::Usage(UsageMsg::BalanceRead(read)),
        )
    }

    /// Handle a [`UsageMsg`].
    pub(crate) fn update_usage(&mut self, msg: UsageMsg) -> Task<Message> {
        match msg {
            UsageMsg::Read(entries) => {
                self.usage.entries = Some(entries);
                Task::none()
            }
            UsageMsg::Written(Some(e)) => {
                self.status = tr!("Could not keep what the AI call used: {error}", error = e);
                Task::none()
            }
            UsageMsg::Written(None) => Task::none(),
            UsageMsg::RefreshBalance => self.read_balance(),
            UsageMsg::BalanceRead(Ok(Some(amounts))) => {
                self.usage.balance = Balance::Read(amounts, now_secs());
                Task::none()
            }
            UsageMsg::BalanceRead(Ok(None)) => {
                self.usage.balance = Balance::NotTold;
                Task::none()
            }
            UsageMsg::BalanceRead(Err(e)) => {
                self.usage.balance = Balance::Failed(e);
                Task::none()
            }
            UsageMsg::OpenBilling => {
                if let Some(url) = self.usage.provider.and_then(billing_page)
                    && let Err(e) = crate::app::content::open_in_browser(url)
                {
                    self.status = e;
                }
                Task::none()
            }
            UsageMsg::Price(field, value) => {
                let model = self.usage.model.clone();
                let mut draft = self.usage.price_draft.clone().unwrap_or_else(|| {
                    price_of(&model).map_or_else(Default::default, |p| {
                        [p.input, p.cached, p.output].map(price_text)
                    })
                });
                draft[field as usize] = value;
                let typed = draft_price(&draft);
                self.usage.price_draft = Some(draft);
                if let Some(price) = typed {
                    // Kept as typed, once all three read as prices — off the
                    // window's thread: each keystroke wrote, and synced, the
                    // file holding the keys while the window waited.
                    if price_of(&model) != Some(price) {
                        let turn = PRICE_SAVES.fetch_add(1, Ordering::SeqCst) + 1;
                        return Task::perform(
                            async move {
                                tokio::task::spawn_blocking(move || {
                                    save_own_price_if_latest(&model, Some(price), turn)
                                })
                                .await
                                .unwrap_or_else(|e| Err(e.to_string()))
                            },
                            |r| Message::Usage(UsageMsg::PriceSaved(r)),
                        );
                    }
                }
                Task::none()
            }
            UsageMsg::PriceSaved(Err(e)) => {
                self.status = e;
                Task::none()
            }
            UsageMsg::PriceSaved(Ok(())) => Task::none(),
            UsageMsg::KeepFresh(on) => {
                self.keep_explanations_fresh = on;
                self.keep_view_pref(crate::app::runtime::KEEP_EXPLANATIONS_FRESH, on);
                // Turned on with code changed meanwhile: refreshed now.
                if on {
                    self.request_auto_refresh()
                } else {
                    Task::none()
                }
            }
            UsageMsg::ResetPrice => {
                let model = self.usage.model.clone();
                if let Err(e) = save_own_price(&model, None) {
                    self.status = e;
                }
                self.usage.price_draft = None;
                Task::none()
            }
        }
    }

    /// [`Self::usage_opened`], if Settings is on the Usage page.
    pub(crate) fn on_usage_page(&mut self) -> Task<Message> {
        if self.settings.page == crate::SettingsPage::Usage {
            self.usage_opened()
        } else {
            Task::none()
        }
    }

    /// What the Usage page asks for as it opens: the ledger as it is now,
    /// and the balance where the provider tells it.
    pub(crate) fn usage_opened(&mut self) -> Task<Message> {
        self.usage.price_draft = None;
        self.refresh_usage_model();
        let ledger = self.read_ledger();
        let balance = match self.usage.balance {
            Balance::Reading => Task::none(),
            _ => self.read_balance(),
        };
        Task::batch([ledger, balance])
    }

    /// What `calls` more Explain calls are likely to cost, said.
    pub(crate) fn explain_estimate(&self, calls: usize) -> Option<Estimate> {
        if calls == 0 || !self.llm_available {
            return None;
        }
        let entries = self.usage.entries.as_deref().unwrap_or(&[]);
        Some(estimate(entries, EXPLAIN, calls as u64, &self.usage.model))
    }

    /// What the Ask answer on `stream` used, and what it cost, said — once
    /// the server has told.
    pub(crate) fn answer_cost(&self, stream: u64) -> Option<String> {
        let (usage, model) = self.usage.by_stream.get(&stream)?;
        let total = usage.input + usage.output;
        Some(
            match price_of(model)
                .map(|p| p.cost(usage.input, usage.cached, usage.written, usage.output))
            {
                Some(cost) => tr!(
                    "{tokens} tokens, {cost}",
                    tokens = tokens(total),
                    cost = money(cost)
                ),
                None => tr!("{tokens} tokens", tokens = tokens(total)),
            },
        )
    }
}

/// A price as its field shows it: no trailing zeros.
pub(crate) fn price_text(n: f64) -> String {
    let s = format!("{n:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() { "0".into() } else { s.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at: u64, purpose: &str, model: &str, input: u64, output: u64) -> Entry {
        Entry {
            at,
            purpose: purpose.into(),
            model: model.into(),
            input,
            cached: 0,
            written: 0,
            output,
        }
    }

    #[test]
    fn a_model_is_priced_by_its_family() {
        let haiku = builtin_price("claude-haiku-4-5-20251001").unwrap();
        assert_eq!((haiku.input, haiku.output), (1.0, 5.0));
        // The newer Opus is not the old one's price.
        assert_eq!(builtin_price("claude-opus-4-8").unwrap().input, 5.0);
        assert_eq!(builtin_price("claude-opus-4-1").unwrap().input, 15.0);
        assert_eq!(builtin_price("gpt-4o-mini").unwrap().input, 0.15);
        assert_eq!(builtin_price("gpt-4o-2024-08-06").unwrap().input, 2.5);
        assert_eq!(
            builtin_price("anthropic/claude-sonnet-4-6").unwrap().input,
            3.0
        );
        assert!(builtin_price("deepseek-v4-flash").is_some());
        assert!(builtin_price("llama3.2").is_none());
        // OpenRouter's ids: the maker first, the version dotted, a variant.
        assert_eq!(
            builtin_price("anthropic/claude-opus-4.5").unwrap().input,
            5.0
        );
        assert_eq!(
            builtin_price("anthropic/claude-haiku-4.5").unwrap().input,
            1.0
        );
        assert_eq!(
            builtin_price("anthropic/claude-3.5-haiku:beta")
                .unwrap()
                .input,
            0.8
        );
        assert_eq!(builtin_price("openai/gpt-4.1-mini").unwrap().input, 0.4);
        assert_eq!(builtin_price("gpt-4.1").unwrap().input, 2.0);
        // Cached tokens bill at the cache's rate; never more than the prompt.
        let price = Price {
            input: 1.0,
            cached: 0.1,
            output: 5.0,
        };
        assert!((price.cost(1_000_000, 500_000, 0, 100_000) - 1.05).abs() < 1e-9);
        assert!((price.cost(10, 99, 0, 0) - 10.0 * 0.1 / 1e6).abs() < 1e-12);
    }

    /// Today, this month and the chart count what fell in them; the month's
    /// features lead with what spent most.
    #[test]
    fn the_ledger_is_summed_by_day_month_and_feature() {
        // 2026-11-11 12:00 UTC.
        let now = 1_794_398_400;
        let day = 86_400;
        let entries = vec![
            entry(now - 40 * day, EXPLAIN, "gpt-4o-mini", 1_000_000, 0),
            entry(now - 3 * day, EXPLAIN, "gpt-4o-mini", 1_000_000, 0),
            entry(now - 60, ASK, "gpt-4o-mini", 0, 1_000_000),
            entry(now - 30, ASK, "no-such-model", 10, 10),
        ];
        let s = summarize(&entries, now);
        assert_eq!(s.today.calls, 2);
        assert_eq!(s.today.unpriced, 1);
        assert!((s.today.cost - 0.60).abs() < 1e-9);
        assert_eq!(s.month.calls, 3, "the call 40 days ago is last month's");
        assert_eq!(s.days.len(), DAYS);
        assert_eq!(s.days[DAYS - 1].calls, 2);
        assert_eq!(s.days[DAYS - 4].calls, 1);
        assert_eq!(s.by_purpose[0].0, ASK, "Ask spent the most");
        assert_eq!(s.recent[0].model, "no-such-model", "newest first");
    }

    /// Days are the reader's: in Tokyo (UTC+9) at 08:20 on Nov 12, a call a
    /// minute before is today's, and one at 14:20 UTC on Nov 11 (23:20
    /// there) is yesterday's. Cut at UTC midnight, both were today's.
    #[test]
    fn days_are_the_readers_own() {
        let tokyo = |t: u64| t + 9 * 3_600;
        // 2026-11-11 23:20 UTC: 08:20 on Nov 12 in Tokyo.
        let now = 1_794_439_200;
        let entries = vec![
            entry(now - 9 * 3_600, EXPLAIN, "gpt-4o-mini", 1, 1),
            entry(now - 60, EXPLAIN, "gpt-4o-mini", 1, 1),
        ];
        let s = summarize_in(&entries, now, tokyo);
        assert_eq!(
            s.today.calls, 1,
            "08:00 Tokyo: only the call since midnight there"
        );
        assert_eq!(s.days[DAYS - 2].calls, 1, "the other was yesterday there");
        let utc = summarize_in(&entries, now, |t| t);
        assert_eq!(utc.today.calls, 2, "by UTC both were the 11th");
    }

    /// An estimate draws on what such calls have used lately, or on a
    /// typical one until there are enough; a model with no price is said in
    /// tokens alone.
    #[test]
    fn an_estimate_draws_on_recent_calls() {
        let typical = estimate(&[], EXPLAIN, 10, "gpt-4o-mini");
        assert_eq!(typical.tokens, 10 * (1_400 + 300));
        let lately: Vec<Entry> = (0..6)
            .map(|i| entry(i, EXPLAIN, "gpt-4o-mini", 1_000, 100))
            .collect();
        let drawn = estimate(&lately, EXPLAIN, 10, "gpt-4o-mini");
        assert_eq!(drawn.tokens, 11_000);
        assert!((drawn.cost.unwrap() - (10_000.0 * 0.15 + 1_000.0 * 0.6) / 1e6).abs() < 1e-12);
        assert!(drawn.from_history && !typical.from_history);
        assert_eq!(estimate(&lately, EXPLAIN, 1, "mystery").cost, None);
        assert_eq!(
            estimate(&lately, EXPLAIN, 1, "mystery").said(1),
            "About 1 AI call: 1.1k tokens."
        );
        assert_eq!(
            drawn.said(10),
            format!(
                "About 10 AI calls: 11k tokens, {}.",
                money(drawn.cost.unwrap())
            )
        );
    }

    /// Tokens written to Anthropic's prompt cache bill at 1.25 times its
    /// input price: priced as input, a Sonnet step writing 20,000 came out
    /// a fifth low.
    #[test]
    fn cache_writes_bill_above_input() {
        let sonnet = builtin_price("claude-sonnet-4-5").unwrap();
        let want = (500.0 * 3.0 + 20_000.0 * 3.75 + 300.0 * 15.0) / 1e6;
        assert!((sonnet.cost(20_500, 0, 20_000, 300) - want).abs() < 1e-12);
        let mut e = entry(0, ASK, "claude-sonnet-4-5", 20_500, 300);
        e.written = 20_000;
        assert!((e.cost().unwrap() - want).abs() < 1e-12);
        // A line kept before writes were counted reads as none.
        let old: Entry = serde_json::from_str(
            r#"{"at":1,"purpose":"ask","model":"m","input":5,"cached":0,"output":1}"#,
        )
        .unwrap();
        assert_eq!(old.written, 0);
    }

    /// The price form's fields read as a price once Input and Output do; a
    /// blank Cached is the Input's, and one that is no number is no price.
    #[test]
    fn the_price_form_reads_its_fields() {
        let draft = |a: &str, b: &str, c: &str| [a.to_string(), b.to_string(), c.to_string()];
        assert_eq!(
            draft_price(&draft("1", "", "2")),
            Some(Price {
                input: 1.0,
                cached: 1.0,
                output: 2.0
            })
        );
        assert_eq!(draft_price(&draft("1", "0.1", "2")).unwrap().cached, 0.1);
        assert_eq!(draft_price(&draft("1", "x", "2")), None);
        assert_eq!(draft_price(&draft("", "0.1", "2")), None);
        assert_eq!(draft_price(&draft("1", "0.1", "-2")), None);
    }

    #[test]
    fn tokens_and_money_read_as_a_reader_writes_them() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(1_000), "1k");
        assert_eq!(tokens(1_450), "1.4k");
        assert_eq!(tokens(72_400), "72k");
        assert_eq!(tokens(1_400_000), "1.4M");
        assert_eq!(tokens(999_499), "999k");
        assert_eq!(tokens(999_500), "1M");
        assert_eq!(money(0.0), "$0");
        assert_eq!(money(0.00312), "$0.0031");
        assert_eq!(money(0.042), "$0.04");
        assert_eq!(money(12.5), "$12.50");
        assert_eq!(money(0.00001), "<$0.0001");
        assert_eq!(price_text(0.15), "0.15");
        assert_eq!(price_text(2.0), "2");
    }

    /// A line torn by a crash, or a newer Clew's, is skipped; the rest read.
    #[test]
    fn the_ledger_reads_back_what_was_written() {
        let dir = clew_core::testutil::TempDir::new("usage-ledger");
        let path = dir.path().join("usage.jsonl");
        let a = entry(1, ASK, "m", 5, 6);
        let mut text = serde_json::to_string(&a).unwrap();
        text.push_str("\n{\"at\": 2, \"purp");
        text.push('\n');
        text.push_str(&serde_json::to_string(&entry(3, EXPLAIN, "m", 1, 1)).unwrap());
        text.push('\n');
        std::fs::write(&path, &text).unwrap();
        let read: Vec<Entry> = read_tail(&path, 1 << 20)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0], a);
        // Its tail alone, from a whole line.
        let tail = read_tail(&path, 40).unwrap();
        assert!(tail.lines().all(|l| l.starts_with('{')), "{tail:?}");
    }
}
