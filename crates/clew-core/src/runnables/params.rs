//! The cases of a parametrized pytest test (`@pytest.mark.parametrize`),
//! each by the id pytest gives it (`test_add[1-2]`), so that one case can
//! be run alone.
//!
//! An id is only given where it is known exactly from the source: from
//! literals pytest names by their value (numbers, plain strings, `True`,
//! `None`), from a container it names by the argument and the case's
//! index (`pair0`), from `pytest.param(…, id=…)` and from a literal
//! `ids=[…]`. Anything else — a name that may be an enum or a class, a
//! call, an escaped or non-ASCII string — has no id here, and its case is
//! run with the rest of the test.

use tree_sitter::Node;

/// One case: its pytest id, and the 1-based lines it is written on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Case {
    pub id: String,
    pub span: (usize, usize),
}

/// The cases of the test `decorated` (its `decorated_definition`) whose
/// ids are known and that have their lines to themselves: a line holding
/// two cases runs the whole test, not one of them. Empty for a test with
/// no `parametrize`, or more than one.
pub(super) fn cases(decorated: Node, source: &str) -> Vec<Case> {
    let text = |node: Node| source.get(node.byte_range()).unwrap_or("");
    let mut cursor = decorated.walk();
    let decorators: Vec<Node> = decorated
        .children(&mut cursor)
        .filter(|c| c.kind() == "decorator")
        .collect();
    let mut calls = Vec::new();
    for decorator in decorators {
        let mentions = text(decorator).contains("parametrize");
        let call = decorator
            .named_child(0)
            .filter(|c| c.kind() == "call")
            .filter(|call| is_parametrize(call, source));
        match (mentions, call) {
            (_, Some(call)) => calls.push(call),
            // Some other way of parametrizing: its ids are not known.
            (true, None) => return Vec::new(),
            (false, None) => {}
        }
    }
    let [call] = calls[..] else {
        return Vec::new();
    };
    cases_of(call, source)
}

/// Whether `call` is `pytest.mark.parametrize(…)` or `mark.parametrize(…)`.
fn is_parametrize(call: &Node, source: &str) -> bool {
    call.child_by_field_name("function")
        .and_then(|f| source.get(f.byte_range()))
        .map(|f| f.split_whitespace().collect::<String>())
        .is_some_and(|f| f == "pytest.mark.parametrize" || f == "mark.parametrize")
}

/// The cases a `parametrize(argnames, argvalues, ids=…)` call makes.
fn cases_of(call: Node, source: &str) -> Vec<Case> {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let (positional, keywords) = split_arguments(arguments, source);
    let keyword = |name: &str| keywords.iter().find(|(n, _)| n == name).map(|(_, v)| *v);
    let (Some(names), Some(values)) = (
        positional.first().copied().or_else(|| keyword("argnames")),
        positional.get(1).copied().or_else(|| keyword("argvalues")),
    ) else {
        return Vec::new();
    };
    let Some((names, alone)) = arg_names(names, source) else {
        return Vec::new();
    };
    // `ids=[…]` of literal strings (or `None`, for pytest's own); a
    // function or anything else names the cases some other way.
    let ids: Vec<Option<String>> = match keyword("ids") {
        None => Vec::new(),
        Some(list) if matches!(list.kind(), "list" | "tuple") => {
            let mut ids = Vec::new();
            for item in elements(list) {
                ids.push(match item.kind() {
                    "none" => None,
                    _ => match string_value(item, source) {
                        Some(id) => Some(id),
                        None => return Vec::new(),
                    },
                });
            }
            ids
        }
        Some(_) => return Vec::new(),
    };
    if !matches!(values.kind(), "list" | "tuple") {
        return Vec::new();
    }
    let items = elements(values);
    if items.iter().any(|i| i.kind() == "list_splat") {
        return Vec::new();
    }
    let mut found: Vec<(Option<String>, (usize, usize))> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let span = (item.start_position().row + 1, item.end_position().row + 1);
        let given = ids.get(index).cloned().flatten();
        found.push((case_id(*item, &names, alone, index, given, source), span));
    }
    // A line one case shares with another runs the whole test: the case
    // the cursor is on cannot be told from its neighbour's.
    let alone = |span: (usize, usize), at: usize| {
        found
            .iter()
            .enumerate()
            .all(|(other, (_, s))| other == at || s.1 < span.0 || span.1 < s.0)
    };
    let known: Vec<&String> = found.iter().filter_map(|(id, _)| id.as_ref()).collect();
    let unique = |id: &String| known.iter().filter(|k| **k == id).count() == 1;
    found
        .iter()
        .enumerate()
        .filter_map(|(at, (id, span))| {
            let id = id.as_ref()?;
            (unique(id) && alone(*span, at)).then(|| Case {
                id: id.clone(),
                span: *span,
            })
        })
        .collect()
}

/// A call's positional arguments and its keyword ones by name.
fn split_arguments<'t>(
    arguments: Node<'t>,
    source: &str,
) -> (Vec<Node<'t>>, Vec<(String, Node<'t>)>) {
    let mut positional = Vec::new();
    let mut keywords = Vec::new();
    for argument in elements(arguments) {
        if argument.kind() != "keyword_argument" {
            positional.push(argument);
        } else if let (Some(name), Some(value)) = (
            argument.child_by_field_name("name"),
            argument.child_by_field_name("value"),
        ) {
            let name = source.get(name.byte_range()).unwrap_or("").to_string();
            keywords.push((name, value));
        }
    }
    (positional, keywords)
}

/// A node's elements: its named children that are not comments.
fn elements(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| c.kind() != "comment")
        .collect()
}

/// `parametrize`'s argument names: `"a, b"` or a list of strings; and
/// whether each case is its one value as it is (one name, in a string),
/// not a tuple of values (a list of names, one or more).
fn arg_names(node: Node, source: &str) -> Option<(Vec<String>, bool)> {
    let names: Vec<String> = match node.kind() {
        "string" => string_value(node, source)?
            .split(',')
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect(),
        "list" | "tuple" => elements(node)
            .into_iter()
            .map(|n| string_value(n, source).map(|n| n.trim().to_string()))
            .collect::<Option<_>>()?,
        _ => return None,
    };
    let alone = node.kind() == "string" && names.len() == 1;
    (!names.is_empty()).then_some((names, alone))
}

/// The id pytest gives the case `item`, the `index`th, of a test taking
/// `names` (`alone`: the case is its one value, not a tuple of them): its
/// `pytest.param(…, id=…)`, else `given` (from `ids=`), else its values'
/// ids joined by `-`. `None` where it is not known.
fn case_id(
    item: Node,
    names: &[String],
    alone: bool,
    index: usize,
    given: Option<String>,
    source: &str,
) -> Option<String> {
    let (values, id) = match item.kind() {
        "call" if is_param(&item, source) => {
            let arguments = item.child_by_field_name("arguments")?;
            let (values, keywords) = split_arguments(arguments, source);
            let id = match keywords.iter().find(|(n, _)| n == "id") {
                Some((_, value)) if value.kind() == "none" => None,
                Some((_, value)) => Some(string_value(*value, source)?),
                None => None,
            };
            (values, id)
        }
        _ if alone => (vec![item], None),
        "tuple" | "list" => (elements(item), None),
        _ => return None,
    };
    if values.len() != names.len() {
        return None;
    }
    let id = match id.or(given) {
        Some(id) => id,
        None => names
            .iter()
            .zip(&values)
            .map(|(name, value)| value_id(*value, name, index, source))
            .collect::<Option<Vec<_>>>()?
            .join("-"),
    };
    // A node id is split at `::`; an empty one names nothing.
    (!id.is_empty() && !id.contains("::")).then_some(id)
}

/// Whether `call` is `pytest.param(…)` or `param(…)`.
fn is_param(call: &Node, source: &str) -> bool {
    call.child_by_field_name("function")
        .and_then(|f| source.get(f.byte_range()))
        .map(|f| f.split_whitespace().collect::<String>())
        .is_some_and(|f| f == "pytest.param" || f == "param")
}

/// The id pytest gives one value: a literal's by its value (`str(value)`),
/// a container's by its argument's name and the case's index (`pair0`).
/// `None` where it cannot be known from the source.
fn value_id(value: Node, name: &str, index: usize, source: &str) -> Option<String> {
    match value.kind() {
        "tuple"
        | "list"
        | "dictionary"
        | "set"
        | "list_comprehension"
        | "dictionary_comprehension"
        | "set_comprehension" => Some(format!("{name}{index}")),
        "parenthesized_expression" => value_id(value.named_child(0)?, name, index, source),
        _ => literal_id(value, source),
    }
}

/// `str(value)` of a literal, as Python writes it: `-3`, `0.5`, `1e-05`,
/// `True`, `None`, a plain ASCII string as it is.
fn literal_id(value: Node, source: &str) -> Option<String> {
    let text = source.get(value.byte_range())?;
    match value.kind() {
        "integer" => integer(text).map(|n| n.to_string()),
        "float" => python_float(float(text)?),
        "true" => Some("True".into()),
        "false" => Some("False".into()),
        "none" => Some("None".into()),
        "string" => string_value(value, source),
        "unary_operator" => {
            let operator = value.child_by_field_name("operator")?;
            let argument = value.child_by_field_name("argument")?;
            let argument_text = source.get(argument.byte_range())?;
            let negate = match source.get(operator.byte_range())? {
                "-" => true,
                "+" => false,
                _ => return None,
            };
            match argument.kind() {
                "integer" => {
                    let n = integer(argument_text)?;
                    Some(if negate { n.checked_neg()? } else { n }.to_string())
                }
                "float" => {
                    let x = float(argument_text)?;
                    python_float(if negate { -x } else { x })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// An integer literal's value: decimal, `0x`, `0o` or `0b`, with `_`s.
fn integer(text: &str) -> Option<i128> {
    let digits: String = text.chars().filter(|c| *c != '_').collect();
    let lower = digits.to_ascii_lowercase();
    let (radix, digits) = match lower.get(..2) {
        Some("0x") => (16, &lower[2..]),
        Some("0o") => (8, &lower[2..]),
        Some("0b") => (2, &lower[2..]),
        _ => (10, lower.as_str()),
    };
    // A complex literal (`2j`) is no integer.
    i128::from_str_radix(digits, radix).ok()
}

/// A float literal's value; `None` for a complex one (`1.5j`).
fn float(text: &str) -> Option<f64> {
    let digits: String = text.chars().filter(|c| *c != '_').collect();
    if digits.ends_with(['j', 'J']) {
        return None;
    }
    digits.parse().ok()
}

/// `str(x)` as Python writes a float: the shortest digits that read back
/// as `x`, in positional notation from `1e-4` up to `1e16` (`0.0001`,
/// `100.0`), in scientific beyond it (`1e-05`, `1.5e+16`).
fn python_float(x: f64) -> Option<String> {
    if !x.is_finite() {
        return None;
    }
    // Rust's `{:e}` is the shortest round-trip digits too: `1.5e-7`.
    let scientific = format!("{x:e}");
    let (mantissa, exponent) = scientific.split_once('e')?;
    let exponent: i32 = exponent.parse().ok()?;
    let sign = if mantissa.starts_with('-') { "-" } else { "" };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let written = if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        if point <= 0 {
            format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize))
        } else {
            let point = point as usize;
            if point >= digits.len() {
                format!("{digits}{}.0", "0".repeat(point - digits.len()))
            } else {
                format!("{}.{}", &digits[..point], &digits[point..])
            }
        }
    } else {
        let mantissa = match digits.len() {
            1 => digits.clone(),
            _ => format!("{}.{}", &digits[..1], &digits[1..]),
        };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{exponent_sign}{:02}", exponent.unsigned_abs())
    };
    Some(format!("{sign}{written}"))
}

/// A plain string literal's text, when pytest writes it as it is: no
/// bytes or f-string prefix, nothing escaped, printable ASCII only — any
/// other pytest escapes in its id — and not empty.
fn string_value(node: Node, source: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let text = source.get(node.byte_range())?;
    let quote = text.find(['\'', '"'])?;
    let (prefix, body) = text.split_at(quote);
    if !prefix.chars().all(|c| matches!(c, 'r' | 'R' | 'u' | 'U')) {
        return None;
    }
    let delimiter = if body.starts_with("\"\"\"") || body.starts_with("'''") {
        &body[..3]
    } else {
        &body[..1]
    };
    let inner = body.strip_prefix(delimiter)?.strip_suffix(delimiter)?;
    let plain = !inner.is_empty()
        && !inner.contains('\\')
        && inner.chars().all(|c| (' '..='~').contains(&c));
    plain.then(|| inner.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Python's own `str(float)`, for the values a test parametrizes with.
    #[test]
    fn floats_are_written_as_python_writes_them() {
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.5, "1.5"),
            (100.0, "100.0"),
            (0.1, "0.1"),
            (0.001, "0.001"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.25e100, "1.25e+100"),
            (123.456, "123.456"),
            (-2.5, "-2.5"),
        ];
        for (x, written) in cases {
            assert_eq!(python_float(x).as_deref(), Some(written), "{x}");
        }
        assert_eq!(python_float(f64::INFINITY), None);
    }

    #[test]
    fn integers_read_in_every_base() {
        assert_eq!(integer("1_000"), Some(1000));
        assert_eq!(integer("0x1F"), Some(31));
        assert_eq!(integer("0o17"), Some(15));
        assert_eq!(integer("0b101"), Some(5));
        assert_eq!(integer("3j"), None);
    }
}
