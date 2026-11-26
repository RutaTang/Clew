//! What a type's declaration says it extends, implements or derives,
//! read from its signature as each language writes it — for the type graph
//! (the client's) and the project graphs (built where the files are).

/// The names a type's declaration says it extends, implements or is based
/// on: after `extends`, `implements` and `with` (Java, TypeScript, Dart,
/// PHP, Scala); a Python class's bases; a Rust trait's supertraits; a C++
/// class's bases after `:`, and a Kotlin or C# type's after the `:` past its
/// constructor's parameters (`class Circle(val r: Double) : Shape(),
/// Drawable`, `record Point(int X) : IShape where …`); a Ruby class's after
/// `<`; a Haskell class's superclasses (`class Eq a => Ord a`) and the
/// classes a type derives. Generic arguments are stripped first, so a bound
/// inside `<…>` is not read as a base; each name's last path segment is what
/// comes back.
pub fn inherited_names(signature: &str, lang: Option<&str>) -> Vec<String> {
    let flat = without_angle_brackets(signature);
    let flat = flat.split('{').next().unwrap_or("").trim();
    let mut names = Vec::new();
    let mut push = |part: &str| {
        let part = part.trim();
        if part.is_empty() || part.contains('=') {
            return;
        }
        let word = part
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == ':'))
            .find(|w| !w.is_empty() && !is_access_word(w));
        if let Some(w) = word {
            let last = w.rsplit(['.', ':']).next().unwrap_or(w);
            if !last.is_empty() && !names.iter().any(|n| n == last) {
                names.push(last.to_string());
            }
        }
    };
    for keyword in ["extends", "implements", "with"] {
        let mut rest = flat;
        while let Some(at) = find_word(rest, keyword) {
            let after = &rest[at + keyword.len()..];
            let end = ["extends", "implements", "with", "where", "permits"]
                .iter()
                .filter_map(|k| find_word(after, k))
                .min()
                .unwrap_or(after.len());
            for part in after[..end].split(',') {
                push(part);
            }
            rest = &after[end..];
        }
    }
    match lang {
        Some("python") => {
            if let Some(open) = flat.find('(')
                && let Some(close) = flat[open..].find(')')
            {
                for part in flat[open + 1..open + close].split(',') {
                    push(part);
                }
            }
        }
        Some("rust") => {
            // Visibility restrictions and `unsafe` precede the trait, and
            // `pub(in crate::m)` itself contains colons. Its syntax field
            // identifies the supertraits without borrowing a generic bound
            // or a predicate from the `where` clause.
            if let Some(bounds) = rust_supertraits(signature) {
                for part in bounds.split('+') {
                    push(part);
                }
            }
        }
        Some("cpp") | Some("c") => {
            if let Some(colon) = flat.find(':')
                && !flat[colon..].starts_with("::")
            {
                for part in flat[colon + 1..].split(',') {
                    push(part);
                }
            }
        }
        Some("kotlin") | Some("csharp") => {
            if let Some(bases) = colon_bases(flat) {
                // `Drawable by b` delegates to `b`; `Shape(1, 2)` passes
                // arguments, whose commas part nothing.
                for part in top_level_parts(bases) {
                    push(part);
                }
            }
        }
        // Ruby's `class Circle < Shape` (never `class << self`); its `<` is
        // no generic bracket, so read before any are stripped.
        Some("ruby") => {
            if let Some(at) = find_word(signature, "class") {
                let rest = signature[at + "class".len()..].trim_start();
                let rest = rest.trim_start_matches(|c: char| !c.is_whitespace() && c != '<');
                if let Some(base) = rest.trim_start().strip_prefix('<')
                    && !base.starts_with('<')
                {
                    push(base.split_whitespace().next().unwrap_or(""));
                }
            }
        }
        Some("haskell") => {
            for name in haskell_bases(signature) {
                push(&name);
            }
        }
        // Go's embedded types: a struct member or an interface element that
        // is a type alone (`User`, `*Logger`, `io.Closer`, a tag aside) —
        // the struct has their methods, the interface their method sets.
        Some("go") => {
            if let (Some(open), Some(close)) = (signature.find('{'), signature.rfind('}')) {
                for member in signature[open + 1..close.max(open + 1)].split(';') {
                    let member = member.split('`').next().unwrap_or("").trim();
                    let words: Vec<&str> = member.split_whitespace().collect();
                    if let [one] = words.as_slice()
                        && !one.contains(['(', '|', '~', '['])
                    {
                        push(one.trim_start_matches('*'));
                    }
                }
            }
        }
        // A Dart mixin's `on` types: what it may be mixed into, and whose
        // members it uses.
        Some("dart") if find_word(signature, "mixin").is_some() => {
            if let Some(at) = find_word(flat, "on") {
                let after = &flat[at + 2..];
                let end = find_word(after, "implements").unwrap_or(after.len());
                for part in after[..end].split(',') {
                    push(part);
                }
            }
        }
        _ => {}
    }
    names
}

/// A Kotlin or C# declaration's bases: what follows its first `:` outside
/// parentheses (a constructor parameter's `val r: Double` is inside them),
/// up to a `where` clause.
fn colon_bases(flat: &str) -> Option<&str> {
    let mut depth = 0i32;
    let colon = flat.char_indices().find_map(|(i, c)| {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ':' if depth == 0 => return Some(i),
            _ => {}
        }
        None
    })?;
    let bases = &flat[colon + 1..];
    Some(match find_word(bases, "where") {
        Some(end) => &bases[..end],
        None => bases,
    })
}

/// `text` split at its commas outside parentheses.
fn top_level_parts(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// A Haskell class's superclasses — its context, before `=>` (`class (Eq
/// a, Show a) => Ord a`) — and the classes a data type or newtype derives
/// (`deriving (Show, Eq)`, `deriving stock Show`, `deriving anyclass (ToJSON)`).
/// A class's methods, after its `where`, say nothing of what it is.
fn haskell_bases(signature: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut add = |part: &str| {
        let part = part.trim().trim_start_matches('(');
        if let Some(word) = part.split_whitespace().next()
            && word.starts_with(|c: char| c.is_uppercase())
        {
            let word = word.trim_end_matches(')');
            out.push(word.to_string());
        }
    };
    let head = match find_word(signature, "where") {
        Some(end) => &signature[..end],
        None => signature,
    };
    if find_word(head, "class").is_some()
        && let Some(arrow) = head.find("=>")
    {
        let context = head[..arrow].trim();
        let context = context
            .strip_prefix("class")
            .map_or(context, str::trim_start);
        let context = context
            .strip_prefix('(')
            .and_then(|c| c.strip_suffix(')'))
            .unwrap_or(context);
        for part in context.split(',') {
            add(part);
        }
    }
    let mut rest = signature;
    while let Some(at) = find_word(rest, "deriving") {
        let after = rest[at + "deriving".len()..].trim_start();
        let after = ["stock", "anyclass", "newtype"]
            .iter()
            .find_map(|s| after.strip_prefix(s).filter(|a| a.starts_with([' ', '('])))
            .map_or(after, str::trim_start);
        match after.strip_prefix('(') {
            Some(list) => {
                for part in list.split(')').next().unwrap_or("").split(',') {
                    add(part);
                }
            }
            None => add(after),
        }
        rest = &rest[at + "deriving".len()..];
    }
    out
}

fn rust_supertraits(signature: &str) -> Option<String> {
    find_word(signature, "trait")?;
    let signature = signature.trim_end();
    let source = if signature.ends_with('{') {
        format!("{signature}}}")
    } else if !signature.contains('{') {
        format!("{signature} {{}}")
    } else {
        signature.to_string()
    };
    let tree = crate::highlight::parse(&source, crate::highlight::Lang::Rust)?;
    let mut cursor = tree.root_node().walk();
    let item = tree
        .root_node()
        .named_children(&mut cursor)
        .find(|n| n.kind() == "trait_item")?;
    let bounds = item.child_by_field_name("bounds")?;
    let text = source.get(bounds.byte_range())?.trim_start_matches(':');
    Some(without_angle_brackets(text))
}

fn is_access_word(w: &str) -> bool {
    matches!(
        w,
        "public" | "private" | "protected" | "virtual" | "final" | "abstract"
    )
}

/// `text` with every balanced `<…>` removed.
fn without_angle_brackets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Where `word` occurs in `text` as a whole word.
fn find_word(text: &str, word: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = text[from..].find(word) {
        let at = from + at;
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        let bounded = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        if bounded(before) && bounded(after) {
            return Some(at);
        }
        from = at + word.len();
    }
    None
}
