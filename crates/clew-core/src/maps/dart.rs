//! Dart and Flutter's maps:
//!
//! - **widget tree** — what each widget's `build` builds, nested as it is
//!   written: the project's own widgets (a `State<W>`'s build is `W`'s) and
//!   the framework widgets under them, each arrow labelled by the slot it
//!   fills (`body`, `child`, `children`); `runApp(…)` at the root;
//! - **state management** — where state lives and who reads it: Riverpod
//!   providers (declared or `@riverpod`-generated) and the widgets and
//!   providers that `ref.watch`/`read`/`listen` them; `provider` package
//!   notifiers, who provides them (`create:`) and who reads them
//!   (`context.watch<T>()`, `Provider.of<T>`, `Consumer<T>`); Blocs and
//!   Cubits, the events each handles (`on<E>`), who adds them and who
//!   builds on them (`BlocBuilder<B, S>`).
//!
//! Read from tokens rather than the tree: what matters is how calls nest,
//! which survives the grammar's gaps (Dart 3 class modifiers, records).

use std::collections::{HashMap, HashSet};

use clew_protocol::ProjectMap;

use super::{Corpus, MapBuilder, Source, line_at};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let files: Vec<File> = corpus.in_langs(&[Lang::Dart]).map(File::read).collect();
    let mut out = Vec::new();
    out.extend(widgets(&files));
    out.extend(state(&files));
    out
}

// -- tokens -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Str,
    Punct,
}

#[derive(Debug, Clone, Copy)]
struct Tok<'s> {
    kind: Kind,
    text: &'s str,
    at: usize,
}

/// Where a string literal starting at `q` (its first quote) ends.
fn string_end(b: &[u8], q: usize, raw: bool) -> usize {
    let quote = b[q];
    let triple = b.get(q + 1) == Some(&quote) && b.get(q + 2) == Some(&quote);
    let mut i = q + if triple { 3 } else { 1 };
    while i < b.len() {
        let c = b[i];
        if !raw && c == b'\\' {
            i += 2;
            continue;
        }
        if !raw && c == b'$' && b.get(i + 1) == Some(&b'{') {
            // `${…}`: to its closing brace.
            let mut depth = 0;
            while i < b.len() {
                match b[i] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == quote {
            if !triple {
                return i + 1;
            }
            if b.get(i + 1) == Some(&quote) && b.get(i + 2) == Some(&quote) {
                return i + 3;
            }
        }
        if c == b'\n' && !triple {
            return i;
        }
        i += 1;
    }
    b.len()
}

fn tokens(src: &str) -> Vec<Tok<'_>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 0;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        let raw = c == b'r' && matches!(b.get(i + 1), Some(b'\'' | b'"'));
        let q = if raw { i + 1 } else { i };
        if matches!(b[q], b'\'' | b'"') {
            let end = string_end(b, q, raw);
            out.push(Tok {
                kind: Kind::Str,
                text: &src[i..end],
                at: i,
            });
            i = end;
            continue;
        }
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80 {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80)
            {
                i += 1;
            }
            out.push(Tok {
                kind: Kind::Word,
                text: &src[start..i],
                at: start,
            });
            continue;
        }
        let len = if src[i..].starts_with("...") {
            3
        } else if ["=>", "?.", "??", ".."]
            .iter()
            .any(|p| src[i..].starts_with(p))
        {
            2
        } else {
            1
        };
        out.push(Tok {
            kind: Kind::Punct,
            text: &src[i..i + len],
            at: i,
        });
        i += len;
    }
    out
}

/// Each bracket's partner: `(`↔`)`, `[`↔`]`, `{`↔`}`.
fn partners(toks: &[Tok]) -> Vec<Option<usize>> {
    let mut out = vec![None; toks.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if t.kind != Kind::Punct {
            continue;
        }
        match t.text {
            "(" | "[" | "{" => stack.push(i),
            ")" | "]" | "}" => {
                let want = match t.text {
                    ")" => "(",
                    "]" => "[",
                    _ => "{",
                };
                // Pop to the opener it closes: an unbalanced file still pairs.
                if let Some(pos) = stack.iter().rposition(|o| toks[*o].text == want) {
                    let o = stack[pos];
                    stack.truncate(pos);
                    out[o] = Some(i);
                    out[i] = Some(o);
                }
            }
            _ => {}
        }
    }
    out
}

/// Past `<…>` type arguments at `i`, if they are there.
fn skip_type_args(toks: &[Tok], i: usize) -> Option<usize> {
    if toks.get(i)?.text != "<" {
        return Some(i);
    }
    let mut depth = 0;
    let mut j = i;
    while let Some(t) = toks.get(j) {
        match (t.kind, t.text) {
            (Kind::Punct, "<") => depth += 1,
            (Kind::Punct, ">") => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            (Kind::Word, _) | (Kind::Punct, "," | "?" | "." | "(" | ")") => {}
            _ => return None,
        }
        j += 1;
    }
    None
}

fn is_type_name(w: &str) -> bool {
    w.trim_start_matches('_')
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
}

/// A constructor call at `i`: `Name`, `Name<T>`, `Name.named`, then `(`.
/// Its shown name and the index of its `(`.
fn constructor(toks: &[Tok], i: usize) -> Option<(String, usize)> {
    let t = toks.get(i)?;
    if t.kind != Kind::Word || !is_type_name(t.text) {
        return None;
    }
    if i > 0 && matches!(toks[i - 1].text, "." | "?.") {
        return None;
    }
    let mut j = skip_type_args(toks, i + 1)?;
    let mut name = t.text.to_string();
    if toks.get(j).is_some_and(|d| d.text == ".")
        && toks.get(j + 1).is_some_and(|w| w.kind == Kind::Word)
    {
        name = format!("{name}.{}", toks[j + 1].text);
        j += 2;
    }
    (toks.get(j)?.text == "(").then_some((name, j))
}

// -- a file's classes ---------------------------------------------------------------------------

struct Class<'s> {
    name: &'s str,
    /// `extends X<…>`: X, and `X<…>` as written.
    base: Option<&'s str>,
    base_written: String,
    /// The first type argument of the base: `W` in `State<W>`.
    base_arg: Option<&'s str>,
    mixins: Vec<&'s str>,
    /// Token indices of its body's braces.
    open: usize,
    close: usize,
    at: usize,
}

struct File<'a> {
    source: &'a Source,
    toks: Vec<Tok<'a>>,
    pairs: Vec<Option<usize>>,
    /// How many brackets enclose each token (a bracket itself is outside
    /// the ones it opens or closes).
    depth: Vec<u32>,
    classes: Vec<Class<'a>>,
}

impl<'a> File<'a> {
    fn read(source: &'a Source) -> File<'a> {
        let toks = tokens(&source.text);
        let pairs = partners(&toks);
        let mut classes = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            let t = toks[i];
            if t.kind == Kind::Punct && matches!(t.text, "(" | "[" | "{") {
                i = pairs[i].map_or(i + 1, |c| c + 1);
                continue;
            }
            if t.kind == Kind::Word
                && matches!(t.text, "class" | "mixin")
                && toks.get(i + 1).is_some_and(|n| n.kind == Kind::Word)
            {
                let name = toks[i + 1].text;
                let mut j = i + 2;
                let (mut base, mut base_arg, mut base_written, mut mixins) =
                    (None, None, String::new(), Vec::new());
                let mut clause = "";
                while let Some(h) = toks.get(j) {
                    if h.text == "{" || h.text == ";" {
                        break;
                    }
                    match h.text {
                        "extends" | "with" | "implements" | "on" => clause = h.text,
                        "<" => {
                            let end = skip_type_args(&toks, j).unwrap_or(j + 1);
                            if clause == "extends" || (clause == "on" && base.is_some()) {
                                base_written = format!(
                                    "{}{}",
                                    base.unwrap_or(""),
                                    source.text[h.at..toks[end - 1].at + 1]
                                        .split_whitespace()
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                );
                                base_arg = toks
                                    .get(j + 1)
                                    .filter(|a| a.kind == Kind::Word)
                                    .map(|a| a.text);
                            }
                            j = end;
                            continue;
                        }
                        w if h.kind == Kind::Word => match clause {
                            "extends" | "on" if base.is_none() => {
                                base = Some(w);
                                base_written = w.to_string();
                            }
                            "with" => mixins.push(w),
                            _ => {}
                        },
                        _ => {}
                    }
                    j += 1;
                }
                if toks.get(j).is_some_and(|b| b.text == "{")
                    && let Some(close) = pairs[j]
                {
                    classes.push(Class {
                        name,
                        base,
                        base_written,
                        base_arg,
                        mixins,
                        open: j,
                        close,
                        at: t.at,
                    });
                    i = close + 1;
                    continue;
                }
            }
            i += 1;
        }
        let mut depth = Vec::with_capacity(toks.len());
        let mut d = 0u32;
        for t in &toks {
            match (t.kind, t.text) {
                (Kind::Punct, ")" | "]" | "}") => {
                    d = d.saturating_sub(1);
                    depth.push(d);
                }
                (Kind::Punct, "(" | "[" | "{") => {
                    depth.push(d);
                    d += 1;
                }
                _ => depth.push(d),
            }
        }
        File {
            source,
            toks,
            pairs,
            depth,
            classes,
        }
    }

    /// Where the expression, argument or statement at token `i` ends: the
    /// first `,` or `;` at its depth, or the bracket that encloses it.
    fn expr_end(&self, i: usize) -> usize {
        let d = self.depth[i];
        let mut k = i + 1;
        while k < self.toks.len() {
            let t = self.toks[k];
            if self.depth[k] < d || (self.depth[k] == d && matches!(t.text, "," | ";")) {
                return k;
            }
            // Past type arguments, whose commas are not this one's.
            if t.text == "<"
                && let Some(after) = skip_type_args(&self.toks, k)
            {
                k = after;
                continue;
            }
            k += 1;
        }
        self.toks.len()
    }

    fn line(&self, tok: usize) -> usize {
        line_at(&self.source.text, self.toks[tok].at)
    }

    /// The class whose body holds token `i`.
    fn class_at(&self, i: usize) -> Option<&Class<'a>> {
        self.classes.iter().find(|c| c.open < i && i < c.close)
    }
}

const WIDGET_BASES: &[&str] = &[
    "StatelessWidget",
    "StatefulWidget",
    "ConsumerWidget",
    "ConsumerStatefulWidget",
    "HookWidget",
    "HookConsumerWidget",
    "StatefulHookWidget",
    "StatefulHookConsumerWidget",
    "InheritedWidget",
    "InheritedNotifier",
    "InheritedModel",
    "ProxyWidget",
];

const STATE_BASES: &[&str] = &["State", "ConsumerState"];

/// The project's widgets by name, those extending another of them too.
fn widget_classes(files: &[File]) -> HashMap<String, (String, usize)> {
    let mut found: HashMap<String, (String, usize)> = HashMap::new();
    loop {
        let before = found.len();
        for f in files {
            for c in &f.classes {
                if c.base
                    .is_some_and(|b| WIDGET_BASES.contains(&b) || found.contains_key(b))
                {
                    found
                        .entry(c.name.to_string())
                        .or_insert((f.source.rel.clone(), line_at(&f.source.text, c.at)));
                }
            }
        }
        if found.len() == before {
            return found;
        }
    }
}

/// The widget a class's build belongs to: itself, or `W` for `State<W>`.
fn widget_of<'s>(c: &Class<'s>) -> &'s str {
    match (c.base, c.base_arg) {
        (Some(b), Some(w)) if STATE_BASES.contains(&b) => w,
        _ => c.name,
    }
}

// -- widget tree ----------------------------------------------------------------------------------

const WIDGET_SLOTS: &[&str] = &[
    "child",
    "body",
    "home",
    "title",
    "appBar",
    "leading",
    "trailing",
    "floatingActionButton",
    "bottomNavigationBar",
    "bottomSheet",
    "drawer",
    "endDrawer",
    "content",
    "icon",
    "label",
    "subtitle",
    "flexibleSpace",
    "bottom",
    "placeholder",
    "background",
    "secondaryBackground",
    "prefix",
    "suffix",
    "prefixIcon",
    "suffixIcon",
    "avatar",
    "header",
    "footer",
    "loading",
    "error",
    "empty",
    "page",
];

const LIST_SLOTS: &[&str] = &[
    "children",
    "actions",
    "slivers",
    "tabs",
    "items",
    "destinations",
    "pages",
    "persistentFooterButtons",
];

struct Frame {
    open: usize,
    close: usize,
    /// The widget whose arguments these are.
    widget: Option<usize>,
    /// Set while the expression being read here is a widget: the slot it
    /// fills (empty for a returned one).
    slot: Option<String>,
    /// A list whose every element is a widget.
    list: bool,
    /// Whether a `return` here returns a widget.
    builds: bool,
}

/// Whether the callback whose parameters close at `close` returns a widget:
/// one passed as a builder (`builder: (context) => …`, `itemBuilder:`), a
/// route (`'/': (c) => …`), or mapped into a list (`items.map((i) => …)`).
fn callback_builds(toks: &[Tok], pairs: &[Option<usize>], stack: &[Frame], close: usize) -> bool {
    if toks.get(close).is_none_or(|t| t.text != ")") {
        return false;
    }
    let Some(open) = pairs[close] else {
        return false;
    };
    let Some(before) = open.checked_sub(1).map(|b| toks[b]) else {
        return false;
    };
    match before.text {
        ":" => open.checked_sub(2).is_some_and(|l| {
            let label = toks[l];
            label.kind == Kind::Str || (label.kind == Kind::Word && label.text.ends_with("uilder"))
        }),
        "(" | "," => stack.last().is_some_and(|f| {
            f.open > 0
                && toks[f.open].text == "("
                && matches!(toks[f.open - 1].text, "map" | "generate" | "expand")
        }),
        _ => false,
    }
}

/// Reads the widgets built in tokens `start..end` of `file` into `map`,
/// under `root`; `slot` is set when the first expression is a widget.
fn scan_widgets(
    map: &mut MapBuilder,
    file: &File,
    project: &HashMap<String, (String, usize)>,
    (start, end): (usize, usize),
    root: Option<usize>,
    slot: Option<&str>,
) {
    let toks = &file.toks;
    let rel = &file.source.rel;
    let mut stack = vec![Frame {
        open: start.saturating_sub(1),
        close: end,
        widget: root,
        slot: slot.map(str::to_string),
        list: false,
        builds: true,
    }];
    let mut opener: Option<(usize, Option<usize>)> = None;
    let mut i = start;
    while i < end.min(toks.len()) {
        let t = toks[i];
        let top = stack.len() - 1;
        if t.kind == Kind::Punct {
            match t.text {
                "(" | "[" | "{" => {
                    let widget = match opener {
                        Some((at, w)) if at == i => w,
                        _ => None,
                    };
                    // `children: [ … ]`: every element is a widget.
                    let list = t.text == "["
                        && i >= 2
                        && toks[i - 1].text == ":"
                        && LIST_SLOTS.contains(&toks[i - 2].text);
                    let builds = t.text == "{"
                        && match toks[i - 1].text {
                            // A callback's body, or a block statement's.
                            ")" if file.pairs[i - 1]
                                .and_then(|o| o.checked_sub(1))
                                .is_some_and(|b| matches!(toks[b].text, ":" | "(" | ",")) =>
                            {
                                callback_builds(toks, &file.pairs, &stack, i - 1)
                            }
                            _ => stack[top].builds,
                        };
                    stack.push(Frame {
                        open: i,
                        close: file.pairs[i].unwrap_or(end),
                        widget,
                        slot: list.then(|| toks[i - 2].text.to_string()),
                        list,
                        builds,
                    });
                }
                ")" | "]" | "}" => {
                    while stack.len() > 1 && stack.last().is_some_and(|f| f.close <= i) {
                        stack.pop();
                    }
                }
                "," => {
                    let f = &mut stack[top];
                    if !f.list {
                        f.slot = None;
                    }
                }
                ";" => stack[top].slot = None,
                "=>" if callback_builds(toks, &file.pairs, &stack, i - 1) => {
                    stack[top].slot.get_or_insert_with(String::new);
                }
                ":" if i >= 2
                    && toks[i - 1].kind == Kind::Word
                    && WIDGET_SLOTS.contains(&toks[i - 1].text)
                    && matches!(toks[i - 2].text, "(" | ",") =>
                {
                    stack[top].slot = Some(toks[i - 1].text.to_string());
                }
                _ => {}
            }
            i += 1;
            continue;
        }
        if t.kind == Kind::Word && t.text == "return" && stack[top].builds {
            stack[top].slot = Some(String::new());
            i += 1;
            continue;
        }
        if let Some((name, paren)) = constructor(toks, i)
            && let Some(slot) = stack[top].slot.clone()
        {
            let parent = stack.iter().rev().find_map(|f| f.widget);
            let label = if slot.is_empty() {
                "builds".to_string()
            } else {
                slot
            };
            let at = match project.get(&name) {
                Some((r, l)) => {
                    let n = map.node("widget", &name, &name);
                    map.place(n, r, *l);
                    n
                }
                None => {
                    let n = map.node("flutter", &format!("{rel}:{}", t.at), &name);
                    map.place(n, rel, file.line(i));
                    n
                }
            };
            map.edge(parent, at, &label);
            opener = Some((paren, at));
            i = paren;
            continue;
        }
        i += 1;
    }
}

fn widgets(files: &[File]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "flutter.widgets",
        "Widget tree",
        "what each widget's build builds, nested as written: the project's widgets (a State's build is its widget's) and the framework widgets under them, each arrow labelled by the slot it fills; runApp at the root",
    );
    let project = widget_classes(files);
    for file in files {
        let toks = &file.toks;
        for c in &file.classes {
            let widget = widget_of(c);
            let Some((r, l)) = project.get(widget) else {
                continue;
            };
            let root = map.node("widget", widget, widget);
            map.place(root, r, *l);
            if c.name == widget {
                if let Some(base) = c.base {
                    map.detail(root, base);
                }
            } else {
                let base = map
                    .get(root.unwrap_or(usize::MAX))
                    .map(|n| n.detail.clone())
                    .unwrap_or_default();
                if !base.contains("state ") {
                    map.detail(
                        root,
                        format!("{base} · state {}", c.name)
                            .trim_start_matches(" · ")
                            .to_string(),
                    );
                }
            }
            // `Widget build(BuildContext context) { … }` or `=> …;`, at the
            // class body's own level.
            let mut i = c.open + 1;
            while i < c.close {
                let t = toks[i];
                if t.kind == Kind::Punct && matches!(t.text, "(" | "[" | "{") {
                    if t.text == "("
                        && i > 0
                        && toks[i - 1].text == "build"
                        && let Some(params_end) = file.pairs[i]
                    {
                        let mut b = params_end + 1;
                        if toks.get(b).is_some_and(|x| x.text == "async") {
                            b += 1;
                        }
                        match toks.get(b).map(|x| x.text) {
                            Some("{") => {
                                let close = file.pairs[b].unwrap_or(c.close);
                                scan_widgets(&mut map, file, &project, (b + 1, close), root, None);
                                i = close + 1;
                                continue;
                            }
                            Some("=>") => {
                                let end = (b..c.close)
                                    .find(|k| {
                                        toks[*k].text == ";"
                                            && file.class_at(*k).is_some_and(|x| x.open == c.open)
                                    })
                                    .unwrap_or(c.close);
                                scan_widgets(
                                    &mut map,
                                    file,
                                    &project,
                                    (b + 1, end),
                                    root,
                                    Some(""),
                                );
                                i = end + 1;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    i = file.pairs[i].map_or(i + 1, |p| p + 1);
                    continue;
                }
                i += 1;
            }
        }
        // `runApp(App())`.
        for (i, t) in toks.iter().enumerate() {
            if t.text == "runApp" && toks.get(i + 1).is_some_and(|p| p.text == "(") {
                let Some(close) = file.pairs[i + 1] else {
                    continue;
                };
                let entry = map.node("entry", &format!("{}:{}", file.source.rel, t.at), "runApp");
                map.place(entry, &file.source.rel, file.line(i));
                scan_widgets(&mut map, file, &project, (i + 2, close), entry, Some(""));
            }
        }
    }
    map.finish()
}

// -- state management -------------------------------------------------------------------------------

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().chain(c).collect(),
        None => String::new(),
    }
}

/// What a class is to state management, from what it extends or mixes in.
fn state_kind(c: &Class) -> Option<&'static str> {
    let base = c.base.unwrap_or("");
    if base == "Bloc" {
        return Some("bloc");
    }
    if base == "Cubit" || base == "HydratedCubit" {
        return Some("cubit");
    }
    if base == "HydratedBloc" {
        return Some("bloc");
    }
    if matches!(
        base,
        "ChangeNotifier"
            | "ValueNotifier"
            | "StateNotifier"
            | "Notifier"
            | "AsyncNotifier"
            | "StreamNotifier"
            | "FamilyNotifier"
            | "AutoDisposeNotifier"
            | "AutoDisposeAsyncNotifier"
    ) || base.starts_with("_$")
        || c.mixins.contains(&"ChangeNotifier")
    {
        return Some("notifier");
    }
    None
}

/// A provider and the tokens it is declared over, its body included.
struct Declared {
    name: String,
    rel: String,
    line: usize,
    written: String,
    file: usize,
    span: (usize, usize),
    /// The class a `@riverpod class` generates it for.
    class: Option<String>,
}

/// The providers a file declares: `final x = SomeProvider…(…);` at the top,
/// and `@riverpod` functions and classes.
fn declared(files: &[File]) -> Vec<Declared> {
    let mut out = Vec::new();
    for (n, f) in files.iter().enumerate() {
        let toks = &f.toks;
        for (i, t) in toks.iter().enumerate() {
            if t.text == "=" && i >= 1 && f.depth[i] == 0 && toks[i - 1].kind == Kind::Word {
                let Some(ctor) = toks.get(i + 1) else {
                    continue;
                };
                if ctor.kind != Kind::Word || !ctor.text.ends_with("Provider") {
                    continue;
                }
                let end = f.expr_end(i);
                let written: String = toks[i + 1..end]
                    .iter()
                    .take_while(|x| x.text != "(")
                    .map(|x| x.text)
                    .collect();
                out.push(Declared {
                    name: toks[i - 1].text.to_string(),
                    rel: f.source.rel.clone(),
                    line: f.line(i - 1),
                    written,
                    file: n,
                    span: (i - 1, end),
                    class: None,
                });
            }
            if t.text == "@"
                && f.depth[i] == 0
                && toks
                    .get(i + 1)
                    .is_some_and(|r| matches!(r.text, "riverpod" | "Riverpod"))
            {
                let mut j = i + 2;
                if toks.get(j).is_some_and(|p| p.text == "(") {
                    j = f.pairs[j].map_or(j + 1, |c| c + 1);
                }
                if toks.get(j).is_some_and(|c| c.text == "class") {
                    let Some(class) = f
                        .classes
                        .iter()
                        .find(|c| c.open > j && toks[j + 1].text == c.name)
                    else {
                        continue;
                    };
                    out.push(Declared {
                        name: format!("{}Provider", lower_first(class.name)),
                        rel: f.source.rel.clone(),
                        line: f.line(i),
                        written: "@riverpod class".into(),
                        file: n,
                        span: (class.open, class.close),
                        class: Some(class.name.to_string()),
                    });
                    continue;
                }
                // A function: its name is the word before its parameters.
                let Some(params) =
                    (j..toks.len().min(j + 40)).find(|k| toks[*k].text == "(" && f.depth[*k] == 0)
                else {
                    continue;
                };
                let Some(close) = f.pairs[params] else {
                    continue;
                };
                let mut b = close + 1;
                while toks
                    .get(b)
                    .is_some_and(|x| matches!(x.text, "async" | "*" | "sync"))
                {
                    b += 1;
                }
                let end = match toks.get(b).map(|x| x.text) {
                    Some("{") => f.pairs[b].unwrap_or(toks.len()),
                    _ => f.expr_end(b),
                };
                out.push(Declared {
                    name: format!("{}Provider", lower_first(toks[params - 1].text)),
                    rel: f.source.rel.clone(),
                    line: f.line(i),
                    written: "@riverpod function".into(),
                    file: n,
                    span: (i, end),
                    class: None,
                });
            }
        }
    }
    out
}

fn state(files: &[File]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "dart.state",
        "State management",
        "where state lives and who reads it: Riverpod providers and who watches, reads or listens to them; provider-package notifiers, who provides and who reads them; Blocs and Cubits, the events each handles, who adds them and who builds on them",
    );
    // The state classes and Bloc events by name.
    let mut holders: HashMap<String, (String, usize, &'static str, String)> = HashMap::new();
    let mut events: HashSet<String> = HashSet::new();
    let mut classes_at: HashMap<String, (String, usize)> = HashMap::new();
    for f in files {
        for c in &f.classes {
            classes_at.insert(
                c.name.to_string(),
                (f.source.rel.clone(), line_at(&f.source.text, c.at)),
            );
            if let Some(kind) = state_kind(c) {
                let written = if c.mixins.contains(&"ChangeNotifier") && c.base.is_none() {
                    "with ChangeNotifier".to_string()
                } else {
                    c.base_written.clone()
                };
                holders.insert(
                    c.name.to_string(),
                    (
                        f.source.rel.clone(),
                        line_at(&f.source.text, c.at),
                        kind,
                        written,
                    ),
                );
            }
        }
    }
    let providers = declared(files);
    let by_name: HashMap<&str, &Declared> =
        providers.iter().map(|p| (p.name.as_str(), p)).collect();
    let holder_node = |map: &mut MapBuilder, name: &str| -> Option<usize> {
        let (r, l, kind, written) = holders.get(name)?;
        let n = map.node(kind, name, name);
        map.place(n, r, *l);
        map.detail(n, written.clone());
        n
    };
    let provider_node = |map: &mut MapBuilder, name: &str| -> Option<usize> {
        let p = by_name.get(name)?;
        let n = map.node("provider", name, name);
        map.place(n, &p.rel, p.line);
        map.detail(n, p.written.clone());
        n
    };
    for name in holders.keys() {
        holder_node(&mut map, name);
    }
    // What each provider creates: a `@riverpod class`, or the state class
    // its body constructs.
    for p in &providers {
        let made: Vec<String> = match &p.class {
            Some(c) => vec![c.clone()],
            None => (p.span.0..p.span.1)
                .filter_map(|k| constructor(&files[p.file].toks, k).map(|(n, _)| n))
                .filter(|n| holders.contains_key(n))
                .collect(),
        };
        for m in made {
            let from = provider_node(&mut map, &p.name);
            let to = holder_node(&mut map, &m);
            map.edge(from, to, "creates");
        }
    }
    // `on<Increment>(…)` in a Bloc: the events it handles.
    for f in files {
        for (i, t) in f.toks.iter().enumerate() {
            if t.text == "on"
                && f.toks.get(i + 1).is_some_and(|l| l.text == "<")
                && let Some(c) = f.class_at(i).filter(|c| state_kind(c) == Some("bloc"))
                && let Some(event) = f.toks.get(i + 2).filter(|e| e.kind == Kind::Word)
            {
                events.insert(event.text.to_string());
                let b = holder_node(&mut map, c.name);
                let e = map.node("event", event.text, event.text);
                if let Some((r, l)) = classes_at.get(event.text) {
                    map.place(e, r, *l);
                }
                map.edge(e, b, "on");
            }
        }
    }
    let mut seen: HashSet<(usize, usize, String)> = HashSet::new();
    for (n, f) in files.iter().enumerate() {
        let toks = &f.toks;
        // Who is reading at token `i`: the provider declared around it, else
        // the widget whose class it is in (a State's widget), else the state
        // class itself.
        let consumer = |map: &mut MapBuilder, i: usize| -> Option<usize> {
            if let Some(p) = providers
                .iter()
                .find(|p| p.file == n && p.span.0 <= i && i <= p.span.1)
            {
                return provider_node(map, &p.name);
            }
            let c = f.class_at(i)?;
            let name = widget_of(c);
            if holders.contains_key(name) {
                return holder_node(map, name);
            }
            let w = map.node("widget", name, name);
            if let Some((r, l)) = classes_at.get(name) {
                map.place(w, r, *l);
            }
            w
        };
        let mut link = |map: &mut MapBuilder, i: usize, to: Option<usize>, how: &str| {
            let Some(to) = to else {
                return;
            };
            let from = consumer(map, i);
            if let Some(a) = from
                && a != to
                && seen.insert((a, to, how.to_string()))
            {
                map.edge(from, Some(to), how);
            }
        };
        for (i, t) in toks.iter().enumerate() {
            let next = |k: usize| toks.get(i + k).map(|x| x.text).unwrap_or("");
            match t.text {
                // `ref.watch(cartProvider)`, `ref.read(cartProvider.notifier)`.
                "ref"
                    if next(1) == "."
                        && matches!(
                            next(2),
                            "watch" | "read" | "listen" | "refresh" | "invalidate"
                        )
                        && next(3) == "(" =>
                {
                    let to = provider_node(&mut map, next(4));
                    link(&mut map, i, to, next(2));
                }
                // `context.watch<Cart>()`, `context.read<CounterBloc>()`.
                "context"
                    if next(1) == "."
                        && matches!(next(2), "watch" | "read" | "select")
                        && next(3) == "<" =>
                {
                    let to = holder_node(&mut map, next(4));
                    link(&mut map, i, to, next(2));
                }
                // `Provider.of<Cart>(context)`.
                "Provider" if next(1) == "." && next(2) == "of" && next(3) == "<" => {
                    let to = holder_node(&mut map, next(4));
                    link(&mut map, i, to, "Provider.of");
                }
                "Consumer" | "Selector" | "BlocBuilder" | "BlocListener" | "BlocConsumer"
                | "BlocSelector"
                    if next(1) == "<" =>
                {
                    let to = holder_node(&mut map, next(2));
                    link(&mut map, i, to, t.text);
                }
                // `ChangeNotifierProvider(create: (_) => Cart())`.
                "create" if next(1) == ":" && f.class_at(i).is_some() => {
                    let end = f.expr_end(i);
                    let made = (i + 2..end).find_map(|k| constructor(toks, k).map(|(n, _)| n));
                    if let Some(made) = made {
                        let to = holder_node(&mut map, made.split('.').next().unwrap_or(&made));
                        link(&mut map, i, to, "provides");
                    }
                }
                // `….add(Increment())`: an event to a Bloc.
                "add" if i > 0 && toks[i - 1].text == "." && next(1) == "(" => {
                    if let Some((event, _)) = constructor(toks, i + 2)
                        && events.contains(&event)
                    {
                        let e = map.node("event", &event, &event);
                        link(&mut map, i, e, "add");
                    }
                }
                _ => {}
            }
        }
    }
    map.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(map: &ProjectMap) -> Vec<(String, String, String)> {
        let mut out: Vec<_> = map
            .edges
            .iter()
            .map(|e| {
                (
                    map.nodes[e.from].label.clone(),
                    map.nodes[e.to].label.clone(),
                    e.label.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn e(a: &str, b: &str, l: &str) -> (String, String, String) {
        (a.into(), b.into(), l.into())
    }

    #[test]
    fn tokens_skip_strings_and_comments() {
        let toks =
            tokens("a /* b /* c */ d */ 'x${y + \"}\"}z' // e\nr'\\n' \"\"\"q\nw\"\"\" f => g");
        let shown: Vec<&str> = toks.iter().map(|t| t.text).collect();
        assert_eq!(
            shown,
            [
                "a",
                "'x${y + \"}\"}z'",
                "r'\\n'",
                "\"\"\"q\nw\"\"\"",
                "f",
                "=>",
                "g"
            ]
        );
    }

    const APP: &[(&str, &str)] = &[
        (
            "lib/main.dart",
            "import 'package:flutter/material.dart';\n\nvoid main() {\n  runApp(const ProviderScope(child: ShopApp()));\n}\n\nclass ShopApp extends StatelessWidget {\n  const ShopApp({super.key});\n  @override\n  Widget build(BuildContext context) {\n    return MaterialApp(\n      title: 'Shop',\n      theme: ThemeData(useMaterial3: true),\n      home: const CartPage(),\n    );\n  }\n}\n",
        ),
        (
            "lib/cart_page.dart",
            "class CartPage extends ConsumerStatefulWidget {\n  const CartPage({super.key});\n  @override\n  ConsumerState<CartPage> createState() => _CartPageState();\n}\n\nclass _CartPageState extends ConsumerState<CartPage> {\n  @override\n  Widget build(BuildContext context) {\n    final items = ref.watch(cartProvider);\n    return Scaffold(\n      appBar: AppBar(title: Text('Cart (${items.length})')),\n      body: Padding(\n        padding: const EdgeInsets.all(8),\n        child: Column(children: [\n          const _Header(),\n          if (items.isEmpty) const Text('empty'),\n          ...items.map((i) => ItemTile(item: i)),\n        ]),\n      ),\n      floatingActionButton: FloatingActionButton(\n        onPressed: () => ref.read(cartProvider.notifier).clear(),\n        child: const Icon(Icons.delete),\n      ),\n    );\n  }\n}\n\nclass _Header extends StatelessWidget {\n  const _Header();\n  @override\n  Widget build(BuildContext context) => const ListTile(title: Text('Items'));\n}\n",
        ),
        (
            "lib/item_tile.dart",
            "class ItemTile extends StatelessWidget {\n  const ItemTile({super.key, required this.item});\n  final Item item;\n  @override\n  Widget build(BuildContext context) {\n    final count = context.watch<Counter>().value;\n    return ListTile(title: Text(item.name), trailing: Text('$count'), style: ListTileStyle.list);\n  }\n}\n",
        ),
        (
            "lib/state.dart",
            "final cartProvider = StateNotifierProvider<CartNotifier, List<Item>>((ref) {\n  final api = ref.watch(apiProvider);\n  return CartNotifier(api);\n});\nfinal apiProvider = Provider<Api>((ref) => Api());\n\nclass CartNotifier extends StateNotifier<List<Item>> {\n  CartNotifier(this.api) : super([]);\n  final Api api;\n  void clear() => state = [];\n}\n\nclass Counter with ChangeNotifier {\n  int value = 0;\n}\n\n@riverpod\nFuture<int> total(TotalRef ref) async {\n  final items = ref.watch(cartProvider);\n  return items.length;\n}\n",
        ),
        (
            "lib/counter_bloc.dart",
            "sealed class CounterEvent {}\nfinal class Increment extends CounterEvent {}\nfinal class Reset extends CounterEvent {}\n\nclass CounterBloc extends Bloc<CounterEvent, int> {\n  CounterBloc() : super(0) {\n    on<Increment>((event, emit) => emit(state + 1));\n    on<Reset>((event, emit) => emit(0));\n  }\n}\n\nclass CounterView extends StatelessWidget {\n  @override\n  Widget build(BuildContext context) {\n    return BlocProvider(\n      create: (_) => CounterBloc(),\n      child: BlocBuilder<CounterBloc, int>(\n        builder: (context, n) => TextButton(\n          onPressed: () => context.read<CounterBloc>().add(Increment()),\n          child: Text('$n'),\n        ),\n      ),\n    );\n  }\n}\n\nclass Shell extends StatelessWidget {\n  @override\n  Widget build(BuildContext context) => ChangeNotifierProvider(create: (_) => Counter(), child: const CounterView());\n}\n",
        ),
    ];

    #[test]
    fn the_widget_tree_nests_as_written() {
        let maps = maps(&Corpus::of(APP));
        let tree = maps.iter().find(|m| m.id == "flutter.widgets").unwrap();
        let got = edges(tree);
        for want in [
            e("runApp", "ProviderScope", "builds"),
            e("ProviderScope", "ShopApp", "child"),
            e("ShopApp", "MaterialApp", "builds"),
            e("MaterialApp", "CartPage", "home"),
            e("CartPage", "Scaffold", "builds"),
            e("Scaffold", "AppBar", "appBar"),
            e("AppBar", "Text", "title"),
            e("Scaffold", "Padding", "body"),
            e("Padding", "Column", "child"),
            e("Column", "_Header", "children"),
            e("Column", "Text", "children"),
            e("Column", "ItemTile", "builds"),
            e("Scaffold", "FloatingActionButton", "floatingActionButton"),
            e("FloatingActionButton", "Icon", "child"),
            e("_Header", "ListTile", "builds"),
            e("ItemTile", "ListTile", "builds"),
            e("ListTile", "Text", "trailing"),
            e("CounterView", "BlocProvider", "builds"),
            e("BlocProvider", "BlocBuilder", "child"),
            e("BlocBuilder", "TextButton", "builds"),
            e("Shell", "ChangeNotifierProvider", "builds"),
            e("ChangeNotifierProvider", "CounterView", "child"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        for not in [
            "ThemeData",
            "EdgeInsets.all",
            "ListTileStyle",
            "CounterBloc",
            "Counter",
            "Item",
            "_CartPageState",
        ] {
            assert!(
                !tree.nodes.iter().any(|n| n.label == not),
                "{not} is not a widget: {got:?}"
            );
        }
        let node = |l: &str| tree.nodes.iter().find(|n| n.label == l).unwrap();
        assert_eq!(
            (node("CartPage").file.as_str(), node("CartPage").line),
            ("lib/cart_page.dart", 1)
        );
        assert_eq!(
            node("CartPage").detail,
            "ConsumerStatefulWidget · state _CartPageState"
        );
        assert_eq!(node("Scaffold").line, 11);
        assert_eq!(node("runApp").line, 4);
    }

    #[test]
    fn state_and_who_reads_it() {
        let maps = maps(&Corpus::of(APP));
        let st = maps.iter().find(|m| m.id == "dart.state").unwrap();
        let got = edges(st);
        for want in [
            e("CartPage", "cartProvider", "watch"),
            e("CartPage", "cartProvider", "read"),
            e("cartProvider", "apiProvider", "watch"),
            e("cartProvider", "CartNotifier", "creates"),
            e("totalProvider", "cartProvider", "watch"),
            e("ItemTile", "Counter", "watch"),
            e("Shell", "Counter", "provides"),
            e("CounterView", "CounterBloc", "provides"),
            e("CounterView", "CounterBloc", "BlocBuilder"),
            e("CounterView", "CounterBloc", "read"),
            e("CounterView", "Increment", "add"),
            e("Increment", "CounterBloc", "on"),
            e("Reset", "CounterBloc", "on"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        let node = |l: &str| st.nodes.iter().find(|n| n.label == l).unwrap();
        assert_eq!(
            node("cartProvider").detail,
            "StateNotifierProvider<CartNotifier,List<Item>>"
        );
        assert_eq!(
            (
                node("cartProvider").file.as_str(),
                node("cartProvider").line
            ),
            ("lib/state.dart", 1)
        );
        assert_eq!(node("totalProvider").detail, "@riverpod function");
        assert_eq!(node("CounterBloc").detail, "Bloc<CounterEvent, int>");
        assert_eq!(node("CounterBloc").kind, "bloc");
        assert_eq!(node("Counter").detail, "with ChangeNotifier");
        assert_eq!(node("CartNotifier").detail, "StateNotifier<List<Item>>");
        assert_eq!(
            (node("Increment").file.as_str(), node("Increment").line),
            ("lib/counter_bloc.dart", 2)
        );
    }
}
