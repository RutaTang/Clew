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
//! own thread (`with_lang`) without the others hearing it.

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

    /// The code macOS knows the language by, in an app's `AppleLanguages`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn apple_code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Fr => "fr",
            Lang::Zh => "zh-Hans",
            Lang::Ko => "ko",
            Lang::Ja => "ja",
        }
    }

    /// The tag the language servers and debuggers clew starts are told the
    /// reader reads (LSP's and DAP's `locale`), in the form editors send,
    /// so one that translates its messages recognises it. None for English,
    /// what they speak unasked.
    pub fn server_locale(self) -> Option<&'static str> {
        match self {
            Lang::En => None,
            Lang::Fr => Some("fr"),
            Lang::Zh => Some("zh-cn"),
            Lang::Ko => Some("ko"),
            Lang::Ja => Some("ja"),
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
    // And language servers started from now on, those that translate.
    clew_core::lsp::client::set_locale(lang.server_locale().map(str::to_string));
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
    // The tests speak English, whatever the machine running them speaks:
    // one that needs another language says so (`with_lang`).
    if cfg!(test) {
        return Lang::En;
    }
    static SYSTEM: OnceLock<Lang> = OnceLock::new();
    *SYSTEM.get_or_init(|| {
        // On macOS the preferred languages are clew's own choice once it
        // has recorded one for its bundle (see `tell_macos`).
        #[cfg(target_os = "macos")]
        if let Some(languages) = bundle_id().and_then(|id| system_beneath_clews(&run_defaults, &id))
        {
            return languages
                .iter()
                .find_map(|language| Lang::parse(language))
                .unwrap_or(Lang::En);
        }
        sys_locale::get_locales()
            .find_map(|locale| Lang::parse(&locale))
            .unwrap_or(Lang::En)
    })
}

const CONFIG_KEY: &str = "language";

/// The saved preference, applied once for the process, before its first
/// window's first frame; the preference in effect. A window opened later
/// speaks what the windows open speak: applied again, config.toml's choice —
/// which a save that failed, or another copy of clew, may have left saying
/// something else — moved every open window to it, and only in part (not
/// their menu bar, nor the servers that answer them).
pub fn init() -> LangPref {
    static APPLIED: std::sync::Once = std::sync::Once::new();
    APPLIED.call_once(|| {
        let pref = clew_core::globalconfig::get(CONFIG_KEY)
            .as_ref()
            .and_then(|v| v.as_str())
            .and_then(LangPref::parse)
            .unwrap_or(LangPref::System);
        set_pref(pref);
        // macOS's own parts of the window follow it from the next launch,
        // also when it was chosen in another copy of clew or in config.toml.
        #[cfg(target_os = "macos")]
        tell_macos(pref);
    });
    pref()
}

/// Speak what `pref` resolves to, and keep it for the next launch.
pub fn apply_and_save(pref: LangPref) -> Result<(), String> {
    set_pref(pref);
    #[cfg(target_os = "macos")]
    tell_macos(pref);
    clew_core::globalconfig::set_keys(vec![(
        CONFIG_KEY.into(),
        toml::Value::String(pref.as_str().into()),
    )])
}

/// macOS draws parts of an app itself — the standard menu items, the Open
/// panel, alerts — in the languages its preferences for that app list
/// (`AppleLanguages`). The choice is recorded for clew's own bundle there,
/// so those parts follow it from the next launch, and removed for System,
/// so they follow the system again — but only when what is recorded is
/// still clew's own: a language picked for clew in System Settings is the
/// user's, and stays. Off the UI thread: it runs `defaults`.
#[cfg(target_os = "macos")]
fn tell_macos(pref: LangPref) {
    std::thread::spawn(move || {
        if let Some(id) = bundle_id() {
            record_for_macos(&run_defaults, &id, pref);
        }
    });
}

/// The identifier of the app bundle clew runs from, if it runs from one.
#[cfg(target_os = "macos")]
fn bundle_id() -> Option<String> {
    let bundle = crate::macos::install::installed_bundle()?;
    let out = std::process::Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :CFBundleIdentifier"])
        .arg(bundle.join("Contents/Info.plist"))
        .output()
        .ok()?;
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !id.is_empty()).then_some(id)
}

/// Run macOS's `defaults` with `args`: what it printed, if it succeeded.
#[cfg(target_os = "macos")]
fn run_defaults(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/defaults")
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A way to run `defaults`: `run_defaults` on macOS, a stand-in in tests.
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
type Defaults<'a> = &'a dyn Fn(&[&str]) -> Option<String>;

/// The key clew keeps beside `AppleLanguages` in its bundle's preferences:
/// the language it wrote there itself.
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
const WROTE: &str = "ClewAppleLanguages";

/// The items of an array as `defaults read` prints it: `(\n    ko\n)`,
/// an item quoted when it holds a dash (`"zh-Hans"`).
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
fn defaults_array(printed: &str) -> Vec<String> {
    let inner = printed.trim();
    let inner = inner.strip_prefix('(').unwrap_or(inner);
    let inner = inner.strip_suffix(')').unwrap_or(inner);
    inner
        .split([',', '\n'])
        .map(|item| item.trim().trim_matches('"').to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// The language clew wrote for its bundle `id`, when that is still what
/// macOS keeps for it (not one picked since in System Settings).
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
fn written_by_clew(defaults: Defaults, id: &str) -> Option<String> {
    let wrote = defaults(&["read", id, WROTE])?.trim().to_string();
    let kept = defaults_array(&defaults(&["read", id, "AppleLanguages"])?);
    (kept == [wrote.as_str()]).then_some(wrote)
}

/// Record `pref` in the preferences of the bundle `id` (see
/// `tell_macos`).
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
fn record_for_macos(defaults: Defaults, id: &str, pref: LangPref) {
    match pref {
        LangPref::System => {
            if written_by_clew(defaults, id).is_some() {
                defaults(&["delete", id, "AppleLanguages"]);
            }
            // What is kept now, if anything, is not clew's.
            defaults(&["delete", id, WROTE]);
        }
        LangPref::Chosen(lang) => {
            let code = lang.apple_code();
            defaults(&["write", id, "AppleLanguages", "-array", code]);
            defaults(&["write", id, WROTE, "-string", code]);
        }
    }
}

/// The system's own languages, when what macOS keeps for clew's bundle `id`
/// is the language clew wrote there: the preferred languages a program
/// reads are then that one, not the system's.
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
fn system_beneath_clews(defaults: Defaults, id: &str) -> Option<Vec<String>> {
    written_by_clew(defaults, id)?;
    defaults(&["read", "-g", "AppleLanguages"]).map(|printed| defaults_array(&printed))
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
    let found = match current() {
        Lang::En => None,
        lang => catalog(lang).get(english).copied(),
    };
    // A key may say which sense of its words it means, before a U+0004
    // (gettext's way): "programming language\u{4}Language" is "Language"
    // as in Rust, not as in French. Untranslated, it reads as its words.
    found.unwrap_or_else(|| {
        english
            .rsplit_once('\u{4}')
            .map_or(english, |(_, text)| text)
    })
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
                // What went wrong, as another of clew's crates said it: in
                // the reader's language where it is one of their messages.
                Some((_, value)) if MESSAGE_PLACES.contains(&name) => {
                    out.push_str(&message(&value.to_string()));
                }
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

/// The places of a template that hold what another part of clew said went
/// wrong — filled with [`message`]'s translation of it.
const MESSAGE_PLACES: &[&str] = &["error", "reason", "message", "detail"];

/// A part of a message template: words as written, or a place a value
/// fills.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Part {
    Words(&'static str),
    Place(&'static str),
}

/// A template's parts, `{{`/`}}` left among its words.
fn parts(template: &'static str) -> Vec<Part> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        if rest[open..].starts_with("{{") {
            out.push(Part::Words(&rest[..open + 2]));
            rest = &rest[open + 2..];
            continue;
        }
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        if open > 0 {
            out.push(Part::Words(&rest[..open]));
        }
        out.push(Part::Place(&rest[open + 1..open + close]));
        rest = &rest[open + close + 1..];
    }
    if !rest.is_empty() {
        out.push(Part::Words(rest));
    }
    out
}

/// Whether `text` is `parts` filled in, and with what: each place the
/// shortest text that lets the rest match, never empty.
fn fill<'t>(parts: &[Part], text: &'t str, filled: &mut Vec<(&'static str, &'t str)>) -> bool {
    match parts.split_first() {
        None => text.is_empty(),
        Some((Part::Words(words), rest)) => {
            let words = words.replace("{{", "{").replace("}}", "}");
            text.strip_prefix(words.as_str())
                .is_some_and(|after| fill(rest, after, filled))
        }
        Some((Part::Place(name), rest)) => match rest.first() {
            None => {
                filled.push((name, text));
                !text.is_empty()
            }
            Some(Part::Words(next)) => {
                let next = next.replace("{{", "{").replace("}}", "}");
                let mut from = text.chars().next().map_or(1, char::len_utf8);
                while let Some(at) = text.get(from..).and_then(|t| t.find(next.as_str())) {
                    let at = from + at;
                    let kept = filled.len();
                    filled.push((name, &text[..at]));
                    if fill(rest, &text[at..], filled) {
                        return true;
                    }
                    filled.truncate(kept);
                    from = at + text[at..].chars().next().map_or(1, char::len_utf8);
                }
                false
            }
            // Two places side by side: where one ends cannot be told.
            Some(Part::Place(_)) => false,
        },
    }
}

/// A template a message fits: its text, what fills each place, and how
/// many bytes of it are words.
type Fit<'t> = (&'static str, Vec<(&'static str, &'t str)>, usize);

/// `text` as it is shown in the interface: each name a message sets in
/// backticks — Markdown's code span, as the model and the Markdown views
/// read it, `cargo` or `{name}` filled — set in the reader's quotation
/// marks instead (“cargo”, « cargo », 「cargo」). Shown as typed, the
/// backticks stood in plain text as marks of their own. So is a name GNU
/// programs quote as `name' in what they say (an error passed through):
/// its apostrophe closes it where it ends a word — `foldl'` and
/// `&'static str` keep theirs, and a span's own single-quoted words
/// (`ssh-keygen -f '/a b/known_hosts'`) theirs. A lone backtick, and a run
/// of them (a double span, a fence), is left as it is.
pub fn quoted(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('`') {
        return std::borrow::Cow::Borrowed(text);
    }
    let (open, close) = match current() {
        Lang::En | Lang::Zh => ("\u{201c}", "\u{201d}"),
        Lang::Fr => ("\u{ab}\u{a0}", "\u{a0}\u{bb}"),
        Lang::Ja => ("\u{300c}", "\u{300d}"),
        Lang::Ko => ("\u{2018}", "\u{2019}"),
    };
    // Every index below is a backtick's, an apostrophe's or a line break's:
    // ASCII, so a boundary of any text around them.
    let bytes = text.as_bytes();
    let run = |at: usize| bytes[at..].iter().take_while(|&&b| b == b'`').count();
    // Where the span a lone backtick opens just before `from` closes: at
    // the next lone backtick, or at an apostrophe that ends a word — on
    // the same line, after something. An apostrophe that starts a word (at
    // the span's start, or after a space) opens a single-quoted stretch of
    // the span's own — a shell command's `'/a path'` — and the next one
    // closes it, unless it is the shell's own escape of an apostrophe
    // inside one (`'O'\''Brien'`): neither ends the span.
    let close_of = |from: usize| {
        let mut quoting = false;
        let mut at = from;
        while let Some(&b) = bytes.get(at) {
            match b {
                b'\n' => return None,
                b'`' => return (at > from && run(at) == 1).then_some(at),
                b'\'' if quoting && bytes[at..].starts_with(b"'\\''") => at += 3,
                b'\'' if quoting => quoting = false,
                b'\'' if at == from || bytes[at - 1].is_ascii_whitespace() => quoting = true,
                b'\''
                    if !bytes
                        .get(at + 1)
                        .is_some_and(|&n| n.is_ascii_alphanumeric() || b"_'`".contains(&n)) =>
                {
                    return Some(at);
                }
                _ => {}
            }
            at += 1;
        }
        None
    };
    // Whether a lone backtick at `at` can open a span: where a name starts —
    // the text's start, after a space, an opening bracket or quote, or
    // after text in another script (Chinese and Japanese set no space
    // before it). One inside a word or a list of characters (`^_`|~`, the
    // characters a header name may hold) is a mark of its own.
    let opens = |at: usize| {
        at == 0 || {
            let before = bytes[at - 1];
            !before.is_ascii() || before.is_ascii_whitespace() || b"([{<\"'".contains(&before)
        }
    };
    let mut out = String::new();
    let mut copied = 0;
    let mut at = 0;
    while let Some(found) = text[at..].find('`') {
        let tick = at + found;
        let ticks = run(tick);
        if ticks > 1 {
            at = tick + ticks;
            continue;
        }
        if !opens(tick) {
            at = tick + 1;
            continue;
        }
        match close_of(tick + 1) {
            Some(end) => {
                out.push_str(&text[copied..tick]);
                out.push_str(open);
                out.push_str(&text[tick + 1..end]);
                out.push_str(close);
                copied = end + 1;
                at = end + 1;
            }
            None => at = tick + 1,
        }
    }
    if copied == 0 {
        return std::borrow::Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    std::borrow::Cow::Owned(out)
}

/// A message template: its text, its parts, and how many bytes of it are
/// words (what a match is ranked by).
type Template = (&'static str, Vec<Part>, usize);

/// A message clew-core, clew-server or clew-voice wrote in English (see
/// `extern_messages`), in the reader's language: the template it was
/// written from found, and its translation filled with what the message
/// held — each part that is itself such a message translated too. Text no
/// template matches (git's own words, a server's) stays as it came.
pub fn message(text: &str) -> String {
    if current() == Lang::En || text.is_empty() {
        return text.to_string();
    }
    // Asked again each frame for what is on screen: kept, per language.
    static SAID: std::sync::Mutex<Option<HashMap<(u8, String), String>>> =
        std::sync::Mutex::new(None);
    let key = (current().to_u8(), text.to_string());
    if let Some(said) = SAID
        .lock()
        .ok()
        .and_then(|held| held.as_ref().and_then(|m| m.get(&key).cloned()))
    {
        return said;
    }
    let said = message_at(text, 0);
    if let Ok(mut held) = SAID.lock() {
        let held = held.get_or_insert_with(HashMap::new);
        if held.len() > 512 {
            held.clear();
        }
        held.insert(key, said.clone());
    }
    said
}

fn message_at(text: &str, depth: usize) -> String {
    let lang = current();
    // Each level says a strictly shorter part of the text (what fills a
    // place, what precedes a dash, one of several lines), so the depth only
    // bounds the work; real messages nest five or six deep ("request
    // failed: " around a proxy's refusal, its advice, the advice's parts).
    if lang == Lang::En || depth > MAX_DEPTH || text.is_empty() {
        return text.to_string();
    }
    if let Some(said) = catalog(lang).get(text) {
        return one_counted(text, said);
    }
    use crate::miscellaneous::extern_messages::{EXTERNAL, MESSAGES, TAILS};
    static WHOLE: OnceLock<Vec<Template>> = OnceLock::new();
    static ENDS: OnceLock<Vec<Template>> = OnceLock::new();
    let whole = WHOLE.get_or_init(|| {
        let mut all = templates(MESSAGES);
        all.extend(templates(EXTERNAL));
        all
    });
    // A message with advice added after a dash ("…: refused — to reach the
    // host without the proxy, …"): each said apart. First where what
    // precedes the dash is a whole message itself — before a looser
    // template, whose last place would take the advice in, is tried on all
    // of it; then whatever precedes it.
    let ends = ENDS.get_or_init(|| templates(TAILS));
    let advice = |head_whole: bool| {
        let mut from = 0;
        while let Some(at) = text[from..].find(" — ").map(|at| from + at) {
            let head = &text[..at];
            let whole_head =
                catalog(lang).contains_key(head) || said_as(whole, head, depth + 1).is_some();
            if (whole_head || !head_whole)
                && let Some(tail) = said_as(ends, &text[at..], depth)
            {
                return Some(message_at(head, depth + 1) + &tail);
            }
            from = at + " — ".len();
        }
        None
    };
    if let Some(said) = advice(true) {
        return said;
    }
    if let Some(said) = said_as(whole, text, depth) {
        return said;
    }
    if let Some(said) = advice(false) {
        return said;
    }
    // Lines another program wrote, joined with "; " (git's last lines of
    // stderr): each said apart, when any of them is one clew knows.
    if text.contains("; ") {
        let pieces: Vec<&str> = text.split("; ").collect();
        let said: Vec<String> = pieces.iter().map(|p| message_at(p, depth + 1)).collect();
        if said.iter().zip(&pieces).any(|(s, p)| s != p) {
            return said
                .into_iter()
                .reduce(|earlier, later| {
                    crate::tr!(
                        "lines joined\u{4}{earlier}; {later}",
                        earlier = earlier,
                        later = later
                    )
                })
                .unwrap_or_default();
        }
    }
    text.to_string()
}

/// `said`, the translation of `english`, with a count's `{n}` given as one
/// where `english` is a count's singular ("1 entry skipped …"), which its
/// translation says with `{n}` (French says 0 and 1 alike) but which, met
/// as another crate's message, comes with no count to fill it.
fn one_counted(english: &str, said: &str) -> String {
    if said.contains("{n}") && !english.contains("{n}") {
        format(said, &[("n", &1)])
    } else {
        said.to_string()
    }
}

/// How deep [`message`] says the parts of parts of a message.
const MAX_DEPTH: usize = 8;

/// The templates of `list` that have places, parsed, with how much of each
/// is words.
fn templates(list: &'static [&'static str]) -> Vec<Template> {
    list.iter()
        .filter(|t| t.contains('{'))
        .map(|&t| {
            let parts = parts(t);
            let words = parts
                .iter()
                .map(|p| match p {
                    Part::Words(w) => w.len(),
                    Part::Place(_) => 0,
                })
                .sum();
            (t, parts, words)
        })
        .collect()
}

/// `text` in the reader's language, if one of `templates` fits all of it
/// (the one that says the most in words) and the catalog translates it.
fn said_as(templates: &[Template], text: &str, depth: usize) -> Option<String> {
    let mut best: Option<Fit<'_>> = None;
    for (template, parts, words) in templates {
        if best.as_ref().is_some_and(|(_, _, w)| w >= words) {
            continue;
        }
        let mut filled = Vec::new();
        if fill(parts, text, &mut filled) {
            best = Some((template, filled, *words));
        }
    }
    let (template, filled, _) = best?;
    let said = catalog(current()).get(template)?;
    let values: Vec<(&str, String)> = filled
        .into_iter()
        .map(|(name, value)| (name, message_at(value, depth + 1)))
        .collect();
    let mut args: Vec<(&str, &dyn Display)> = values
        .iter()
        .map(|(name, value)| (*name, value as &dyn Display))
        .collect();
    // A count's singular ("1 unreadable entry (first: {error})"): its
    // translation says the one as `{n}` (see `one_counted`).
    if !template.contains("{n}") {
        args.push(("n", &1));
    }
    Some(format(said, &args))
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
/// [`lookup`]. Marks it for the catalogs; gives it as
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
        // In one order everywhere: what is found does not hang on the order
        // the file system lists a folder in.
        files.sort();
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
                        // A key both counted and passed through (a core
                        // message, `extern_messages`) is a count's form:
                        // either way its `{n}` is filled in.
                        let seen = keys.entry(key).or_insert(kind);
                        if kind == Kind::Count {
                            *seen = Kind::Count;
                        }
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

    /// A name a message sets in backticks is shown in the reader's
    /// quotation marks, the backticks gone; text with no code span, and a
    /// stray backtick, stay as they are.
    #[test]
    fn a_code_span_is_shown_in_the_readers_quotation_marks() {
        assert_eq!(
            quoted("Connecting to `prod`…"),
            "Connecting to \u{201c}prod\u{201d}…"
        );
        assert_eq!(
            quoted("`a` and `b`"),
            "\u{201c}a\u{201d} and \u{201c}b\u{201d}"
        );
        assert!(matches!(quoted("no names"), std::borrow::Cow::Borrowed(_)));
        for kept in [
            "a lone ` mark",
            "``",
            "a ``double`` span",
            "split `a\nb` lines",
            "``x`",
            "`a``",
        ] {
            assert_eq!(quoted(kept), kept, "{kept:?}");
        }
        // A run of backticks opens nothing, and the backtick after it is a
        // span's own.
        assert_eq!(
            quoted("a ``double`` and `y`"),
            "a ``double`` and \u{201c}y\u{201d}"
        );
        // GNU's `name' quoting, in an error passed through; an apostrophe in
        // a name stays the name's.
        assert_eq!(
            quoted("cannot stat `a': no; see `b'"),
            "cannot stat \u{201c}a\u{201d}: no; see \u{201c}b\u{201d}"
        );
        // The characters a header name may hold, a backtick among them, and
        // the form a header takes after them.
        assert_eq!(
            quoted(
                "a header name holds letters, digits and !#$%&'*+-.^_`|~) \u{2014} write each as `Name: value`"
            ),
            "a header name holds letters, digits and !#$%&'*+-.^_`|~) \u{2014} write each as \u{201c}Name: value\u{201d}"
        );
        // A name with no space before it in a script that sets none.
        assert_eq!(quoted("已打开`a.rs`"), "已打开\u{201c}a.rs\u{201d}");
        // The shell's escape of an apostrophe inside a quoted path.
        assert_eq!(
            quoted(
                "forget it \u{2014} `ssh-keygen -f '/home/o'\\''brien/.ssh/known_hosts' -R host` \u{2014} then"
            ),
            "forget it \u{2014} \u{201c}ssh-keygen -f '/home/o'\\''brien/.ssh/known_hosts' -R host\u{201d} \u{2014} then"
        );
        // A command's own single quotes, as Clew writes them round a path
        // with a space or a host with its port, are the command's.
        assert_eq!(
            quoted(
                "forget it \u{2014} `ssh-keygen -f '/Users/me/Library/Application Support/clew/known_hosts' -R '[example.com]:2222'` \u{2014} then"
            ),
            "forget it \u{2014} \u{201c}ssh-keygen -f '/Users/me/Library/Application Support/clew/known_hosts' -R '[example.com]:2222'\u{201d} \u{2014} then"
        );
        assert_eq!(
            quoted("run `git commit -m 'fix it'` first"),
            "run \u{201c}git commit -m 'fix it'\u{201d} first"
        );
        assert_eq!(
            quoted("Opened `foldl'` and `&'static str`"),
            "Opened \u{201c}foldl'\u{201d} and \u{201c}&'static str\u{201d}"
        );
        with_lang(Lang::Fr, || {
            assert_eq!(quoted("`x`"), "\u{ab}\u{a0}x\u{a0}\u{bb}")
        });
        with_lang(Lang::Ja, || assert_eq!(quoted("`x`"), "\u{300c}x\u{300d}"));
        with_lang(Lang::Ko, || assert_eq!(quoted("`x`"), "\u{2018}x\u{2019}"));
        with_lang(Lang::Zh, || assert_eq!(quoted("`x`"), "\u{201c}x\u{201d}"));
    }

    /// A text that sends the reader to a page of Settings ("… in Settings →
    /// Voice") names one there is, by its title, and in every language by
    /// the title that page has in it: a page renamed left "Settings →
    /// Languages" pointing at nothing, and a group named as a page did too.
    #[test]
    fn every_settings_page_a_text_names_is_one() {
        let titles: Vec<&str> = crate::SettingsPage::ALL.map(|p| p.title()).into();
        let keys = source_keys();
        let naming: Vec<(&String, &str)> = keys
            .keys()
            .filter_map(|k| {
                let after = k.split_once("Settings → ")?.1;
                let title = titles.iter().find(|t| after.starts_with(**t));
                assert!(title.is_some(), "{k:?} names no page of Settings");
                title.map(|t| (k, *t))
            })
            .collect();
        assert!(naming.len() >= 3, "{naming:?}");
        for lang in Lang::ALL.into_iter().filter(|l| *l != Lang::En) {
            let catalog = catalog_entries(lang);
            for (key, title) in &naming {
                let page = &catalog[*title];
                assert!(
                    catalog[*key].contains(page.as_str()),
                    "{}: {:?} does not name the page as its title does ({page:?})",
                    lang.code(),
                    catalog[*key]
                );
            }
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
        for m in clew_voice::speech::VOICE_MODELS {
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
        // The verbs clew-server starts its agent steps' titles with.
        let agent = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("crates/clew-server/src/agent.rs"),
        )
        .unwrap();
        for verb in crate::miscellaneous::extern_text::STEP_VERBS {
            let lower = verb.to_ascii_lowercase();
            assert!(
                agent.contains(&format!("format!(\"{lower} "))
                    || agent.contains(&format!("format!(\"{lower}{{")),
                "clew-server no longer titles a step {lower:?}"
            );
        }
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

    /// A key with a sense before its words reads as its words where nothing
    /// translates it.
    #[test]
    fn a_keys_sense_is_not_shown() {
        assert_eq!(lookup("programming language\u{4}Language"), "Language");
        assert_eq!(lookup("no such text"), "no such text");
    }

    /// Where a paragraph set at `width` breaks its lines: the byte at which
    /// each line after the first starts.
    fn line_starts(text: &str, width: f32) -> Vec<usize> {
        use iced::advanced::graphics::text::cosmic_text as ct;
        let mut fs = ct::FontSystem::new();
        let mut buffer = ct::Buffer::new(&mut fs, ct::Metrics::new(14.0, 20.0));
        buffer.set_size(&mut fs, Some(width), None);
        buffer.set_wrap(&mut fs, ct::Wrap::Word);
        buffer.set_text(
            &mut fs,
            text,
            &ct::Attrs::new(),
            ct::Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut fs, false);
        buffer
            .layout_runs()
            .skip(1)
            .filter_map(|run| run.glyphs.first().map(|g| g.start))
            .collect()
    }

    /// Korean is set as Korean is read: a line breaks between words, at a
    /// space, never inside one — while Chinese and Japanese, written
    /// without spaces, still break between characters.
    #[test]
    fn korean_lines_break_between_words() {
        let korean =
            "변경된 파일을 정밀화하려면 언어 서버가 프로젝트를 모두 불러올 때까지 기다려야 합니다";
        let starts = line_starts(korean, 120.0);
        assert!(starts.len() >= 3, "it wraps: {starts:?}");
        for at in starts {
            assert!(
                korean[..at].ends_with(' '),
                "a line starts inside a word: {:?}",
                &korean[at..]
            );
        }
        let chinese = "变更的文件需要等语言服务器加载完整个项目之后才能精确化调用图";
        let starts = line_starts(chinese, 120.0);
        assert!(!starts.is_empty(), "it wraps");
        assert!(
            starts.iter().all(|&at| !chinese[..at].ends_with(' ')),
            "Chinese breaks between characters"
        );
    }

    /// A message another crate wrote is found by its template and said in
    /// the reader's language, its parts in place — and a part that is
    /// itself such a message translated too. English, and text no template
    /// fits, stay as they came.
    #[test]
    fn another_crates_message_is_said_in_the_readers_language() {
        let plain = "the session is ending";
        let shaped = "no speech model at {arg1}";
        let nested = "the connection to the server broke: {error}";
        for t in [plain, shaped, nested] {
            assert!(
                crate::miscellaneous::extern_messages::MESSAGES.contains(&t),
                "{t}"
            );
        }
        assert_eq!(message(plain), plain);
        with_lang(Lang::Fr, || {
            let fr = catalog(Lang::Fr);
            assert_eq!(message(plain), fr[plain]);
            let said = message("no speech model at /m/small.bin");
            assert_eq!(said, format(fr[shaped], &[("arg1", &"/m/small.bin")]));
            assert!(said.contains("/m/small.bin"), "{said}");
            let said = message("the connection to the server broke: the session is ending");
            assert_eq!(said, format(fr[nested], &[("error", &fr[plain])]));
            assert_eq!(
                message("fatal: not a git repository"),
                "fatal: not a git repository"
            );
            // As a sentence's error, in place.
            let shown = crate::tr!("Could not save the language: {error}", error = plain);
            assert!(shown.contains(fr[plain]), "{shown}");
        });
    }

    /// The parts those crates put into their messages and the advice they
    /// add after a dash are still theirs: each is in their source.
    #[test]
    fn message_parts_and_tails_are_the_crates_own() {
        use crate::miscellaneous::extern_messages::{PARTS, TAILS};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates");
        let mut source = String::new();
        for file in ["clew-core/src/net.rs", "clew-core/src/lsp/store.rs"] {
            source.push_str(&std::fs::read_to_string(root.join(file)).unwrap());
        }
        for said in PARTS.iter().chain(TAILS) {
            // As far as it is written plainly: up to its first character
            // the source spells another way (a `\u{..}`, a line's end).
            let plain: String = said
                .replace("{arg1}", "{}")
                .chars()
                .take_while(|c| c.is_ascii() && *c != '\n')
                .take(40)
                .collect();
            assert!(source.contains(&plain), "not in the source: {said:?}");
        }
    }

    /// Advice added after a dash is said in the reader's language, apart
    /// from what it follows, and a part the code puts into a message (why
    /// a time-out happened) too.
    #[test]
    fn a_messages_advice_and_parts_are_translated() {
        with_lang(Lang::Fr, || {
            let fr = catalog(Lang::Fr);
            let bypass = "list the host in no_proxy, which Clew reads in place of NO_PROXY";
            let tail = " — to reach the host without the proxy, {bypass}";
            let said = message(&format!(
                "the session is ending — to reach the host without the proxy, {bypass}"
            ));
            assert_eq!(
                said,
                format!(
                    "{}{}",
                    fr["the session is ending"],
                    format(fr[tail], &[("bypass", &fr[bypass])])
                )
            );
            let timed = message("timed out: the peer sent nothing for 30 s");
            assert!(timed.contains(fr["sent nothing"]), "{timed}");
        });
    }

    /// What git says reaches the reader in their language where clew knows
    /// it: inside clew's own message, and line by line where git wrote
    /// several (joined with "; "); a line clew does not know stays as git
    /// wrote it.
    #[test]
    fn gits_own_words_are_translated() {
        with_lang(Lang::Fr, || {
            let fr = catalog(Lang::Fr);
            let bad = format(fr["fatal: bad revision '{arg1}'"], &[("arg1", &"abc")]);
            assert_ne!(bad, "fatal: bad revision 'abc'");
            assert_eq!(
                message("git failed: fatal: bad revision 'abc'"),
                format(fr["git failed: {stderr}"], &[("stderr", &bad)])
            );
            let hint = "hint: use 'git switch' to change branches";
            assert_eq!(
                message(&format!("fatal: bad revision 'abc'; {hint}")),
                format(
                    fr["lines joined\u{4}{earlier}; {later}"],
                    &[("earlier", &bad), ("later", &hint)]
                )
            );
            let unknown = "warning: one thing; warning: another";
            assert_eq!(message(unknown), unknown);
            let os = format(
                fr["No such file or directory (os error {arg1})"],
                &[("arg1", &2)],
            );
            assert_eq!(
                message("cannot be read: No such file or directory (os error 2)"),
                format(fr["cannot be read: {e}"], &[("e", &os)])
            );
        });
    }

    /// What clew records for its bundle on macOS is removed when System is
    /// picked again, but a language picked for clew in System Settings is
    /// the user's and stays; and while clew's own is recorded, the system's
    /// languages are read past it.
    #[test]
    fn macos_keeps_only_clews_own_language_choice() {
        use std::cell::RefCell;
        let store: RefCell<HashMap<String, String>> = RefCell::new(HashMap::from([(
            "-g AppleLanguages".to_string(),
            "(\n    \"en-US\",\n    \"zh-Hans-CN\"\n)\n".to_string(),
        )]));
        let defaults = |args: &[&str]| -> Option<String> {
            let mut store = store.borrow_mut();
            match args {
                ["read", rest @ ..] => store.get(&rest.join(" ")).cloned(),
                ["delete", id, key] => store.remove(&format!("{id} {key}")).map(|_| String::new()),
                ["write", id, key, "-array", value] => {
                    let printed = format!("(\n    \"{value}\"\n)\n");
                    store.insert(format!("{id} {key}"), printed);
                    Some(String::new())
                }
                ["write", id, key, "-string", value] => {
                    store.insert(format!("{id} {key}"), format!("{value}\n"));
                    Some(String::new())
                }
                _ => None,
            }
        };
        let id = "app.clew";
        let kept = |store: &RefCell<HashMap<String, String>>| {
            store.borrow().get("app.clew AppleLanguages").cloned()
        };
        assert_eq!(system_beneath_clews(&defaults, id), None);
        record_for_macos(&defaults, id, LangPref::Chosen(Lang::Zh));
        assert_eq!(written_by_clew(&defaults, id).as_deref(), Some("zh-Hans"));
        assert_eq!(
            system_beneath_clews(&defaults, id),
            Some(vec!["en-US".to_string(), "zh-Hans-CN".to_string()])
        );
        record_for_macos(&defaults, id, LangPref::System);
        assert_eq!(kept(&store), None);
        // Picked in System Settings: not clew's, so it stays, and it is
        // what the preferred languages are.
        let french = "(\n    fr\n)\n".to_string();
        store
            .borrow_mut()
            .insert("app.clew AppleLanguages".into(), french.clone());
        record_for_macos(&defaults, id, LangPref::System);
        assert_eq!(kept(&store), Some(french.clone()));
        assert_eq!(system_beneath_clews(&defaults, id), None);
        // Also when clew had written one before it was replaced there.
        record_for_macos(&defaults, id, LangPref::Chosen(Lang::Ja));
        store
            .borrow_mut()
            .insert("app.clew AppleLanguages".into(), french.clone());
        assert_eq!(system_beneath_clews(&defaults, id), None);
        record_for_macos(&defaults, id, LangPref::System);
        assert_eq!(kept(&store), Some(french));
        assert_eq!(defaults_array("(\n    ko\n)\n"), ["ko"]);
    }

    /// Every message the other crates write, and every program's message
    /// clew knows, is said whole in every language: each template, its
    /// places filled with plain values, comes back as its translation so
    /// filled. One the matcher cannot fit (two places side by side, a place
    /// that may stand for nothing) fails here, not on a reader's screen.
    #[test]
    fn every_listed_message_is_said_whole_in_every_language() {
        use crate::miscellaneous::extern_messages::{EXTERNAL, MESSAGES};
        let mut unsaid = Vec::new();
        for lang in [Lang::Fr, Lang::Zh, Lang::Ko, Lang::Ja] {
            with_lang(lang, || {
                for &template in MESSAGES.iter().chain(EXTERNAL) {
                    // A place glued to a word's end, last, is a tail the
                    // code may leave empty ("…settings{detail}"), and a
                    // place that stands for nothing matches no template.
                    if let [.., Part::Words(words), Part::Place(_)] = parts(template).as_slice()
                        && words.ends_with(char::is_alphanumeric)
                    {
                        unsaid.push(format!("{}: {template} (a tail glued on)", lang.code()));
                    }
                    let mut sample = String::new();
                    let mut values = Vec::new();
                    for (i, part) in parts(template).into_iter().enumerate() {
                        match part {
                            Part::Words(words) => {
                                sample.push_str(&words.replace("{{", "{").replace("}}", "}"));
                            }
                            Part::Place(name) => {
                                let value = format!("v{i}");
                                sample.push_str(&value);
                                values.push((name, value));
                            }
                        }
                    }
                    // A count's singular is said with its one (see
                    // `one_counted`).
                    if !template.contains("{n}") {
                        values.push(("n", "1".to_string()));
                    }
                    let args: Vec<(&str, &dyn Display)> = values
                        .iter()
                        .map(|(name, value)| (*name, value as &dyn Display))
                        .collect();
                    let want = format(catalog(lang)[template], &args);
                    if message(&sample) != want {
                        unsaid.push(format!("{}: {template}", lang.code()));
                    }
                }
            });
        }
        assert!(unsaid.is_empty(), "not said whole: {unsaid:#?}");
    }

    /// A message said with or without a detail is two messages, each said
    /// whole: a place at a message's end cannot stand for nothing.
    #[test]
    fn a_refusal_is_said_with_or_without_its_detail() {
        with_lang(Lang::Ja, || {
            let refused = "the server refused the request (HTTP 401) — it needs credentials: \
                           add them as a header (e.g. Authorization) in the server's settings";
            let said = message(refused);
            assert!(!said.contains("refused"), "{said}");
            let said = message(&format!("{refused} (invalid_token)"));
            assert!(
                said.contains("invalid_token") && !said.contains("refused"),
                "{said}"
            );
        });
    }

    /// A proxy's refusal is said whole, with its advice: the message, what
    /// the proxy said and what to do, each in the reader's language.
    #[test]
    fn a_proxys_refusal_is_said_whole() {
        with_lang(Lang::Fr, || {
            let said = message(
                "the proxy http://…@proxy.corp:3128 answered HTTP 407 (Proxy Authentication \
                 Required): it accepts only NTLM or Negotiate logins, which Clew does not support — set a local relay that can log in for Clew, such as cntlm or px, as the proxy instead, or list the host in no_proxy, which Clew reads in place of \
                 NO_PROXY",
            );
            for english in ["answered", "accepts only", "local relay", "list the host"] {
                assert!(!said.contains(english), "{said}");
            }
            assert!(said.contains("http://…@proxy.corp:3128"), "{said}");
            let said = message(
                "the proxy http://proxy.corp:3128 answered HTTP 407 (Proxy Authentication \
                 Required): it asks for a user name and password",
            );
            assert!(!said.contains("asks for"), "{said}");
        });
    }

    /// A proxy's failure as Ask and Explain show it — inside "request
    /// failed: …", with advice after a dash whose own parts are said too —
    /// comes out in the reader's language to its last part; and advice is
    /// said apart before a looser message is tried on all of it, which
    /// would take the advice into its last place.
    #[test]
    fn a_proxys_failure_is_said_to_its_last_part() {
        let system_bypass = "list the host under “Bypass proxy settings for these hosts” in System Settings › Network › Details › Proxies, or in no_proxy, which Clew reads in place of NO_PROXY";
        let env_bypass = "list the host in no_proxy, which Clew reads in place of NO_PROXY";
        with_lang(Lang::Fr, || {
            let said = message(&format!(
                "request failed: the proxy http://proxy.corp:3128 answered HTTP 407 (Proxy \
                 Authentication Required): it asks for a user name and password — Clew cannot read proxy passwords from the keychain: put them in environment variables (percent-encoding any character in them that is not a letter or a digit); Clew then uses those instead of all of the system's proxy settings, so set \
                 https_proxy=http://user:password@proxy.corp:3128, or {system_bypass}"
            ));
            for english in [
                "request failed",
                "answered",
                "asks for",
                "cannot read",
                "percent-encoding",
                "list the host",
            ] {
                assert!(!said.contains(english), "{english}: {said}");
            }
            let said = message(&format!(
                "request failed: the SOCKS proxy 10.0.0.1:1080 could not reach api.x.com:443 \
                 (SOCKS4 answer 91: rejected or failed) — to reach the host without the proxy, \
                 {env_bypass}"
            ));
            for english in [
                "SOCKS4 answer",
                "rejected",
                "to reach the host",
                "list the host",
            ] {
                assert!(!said.contains(english), "{english}: {said}");
            }
            let said = message(&format!(
                "the SOCKS proxy 10.0.0.1:1080 closed the connection in its handshake — to \
                 reach the host without the proxy, {env_bypass}"
            ));
            assert!(!said.contains("closed the connection"), "{said}");
        });
    }

    /// A count's singular met as another crate's message ("1 entry skipped
    /// …", from a remote scan) is said with its one, not with the `{n}` its
    /// translation says it with.
    #[test]
    fn a_counts_singular_is_said_with_its_one() {
        with_lang(Lang::Fr, || {
            for text in [
                "1 entry skipped (name is not UTF-8)",
                "1 unreadable entry (first: permission denied)",
                "1 entry skipped (name is not UTF-8); 3 unreadable entries (first: denied)",
            ] {
                let said = message(text);
                assert!(!said.contains("{n}") && said.contains('1'), "{said}");
                assert_ne!(said, text);
            }
        });
    }

    /// What clew-core, clew-server and clew-voice write that may reach the
    /// screen is listed in `extern_messages`, word for word, so the
    /// catalogs carry it — found as the list was made: the literal of each
    /// `format!`, `Err(..)`, `msg =` and `Display` (`write!(f, ..)`,
    /// `f.write_str(..)`) outside tests and logging that reads as a
    /// sentence, or says in two words what failed before a colon and why.
    #[test]
    fn other_crates_messages_are_listed() {
        let listed: BTreeSet<&str> = crate::miscellaneous::extern_messages::MESSAGES
            .iter()
            .copied()
            .collect();
        let mut written = BTreeSet::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates");
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
        for krate in ["clew-core", "clew-server", "clew-speak", "clew-voice"] {
            let mut files = Vec::new();
            walk(&root.join(krate).join("src"), &mut files);
            for path in files {
                let text = std::fs::read_to_string(&path).unwrap();
                let text = text.split("#[cfg(test)]\nmod tests").next().unwrap();
                // A `Display` says its message with `write!(f, ..)` and
                // `f.write_str(..)`.
                for opener in [
                    "format!(",
                    "Err(",
                    "SayError::Voice(",
                    "SayError::Sentence(",
                    "Halt::Engine(",
                    "Halt::Batch(",
                    "msg =",
                    "write!(",
                    "f.write_str(",
                ] {
                    for (at, _) in text.match_indices(opener) {
                        let mut after = text[at + opener.len()..].trim_start();
                        // `write!(f, ..)`, the formatter on any line.
                        if opener == "write!(" {
                            let Some(rest) = after.strip_prefix('f') else {
                                continue;
                            };
                            let Some(rest) = rest.trim_start().strip_prefix(',') else {
                                continue;
                            };
                            after = rest.trim_start();
                        }
                        let Some(open) = after.strip_prefix('"') else {
                            continue;
                        };
                        // What this statement says before the literal: back to
                        // the end of the one before it (or a match arm's).
                        let mut from = at.saturating_sub(200);
                        while !text.is_char_boundary(from) {
                            from -= 1;
                        }
                        let before = &text[from..at];
                        let before = before
                            .rfind([';', '{', '}', ','])
                            .map_or(before, |end| &before[end + 1..]);
                        if LOGGING.iter().any(|l| before.contains(l)) {
                            continue;
                        }
                        let Some((said, tail)) = literal(open) else {
                            continue;
                        };
                        let formats = opener == "format!(" || opener == "write!(";
                        if !formats {
                            let tail = tail.trim_start();
                            if !(tail.starts_with(".into()")
                                || tail.starts_with(".to_string()")
                                || tail.starts_with(".to_owned()")
                                || tail.starts_with(')'))
                            {
                                continue;
                            }
                        }
                        // Braces in a plain literal are its own (JSON, code),
                        // not places: not a message to a person.
                        let plain_braces = !formats && said.contains(['{', '}']);
                        // Two words are enough for a formatted message
                        // that says what failed and then why ("git failed:
                        // {stderr}").
                        let short_with_why = formats && {
                            let words: Vec<&str> = said.split_whitespace().collect();
                            let plain = |w: &str| {
                                !w.is_empty() && w.chars().all(|c| c.is_ascii_alphabetic())
                            };
                            let place = |w: &str| {
                                let w = w.trim_matches('`');
                                w.starts_with('{') && w.find('}') == Some(w.len() - 1)
                            };
                            match words.as_slice() {
                                // "git failed: {stderr}"
                                [first, second, why] => {
                                    plain(first)
                                        && second.strip_suffix(':').is_some_and(plain)
                                        && place(why)
                                        && !why.contains('`')
                                }
                                // "MCP server `{name}`: {why}"
                                [first, second, named, why] => {
                                    plain(first)
                                        && plain(second)
                                        && named.strip_suffix(':').is_some_and(place)
                                        && place(why)
                                        && !why.contains('`')
                                }
                                _ => false,
                            }
                        };
                        if (reads_as_a_sentence(&said) || short_with_why) && !plain_braces {
                            written.insert(as_template(&said));
                        }
                    }
                }
            }
        }
        // UPDATE_EXTERN_MESSAGES=1 writes the list as the crates have it
        // (then translate what is new in every catalog).
        if std::env::var_os("UPDATE_EXTERN_MESSAGES").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/miscellaneous/extern_messages.rs");
            let old = std::fs::read_to_string(&path).unwrap();
            let start = old.find("pub(crate) const MESSAGES").unwrap();
            let end = start + old[start..].find("\n];\n").unwrap() + "\n];\n".len();
            let mut new = format!("{}pub(crate) const MESSAGES: &[&str] = &[\n", &old[..start]);
            for w in &written {
                new.push_str(&format!("    tr_noop!({w:?}),\n"));
            }
            new.push_str("];\n");
            // What follows the list (the parts and tails kept by hand) stays.
            new.push_str(&old[end..]);
            std::fs::write(&path, new).unwrap();
            return;
        }
        let missing: Vec<&String> = written
            .iter()
            .filter(|w| !listed.contains(w.as_str()))
            .collect();
        assert!(missing.is_empty(), "not in extern_messages: {missing:?}");
        let stale: Vec<&&str> = listed.iter().filter(|l| !written.contains(**l)).collect();
        assert!(stale.is_empty(), "no longer written: {stale:?}");
    }

    /// What, written just before a literal, makes it a log line's.
    const LOGGING: &[&str] = &[
        "eprintln!(",
        "println!(",
        "log::",
        "debug!(",
        "info!(",
        "warn!(",
        "error!(",
        "trace!(",
        "panic!(",
        "assert",
        "unreachable!(",
        "expect(",
        "writeln!(",
        "write!(",
    ];

    /// Whether a literal reads as a message to a person: three words or
    /// more, on one line, not code.
    fn reads_as_a_sentence(said: &str) -> bool {
        let mut words = 0;
        let mut plain = String::new();
        let mut depth = 0;
        for c in said.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                c if depth == 0 => plain.push(c),
                _ => {}
            }
        }
        for word in plain.split(|c: char| !c.is_ascii_alphabetic()) {
            if word.len() >= 2 {
                words += 1;
            }
        }
        let code = ["{ \"", "=>", "::", "fn ", "impl "];
        words >= 3
            && said.contains(' ')
            && said.matches('\n').count() <= 1
            && said.chars().count() <= 220
            && !said.starts_with(char::is_whitespace)
            && !said.starts_with(['[', '<', '$'])
            && (!said.starts_with('{') || opens_with_a_place(said))
            && !said.starts_with("--")
            && !said.contains("{{")
            && !said.trim_end().ends_with(';')
            && !said.trim_end().ends_with("()")
            && !code.iter().any(|c| said.contains(c))
    }

    /// Whether a literal opens with a format argument followed by words
    /// ("{url}: the transfer took too long", "{path} is not valid TOML"),
    /// not with a brace of its own (JSON, code).
    fn opens_with_a_place(said: &str) -> bool {
        said.strip_prefix('{')
            .and_then(|rest| rest.split_once('}'))
            .is_some_and(|(inner, after)| {
                let name = inner.split(':').next().unwrap_or_default();
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && (after.starts_with(' ') || after.starts_with(": "))
            })
    }

    /// A format string as a template: each argument a named place, a
    /// positional one `{argN}`, its format spec dropped.
    fn as_template(said: &str) -> String {
        let mut out = String::new();
        let mut rest = said;
        let mut positional = 0;
        while let Some(i) = rest.find(['{', '}']) {
            out.push_str(&rest[..i]);
            let tail = &rest[i..];
            if tail.starts_with("{{") || tail.starts_with("}}") {
                out.push_str(&tail[..2]);
                rest = &tail[2..];
            } else if tail.starts_with('{') {
                let end = tail.find('}').unwrap();
                let inner = tail[1..end].split(':').next().unwrap().trim();
                let name = if inner.is_empty() {
                    positional += 1;
                    format!("arg{positional}")
                } else if let Ok(n) = inner.parse::<usize>() {
                    format!("arg{}", n + 1)
                } else {
                    inner
                        .chars()
                        .map(|c| {
                            if c.is_alphanumeric() || c == '_' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .collect()
                };
                out.push('{');
                out.push_str(&name);
                out.push('}');
                rest = &tail[end + 1..];
            } else {
                out.push('}');
                rest = &tail[1..];
            }
        }
        out.push_str(rest);
        out
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

    /// The tests speak English whatever the machine running them speaks:
    /// on one set to French or Japanese, every English assertion failed.
    #[test]
    fn the_system_a_test_runs_on_speaks_english() {
        assert_eq!(system(), Lang::En);
        assert_eq!(LangPref::System.resolve(), Lang::En);
    }

    /// The saved language is applied once for the process: a window opened
    /// later (`App::blank` runs `init`) leaves the language every window
    /// speaks as it is. It applied config.toml's again — one a save that
    /// failed, or another copy of clew, had left saying something else.
    #[test]
    fn a_later_window_leaves_the_language_spoken() {
        let _first = crate::app::tests::blank_app();
        // The live choice, where config.toml names none: English, chosen —
        // what the tests beside this one speak, so none of them is moved.
        set_pref(LangPref::Chosen(Lang::En));
        let _later = crate::app::tests::blank_app();
        assert_eq!(pref(), LangPref::Chosen(Lang::En));
        set_pref(LangPref::System);
    }
}
