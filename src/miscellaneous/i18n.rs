//! The interface in the reader's language — English, French, Chinese,
//! Korean or Japanese.
//!
//! Text is written in English where it is used and looked up, by that
//! English, in a catalog per language (`locales/*.json`, built into the
//! program). Text a catalog does not have yet reads in English rather than
//! not at all.
//!
//! - [`tr!`](crate::tr) gives a piece of text in the current language; with
//!   arguments, `{name}` in it is replaced by each one's value.
//! - [`trn!`](crate::trn) gives one of two forms by a count — "1 file",
//!   "{n} files" — as the language counts: French says "0 fichier",
//!   Chinese, Korean and Japanese have one form for every count.
//!
//! The language is the process's (every window speaks it), chosen in
//! Settings or followed from the system; a test may speak another on its
//! own thread ([`with_lang`]) without the others hearing it.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// A language the interface speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    En,
    Fr,
    /// Simplified Chinese.
    Zh,
    Ko,
    Ja,
}

impl Lang {
    pub const ALL: [Lang; 5] = [Lang::En, Lang::Fr, Lang::Zh, Lang::Ko, Lang::Ja];

    /// Its code in the config file and the catalogs' names.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Fr => "fr",
            Lang::Zh => "zh",
            Lang::Ko => "ko",
            Lang::Ja => "ja",
        }
    }

    /// Its name in itself, as a picker of languages shows it — never
    /// translated, so a reader finds their own whatever the interface
    /// speaks.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Fr => "Français",
            Lang::Zh => "简体中文",
            Lang::Ko => "한국어",
            Lang::Ja => "日本語",
        }
    }

    /// The language's name in English, as a model is told to write in it.
    pub fn english_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Fr => "French",
            Lang::Zh => "Simplified Chinese",
            Lang::Ko => "Korean",
            Lang::Ja => "Japanese",
        }
    }

    /// The language a code or a system locale names ("fr", "fr-FR",
    /// "fr_FR.UTF-8", "zh-Hans-CN", "ja-JP"), if one it speaks.
    pub fn parse(code: &str) -> Option<Lang> {
        let primary = code
            .split(['-', '_', '.', '@'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match primary.as_str() {
            "en" => Some(Lang::En),
            "fr" => Some(Lang::Fr),
            "zh" => Some(Lang::Zh),
            "ko" => Some(Lang::Ko),
            "ja" => Some(Lang::Ja),
            _ => None,
        }
    }

    /// Whether it writes without spaces between words, and so counts and
    /// breaks lines by the character.
    pub fn is_cjk(self) -> bool {
        matches!(self, Lang::Zh | Lang::Ko | Lang::Ja)
    }

    fn from_u8(n: u8) -> Lang {
        match n {
            1 => Lang::Fr,
            2 => Lang::Zh,
            3 => Lang::Ko,
            4 => Lang::Ja,
            _ => Lang::En,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Lang::En => 0,
            Lang::Fr => 1,
            Lang::Zh => 2,
            Lang::Ko => 3,
            Lang::Ja => 4,
        }
    }
}

/// Which language to speak: the system's, or one chosen in Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LangPref {
    System,
    Chosen(Lang),
}

impl LangPref {
    /// Every choice, as Settings lists them.
    pub fn all() -> Vec<LangPref> {
        std::iter::once(LangPref::System)
            .chain(Lang::ALL.into_iter().map(LangPref::Chosen))
            .collect()
    }

    /// The language it speaks now.
    pub fn resolve(self) -> Lang {
        match self {
            LangPref::System => system(),
            LangPref::Chosen(lang) => lang,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            LangPref::System => "system",
            LangPref::Chosen(lang) => lang.code(),
        }
    }

    fn parse(s: &str) -> Option<LangPref> {
        match s {
            "system" => Some(LangPref::System),
            other => Lang::parse(other).map(LangPref::Chosen),
        }
    }
}

impl std::fmt::Display for LangPref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // "System" in the language spoken, and what it resolves to in
            // that language's own name.
            LangPref::System => write!(
                f,
                "{}",
                crate::tr!("System ({language})", language = system().native_name())
            ),
            LangPref::Chosen(lang) => f.write_str(lang.native_name()),
        }
    }
}

/// The language every window speaks.
static CURRENT: AtomicU8 = AtomicU8::new(0);
/// What it was chosen by: 0 the system, else the language chosen, plus one.
static PREF: AtomicU8 = AtomicU8::new(0);

/// The preference in effect, which every window shows in Settings.
pub fn pref() -> LangPref {
    match PREF.load(Ordering::Relaxed) {
        0 => LangPref::System,
        n => LangPref::Chosen(Lang::from_u8(n - 1)),
    }
}

fn set_pref(pref: LangPref) {
    PREF.store(
        match pref {
            LangPref::System => 0,
            LangPref::Chosen(lang) => lang.to_u8() + 1,
        },
        Ordering::Relaxed,
    );
    let lang = pref.resolve();
    set(lang);
    // Explanations and answers come back in the same language; English is
    // what a model writes unasked.
    clew_core::llm::set_reply_language((lang != Lang::En).then(|| lang.english_name()));
}

thread_local! {
    /// A language this thread speaks instead (a test's).
    static OVERRIDE: Cell<Option<Lang>> = const { Cell::new(None) };
}

/// The language spoken now.
pub fn current() -> Lang {
    OVERRIDE
        .with(Cell::get)
        .unwrap_or_else(|| Lang::from_u8(CURRENT.load(Ordering::Relaxed)))
}

/// Speak `lang` in every window.
pub fn set(lang: Lang) {
    CURRENT.store(lang.to_u8(), Ordering::Relaxed);
}

/// Run `f` speaking `lang` on this thread only: what a test draws or says
/// in another language, while the tests beside it speak English.
#[cfg(test)]
pub fn with_lang<R>(lang: Lang, f: impl FnOnce() -> R) -> R {
    let before = OVERRIDE.with(|o| o.replace(Some(lang)));
    let out = f();
    OVERRIDE.with(|o| o.set(before));
    out
}

/// The language the system asks for: the first of the user's preferred
/// languages this speaks, English if none.
pub fn system() -> Lang {
    static SYSTEM: OnceLock<Lang> = OnceLock::new();
    *SYSTEM.get_or_init(|| {
        sys_locale::get_locales()
            .find_map(|locale| Lang::parse(&locale))
            .unwrap_or(Lang::En)
    })
}

const CONFIG_KEY: &str = "language";

/// The saved preference, applied: called once, before the first frame.
pub fn init() -> LangPref {
    let pref = clew_core::globalconfig::get(CONFIG_KEY)
        .as_ref()
        .and_then(|v| v.as_str())
        .and_then(LangPref::parse)
        .unwrap_or(LangPref::System);
    set_pref(pref);
    pref
}

/// Speak what `pref` resolves to, and keep it for the next launch.
pub fn apply_and_save(pref: LangPref) -> Result<(), String> {
    set_pref(pref);
    clew_core::globalconfig::set_keys(vec![(
        CONFIG_KEY.into(),
        toml::Value::String(pref.as_str().into()),
    )])
}

/// The catalogs, as built into the program.
fn source(lang: Lang) -> Option<&'static str> {
    match lang {
        Lang::En => None,
        Lang::Fr => Some(include_str!("../../locales/fr.json")),
        Lang::Zh => Some(include_str!("../../locales/zh.json")),
        Lang::Ko => Some(include_str!("../../locales/ko.json")),
        Lang::Ja => Some(include_str!("../../locales/ja.json")),
    }
}

type Catalog = HashMap<String, &'static str>;

/// `lang`'s catalog: English text to its translation, read once.
fn catalog(lang: Lang) -> &'static Catalog {
    static CATALOGS: [OnceLock<Catalog>; 5] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    CATALOGS[usize::from(lang.to_u8())].get_or_init(|| parse_catalog(source(lang).unwrap_or("{}")))
}

/// A catalog's entries — a JSON object of English text to its translation
/// — the translations kept for the program's life (they are drawn every
/// frame). An entry left empty is not translated yet.
pub(crate) fn parse_catalog(text: &str) -> Catalog {
    let entries: HashMap<String, String> = match serde_json::from_str(text) {
        Ok(entries) => entries,
        Err(e) => {
            // Built in: a catalog that does not parse is caught by the
            // tests, and a build with one speaks English, not nothing.
            eprintln!("[clew] a language catalog could not be read: {e}");
            return Catalog::new();
        }
    };
    entries
        .into_iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(english, text)| (english, &*Box::leak(text.into_boxed_str())))
        .collect()
}

/// `english` in the language spoken now, or as it is if there is no
/// translation of it. Text written where it is used goes through
/// [`tr!`](crate::tr); this takes English decided at run time — a command's
/// title, kept in English as its name — whose every value is marked with
/// [`tr_noop!`](crate::tr_noop) where it is written.
pub fn lookup(english: &str) -> &str {
    match current() {
        Lang::En => english,
        lang => catalog(lang).get(english).copied().unwrap_or(english),
    }
}

/// `template` with each `{name}` replaced by its argument's value. `{{`
/// and `}}` are braces.
pub fn format(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(after) = tail.strip_prefix("{{") {
            out.push('{');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("}}") {
            out.push('}');
            rest = after;
        } else if let Some(end) = tail.strip_prefix('{').and_then(|t| t.find('}')) {
            let name = &tail[1..=end];
            match args.iter().find(|(k, _)| *k == name) {
                Some((_, value)) => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "{value}");
                }
                // An argument a translation names that the code does not
                // give: shown as written, never a panic.
                None => out.push_str(&tail[..end + 2]),
            }
            rest = &tail[end + 2..];
        } else {
            out.push_str(&tail[..1]);
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Whether `n` takes the singular form ("1 file") in the language spoken.
/// French counts 0 and 1 as one; English only 1. Chinese, Korean and
/// Japanese have one form, which their catalogs give for both.
pub fn is_one(n: i64) -> bool {
    match current() {
        Lang::Fr => n == 0 || n == 1,
        _ => n == 1,
    }
}

/// A whole number as the reader writes it, its thousands set apart:
/// "1,858"; in French "1 858", with a narrow no-break space.
pub fn number(n: u64) -> String {
    let mark = match current() {
        Lang::Fr => '\u{202f}',
        _ => ',',
    };
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(mark);
        }
        out.push(c);
    }
    out
}

/// `x` with `places` decimals, its decimal mark the reader's: "2.5"; in
/// French "2,5".
pub fn decimal(x: f64, places: usize) -> String {
    let text = format!("{x:.places$}");
    match current() {
        Lang::Fr => text.replace('.', ","),
        _ => text,
    }
}

/// How long ago `time` was, at `now` (unix seconds), as a reader says it:
/// "just now", "5 minutes ago", "3 days ago", "2 years ago".
pub fn ago(time: i64, now: i64) -> String {
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    let d = now - time;
    if d < MIN {
        return crate::tr!("just now").to_string();
    }
    if d < HOUR {
        crate::trn!("1 minute ago", "{n} minutes ago", d / MIN)
    } else if d < DAY {
        crate::trn!("1 hour ago", "{n} hours ago", d / HOUR)
    } else if d < 30 * DAY {
        crate::trn!("1 day ago", "{n} days ago", d / DAY)
    } else if d < 365 * DAY {
        crate::trn!("1 month ago", "{n} months ago", d / (30 * DAY))
    } else {
        crate::trn!("1 year ago", "{n} years ago", d / (365 * DAY))
    }
}

/// Text in the language spoken now: `tr!("Close")`; with arguments,
/// `tr!("{n} files", n = count)`, each `{name}` replaced by its value.
#[macro_export]
macro_rules! tr {
    ($text:literal) => {
        $crate::i18n::lookup($text)
    };
    ($text:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::i18n::format(
            $crate::i18n::lookup($text),
            &[$((stringify!($name), &$value as &dyn ::std::fmt::Display)),+],
        )
    };
}

/// English text kept in English — a name something is known by, saved or
/// compared — that is shown translated elsewhere, with
/// [`lookup`](crate::i18n::lookup). Marks it for the catalogs; gives it as
/// written.
#[macro_export]
macro_rules! tr_noop {
    ($text:literal) => {
        $text
    };
}

/// One of two forms by a count, as the language spoken counts:
/// `trn!("1 file", "{n} files", count)`. `{n}` is the count; more
/// arguments follow it.
#[macro_export]
macro_rules! trn {
    ($one:literal, $other:literal, $n:expr $(, $name:ident = $value:expr)* $(,)?) => {{
        let n = $n;
        let template = if $crate::i18n::is_one(n as i64) {
            $crate::i18n::lookup($one)
        } else {
            $crate::i18n::lookup($other)
        };
        $crate::i18n::format(
            template,
            &[("n", &n as &dyn ::std::fmt::Display) $(, (stringify!($name), &$value as &dyn ::std::fmt::Display))*],
        )
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// Whether a key is one form of a count (`trn!`), whose `{n}` a
    /// translation may use or leave out.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Kind {
        Text,
        Count,
    }

    /// A Rust string literal at the start of `s` (after its opening quote),
    /// unescaped, and what follows it.
    fn literal(s: &str) -> Option<(String, &str)> {
        let mut out = String::new();
        let mut chars = s.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => return Some((out, &s[i + 1..])),
                '\\' => match chars.next()?.1 {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '0' => out.push('\0'),
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    '\'' => out.push('\''),
                    '\n' => {
                        // A line continued: the next line's indent goes.
                        while chars.clone().next().is_some_and(|(_, c)| c.is_whitespace()) {
                            chars.next();
                        }
                    }
                    'u' => {
                        let rest = &s[chars.clone().next()?.0..];
                        let end = rest.find('}')?;
                        let code = u32::from_str_radix(&rest[1..end], 16).ok()?;
                        out.push(char::from_u32(code)?);
                        for _ in 0..=end {
                            chars.next();
                        }
                    }
                    other => out.push(other),
                },
                c => out.push(c),
            }
        }
        None
    }

    /// Every key the program's source asks the catalogs for: the text of
    /// each `tr!`, `trn!` (both forms) and `tr_noop!`, outside comments and
    /// tests.
    fn source_keys() -> BTreeMap<String, Kind> {
        fn walk(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if path.is_dir() {
                    if name != "tests" {
                        walk(&path, files);
                    }
                } else if name.ends_with(".rs") && name != "tests.rs" {
                    files.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut keys = BTreeMap::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            let text = match text.find("#[cfg(test)]\nmod tests") {
                Some(i) => &text[..i],
                None => &text[..],
            };
            // Comments out: a doc comment's example is not a key.
            let code: String = text
                .lines()
                .map(|l| {
                    if l.trim_start().starts_with("//") {
                        ""
                    } else {
                        l
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            for (mac, kind, forms) in [
                ("tr!(", Kind::Text, 1),
                ("tr_noop!(", Kind::Text, 1),
                ("trn!(", Kind::Count, 2),
            ] {
                let mut rest = code.as_str();
                while let Some(i) = rest.find(mac) {
                    // `ntr!(` is not `tr!(`.
                    let before = rest[..i].chars().last();
                    rest = &rest[i + mac.len()..];
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    let mut tail = rest;
                    for _ in 0..forms {
                        let Some(open) = tail.trim_start().strip_prefix('"') else {
                            break;
                        };
                        let Some((key, after)) = literal(open) else {
                            break;
                        };
                        keys.insert(key, kind);
                        tail = after.trim_start().strip_prefix(',').unwrap_or(after);
                    }
                }
            }
        }
        keys
    }

    /// The `{name}`s a text fills in.
    fn placeholders(text: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let mut rest = text.replace("{{", "").replace("}}", "");
        while let Some(i) = rest.find('{') {
            let Some(end) = rest[i..].find('}') else {
                break;
            };
            names.insert(rest[i + 1..i + end].to_string());
            rest = rest[i + end + 1..].to_string();
        }
        names
    }

    fn catalog_entries(lang: Lang) -> HashMap<String, String> {
        serde_json::from_str(source(lang).unwrap())
            .unwrap_or_else(|e| panic!("{} catalog does not parse: {e}", lang.code()))
    }

    /// Every piece of text the program shows has a translation in every
    /// language — a new string left out of a catalog shows in English to a
    /// reader of another language.
    #[test]
    fn every_language_translates_every_text() {
        let keys = source_keys();
        assert!(
            keys.len() > 100,
            "the source scan found {} keys",
            keys.len()
        );
        for lang in Lang::ALL.into_iter().filter(|l| *l != Lang::En) {
            let catalog = catalog_entries(lang);
            let missing: Vec<&String> = keys
                .keys()
                .filter(|k| catalog.get(*k).is_none_or(|t| t.trim().is_empty()))
                .collect();
            assert!(
                missing.is_empty(),
                "{} lacks {} of {} texts, e.g. {:?}",
                lang.code(),
                missing.len(),
                keys.len(),
                &missing[..missing.len().min(15)]
            );
        }
    }

    /// What other crates write and the app shows through `lookup` is
    /// listed in `extern_text`, word for word — so the catalogs carry it.
    #[test]
    fn other_crates_labels_are_listed() {
        let listed: BTreeSet<&str> = crate::miscellaneous::extern_text::all()
            .into_iter()
            .collect();
        let mut written: Vec<String> = Vec::new();
        for m in clew_voice::models::MODELS {
            written.push(m.name.into());
            written.push(m.about.into());
        }
        written.extend(clew_core::mcp::config::Approve::ALL.map(|a| a.label().to_string()));
        written.push(clew_core::llm::Provider::Custom.label().into());
        written.extend(
            clew_core::outline::EntryKind::ALL
                .iter()
                .map(|k| k.label().to_string()),
        );
        // Each project map's title and description, as clew-core's source
        // gives them to `MapBuilder::new`.
        let core = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/clew-core/src");
        let mut maps = 0;
        for entry in std::fs::read_dir(core.join("maps")).unwrap().flatten() {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            let text = text.split("#[cfg(test)]").next().unwrap();
            for call in text.split("MapBuilder::new(").skip(1) {
                let mut rest = call;
                for field in 0..3 {
                    let Some(open) = rest.trim_start().strip_prefix('"') else {
                        break;
                    };
                    let (value, after) = literal(open).expect("a string");
                    if field > 0 {
                        written.push(value);
                    }
                    rest = after.trim_start().strip_prefix(',').unwrap_or(after);
                }
                maps += 1;
            }
        }
        assert!(maps > 30, "found {maps} maps");
        // Notices shown as clew-core wrote them.
        let notice = std::fs::read_to_string(core.join("debugadapter.rs")).unwrap();
        let start = "const ZIG_BACKEND_NOTICE: &str = \"";
        let at = notice.find(start).expect("the Zig notice") + start.len();
        written.push(literal(&notice[at..]).unwrap().0);
        let search = std::fs::read_to_string(core.join("search.rs")).unwrap();
        assert!(search.contains(
            "\"not a regular file inside the project (a link, or moved since the scan)\""
        ));
        assert!(search.contains("format!(\"larger than {} MB\", cap / (1024 * 1024))"));
        assert_eq!(clew_core::search::MAX_SEARCH_FILE_BYTES / (1024 * 1024), 64);
        let missing: Vec<&String> = written
            .iter()
            .filter(|w| !listed.contains(w.as_str()))
            .collect();
        assert!(missing.is_empty(), "not in extern_text: {missing:?}");
    }

    /// Numbers are written as the reader writes them: thousands apart,
    /// the decimal mark theirs; and time gone by in their words.
    #[test]
    fn numbers_and_times_are_written_as_the_reader_writes_them() {
        assert_eq!(number(1_858), "1,858");
        assert_eq!(number(999), "999");
        assert_eq!(number(1_234_567), "1,234,567");
        assert_eq!(decimal(2.25, 1), "2.2");
        with_lang(Lang::Fr, || {
            assert_eq!(number(1_858), "1\u{202f}858");
            assert_eq!(decimal(2.5, 1), "2,5");
        });
        with_lang(Lang::Ja, || assert_eq!(number(1_858), "1,858"));
        let now = 1_800_000_000;
        assert_eq!(ago(now - 5, now), "just now");
        assert_eq!(ago(now - 60, now), "1 minute ago");
        assert_eq!(ago(now - 3 * 86_400, now), "3 days ago");
        assert_eq!(ago(now - 400 * 86_400, now), "1 year ago");
    }

    /// No catalog keeps text the program no longer shows.
    #[test]
    fn no_catalog_keeps_text_the_program_dropped() {
        let keys = source_keys();
        for lang in Lang::ALL.into_iter().filter(|l| *l != Lang::En) {
            let stale: Vec<String> = catalog_entries(lang)
                .into_keys()
                .filter(|k| !keys.contains_key(k))
                .collect();
            assert!(
                stale.is_empty(),
                "{} keeps {} texts no longer shown, e.g. {:?}",
                lang.code(),
                stale.len(),
                &stale[..stale.len().min(15)]
            );
        }
    }

    /// A translation fills in what its English does, and nothing the code
    /// does not give: a count's form may leave its `{n}` out.
    #[test]
    fn translations_keep_their_placeholders() {
        let keys = source_keys();
        for lang in Lang::ALL.into_iter().filter(|l| *l != Lang::En) {
            for (english, translated) in catalog_entries(lang) {
                let Some(kind) = keys.get(&english) else {
                    continue;
                };
                let mut want = placeholders(&english);
                let got = placeholders(&translated);
                if *kind == Kind::Count {
                    want.remove("n");
                    let mut allowed = want.clone();
                    allowed.insert("n".into());
                    assert!(
                        want.is_subset(&got) && got.is_subset(&allowed),
                        "{}: {english:?} → {translated:?}",
                        lang.code()
                    );
                } else {
                    assert_eq!(want, got, "{}: {english:?} → {translated:?}", lang.code());
                }
            }
        }
    }

    /// Spoken, a translation is what the reader sees; a text with none
    /// reads in English.
    #[test]
    fn lookup_speaks_the_language_of_its_thread() {
        let key = source_keys()
            .into_keys()
            .find(|k| catalog(Lang::Fr).contains_key(k))
            .expect("a translated key");
        let english: &'static str = Box::leak(key.into_boxed_str());
        assert_eq!(lookup(english), english);
        with_lang(Lang::Fr, || {
            assert_eq!(lookup(english), catalog(Lang::Fr)[english]);
            assert_eq!(lookup("no such text, anywhere"), "no such text, anywhere");
        });
    }

    #[test]
    fn arguments_fill_their_places_and_braces_escape() {
        assert_eq!(
            format(
                "{n} of {total} — {{raw}}",
                &[("n", &3 as &dyn Display), ("total", &"ten" as &dyn Display)]
            ),
            "3 of ten — {raw}"
        );
        // An argument nobody gave stays as written.
        assert_eq!(format("{missing} left", &[]), "{missing} left");
    }

    #[test]
    fn system_locales_are_read_by_their_language() {
        assert_eq!(Lang::parse("fr_FR.UTF-8"), Some(Lang::Fr));
        assert_eq!(Lang::parse("zh-Hans-CN"), Some(Lang::Zh));
        assert_eq!(Lang::parse("ja-JP"), Some(Lang::Ja));
        assert_eq!(Lang::parse("ko"), Some(Lang::Ko));
        assert_eq!(Lang::parse("de-DE"), None);
    }

    #[test]
    fn each_thread_speaks_its_own_language_in_a_test() {
        assert_eq!(current(), Lang::En);
        with_lang(Lang::Fr, || {
            assert_eq!(current(), Lang::Fr);
            assert!(is_one(0), "French counts 0 as one");
        });
        assert_eq!(current(), Lang::En);
        assert!(!is_one(0));
    }
}
