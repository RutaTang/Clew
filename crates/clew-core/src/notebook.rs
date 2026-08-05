//! Jupyter notebook (.ipynb, nbformat 4.x) parsing for read-only viewing.
//!
//! A notebook is JSON: markdown/code cells plus serialized outputs (MIME
//! bundles). clew renders it natively — markdown through the richmd pipeline,
//! code through tree-sitter, images/SVG through iced — so nothing here touches
//! a webview or a kernel. Everything text-search-shaped (grep, the Ask agent,
//! explanations) consumes the *script projection*: the notebook flattened to a
//! jupytext-style `# %%` script whose line numbers are the notebook's canonical
//! "line" space (search hits, citations, outline entries all use them).

use serde::Deserialize;

/// Largest embedded image kept (base64-decoded); bigger outputs become a
/// placeholder so one giant figure can't balloon the protocol frame.
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// Cap per text output, so a runaway training log stays readable.
const MAX_TEXT_CHARS: usize = 20_000;

/// Whether this path is a Jupyter notebook.
pub fn is_notebook(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ipynb"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellKind {
    Markdown,
    Code,
    /// nbformat "raw" — shown as plain text.
    Raw,
}

/// One rendered-ready output of a code cell.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Stream/plain text, ANSI colors resolved into spans.
    Text {
        /// `(text, ansi_color)` runs; color is a 0–15 ANSI palette index.
        spans: Vec<(String, Option<u8>)>,
        stderr: bool,
    },
    /// A base64-decoded raster image (PNG/JPEG).
    Image {
        data: Vec<u8>,
    },
    Svg(String),
    /// Output clew doesn't render natively (interactive widgets, HTML-only);
    /// the label names what was skipped.
    Placeholder(String),
}

#[derive(Debug, Clone)]
pub struct Cell {
    pub kind: CellKind,
    pub source: String,
    /// 1-based first line of this cell's content in the script projection.
    pub proj_line: usize,
    pub outputs: Vec<Output>,
    pub execution_count: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Notebook {
    /// Language key for highlighting code cells (tree-sitter key when known).
    pub language: String,
    pub cells: Vec<Cell>,
    /// The jupytext-style `# %%` script projection of the whole notebook.
    pub projection: String,
}

// -- raw JSON shape (tolerant: unknown fields ignored) ------------------------

#[derive(Deserialize)]
struct RawNotebook {
    #[serde(default)]
    cells: Vec<RawCell>,
    #[serde(default)]
    metadata: serde_json::Value,
}

#[derive(Deserialize)]
struct RawCell {
    #[serde(default)]
    cell_type: String,
    #[serde(default)]
    source: SourceLines,
    #[serde(default)]
    outputs: Vec<serde_json::Value>,
    #[serde(default)]
    execution_count: Option<u64>,
}

/// nbformat allows a cell's source (and text fields) as either one string or a
/// list of line strings.
#[derive(Deserialize, Default)]
#[serde(untagged)]
enum SourceLines {
    #[default]
    Missing,
    One(String),
    Many(Vec<String>),
}

impl SourceLines {
    fn join(self) -> String {
        match self {
            SourceLines::Missing => String::new(),
            SourceLines::One(s) => s,
            SourceLines::Many(v) => v.concat(),
        }
    }
}

/// Parse a notebook from its JSON source. Returns `None` when the JSON does not
/// look like a notebook at all (callers fall back to the plain-text view).
pub fn parse(json: &str) -> Option<Notebook> {
    let raw: RawNotebook = serde_json::from_str(json).ok()?;
    let language = language_of(&raw.metadata);
    let mut cells: Vec<Cell> = raw
        .cells
        .into_iter()
        .map(|c| {
            let kind = match c.cell_type.as_str() {
                "markdown" => CellKind::Markdown,
                "code" => CellKind::Code,
                _ => CellKind::Raw,
            };
            let outputs = if kind == CellKind::Code {
                c.outputs.iter().filter_map(parse_output).collect()
            } else {
                Vec::new()
            };
            Cell {
                kind,
                source: c.source.join(),
                proj_line: 0, // filled below, alongside the projection
                outputs,
                execution_count: c.execution_count,
            }
        })
        .collect();
    let projection = build_projection(&mut cells);
    Some(Notebook {
        language,
        cells,
        projection,
    })
}

/// Highlighting key for the notebook's language, from kernelspec/language_info.
fn language_of(metadata: &serde_json::Value) -> String {
    let name = metadata
        .pointer("/language_info/name")
        .or_else(|| metadata.pointer("/kernelspec/language"))
        .and_then(|v| v.as_str())
        .unwrap_or("python");
    // Normalize a few kernel spellings onto tree-sitter keys.
    match name.to_ascii_lowercase().as_str() {
        "python" | "python3" | "ipython" => "python",
        "javascript" | "nodejs" => "javascript",
        "typescript" => "typescript",
        "rust" => "rust",
        "bash" | "sh" => "bash",
        other => return other.to_string(),
    }
    .to_string()
}

/// One output value → the renderable it becomes, or `None` to drop it silently
/// (empty streams).
fn parse_output(v: &serde_json::Value) -> Option<Output> {
    let text_of = |v: &serde_json::Value| -> String {
        match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(a) => a
                .iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .concat(),
            other => other.to_string(),
        }
    };
    match v.get("output_type").and_then(|t| t.as_str())? {
        "stream" => {
            let text = text_of(v.get("text")?);
            if text.trim().is_empty() {
                return None;
            }
            let stderr = v.get("name").and_then(|n| n.as_str()) == Some("stderr");
            Some(Output::Text {
                spans: ansi_spans(&cap_text(&text)),
                stderr,
            })
        }
        "error" => {
            // The traceback carries the ANSI-colored panic; join its lines.
            let tb = v
                .get("traceback")
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| {
                    format!(
                        "{}: {}",
                        v.get("ename").and_then(|s| s.as_str()).unwrap_or("Error"),
                        v.get("evalue").and_then(|s| s.as_str()).unwrap_or("")
                    )
                });
            Some(Output::Text {
                spans: ansi_spans(&cap_text(&tb)),
                stderr: true,
            })
        }
        "execute_result" | "display_data" => {
            let data = v.get("data")?;
            // Prefer natively renderable MIME types, richest first.
            if let Some(svg) = data.get("image/svg+xml") {
                return Some(Output::Svg(text_of(svg)));
            }
            for mime in ["image/png", "image/jpeg"] {
                if let Some(b64) = data.get(mime) {
                    let cleaned: String = text_of(b64)
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect();
                    if let Ok(bytes) = base64_decode(&cleaned) {
                        if bytes.len() <= MAX_IMAGE_BYTES {
                            return Some(Output::Image { data: bytes });
                        }
                        return Some(Output::Placeholder(format!(
                            "{mime} ({} MB, too large)",
                            bytes.len() / (1024 * 1024)
                        )));
                    }
                }
            }
            if let Some(text) = data.get("text/plain") {
                let text = text_of(text);
                if text.trim().is_empty() {
                    return None;
                }
                return Some(Output::Text {
                    spans: ansi_spans(&cap_text(&text)),
                    stderr: false,
                });
            }
            // HTML-only (interactive widgets, plotly, …): name what's skipped.
            let label = data
                .as_object()
                .and_then(|o| o.keys().next())
                .cloned()
                .unwrap_or_else(|| "output".into());
            Some(Output::Placeholder(label))
        }
        _ => None,
    }
}

fn cap_text(s: &str) -> String {
    if s.chars().count() <= MAX_TEXT_CHARS {
        return s.to_string();
    }
    let capped: String = s.chars().take(MAX_TEXT_CHARS).collect();
    format!("{capped}\n… [truncated]")
}

/// Build the `# %%` script projection and stamp each cell's projection line.
fn build_projection(cells: &mut [Cell]) -> String {
    let mut out = String::new();
    let mut line = 1usize;
    let push = |s: &str, line: &mut usize, out: &mut String| {
        out.push_str(s);
        out.push('\n');
        *line += 1;
    };
    for (i, cell) in cells.iter_mut().enumerate() {
        if i > 0 {
            push("", &mut line, &mut out);
        }
        match cell.kind {
            CellKind::Markdown | CellKind::Raw => {
                push("# %% [markdown]", &mut line, &mut out);
                cell.proj_line = line;
                for l in cell.source.lines() {
                    push(&format!("# {l}"), &mut line, &mut out);
                }
                if cell.source.is_empty() {
                    push("#", &mut line, &mut out);
                }
            }
            CellKind::Code => {
                push("# %%", &mut line, &mut out);
                cell.proj_line = line;
                for l in cell.source.lines() {
                    push(l, &mut line, &mut out);
                }
                if cell.source.is_empty() {
                    push("", &mut line, &mut out);
                }
            }
        }
    }
    out
}

impl Notebook {
    /// Index of the cell containing this 1-based projection line (the nearest
    /// cell starting at or before it).
    pub fn cell_at_line(&self, line: usize) -> usize {
        match self
            .cells
            .binary_search_by(|c| c.proj_line.cmp(&line.max(1)))
        {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        }
    }

    /// Outline entries: markdown headings by their text, code cells by their
    /// first non-empty line. `(name, kind, proj_line, proj_end_line)`.
    pub fn outline(&self) -> Vec<(String, String, usize, usize)> {
        let mut out = Vec::new();
        for (i, cell) in self.cells.iter().enumerate() {
            let end = self
                .cells
                .get(i + 1)
                .map(|n| n.proj_line.saturating_sub(1))
                .unwrap_or_else(|| self.projection.lines().count());
            match cell.kind {
                CellKind::Markdown => {
                    // Only heading lines make the outline — prose cells would
                    // flood it.
                    if let Some(h) = cell.source.lines().find_map(|l| {
                        let t = l.trim_start();
                        t.starts_with('#')
                            .then(|| t.trim_start_matches('#').trim().to_string())
                            .filter(|s| !s.is_empty())
                    }) {
                        out.push((h, "section".to_string(), cell.proj_line, end));
                    }
                }
                CellKind::Code => {
                    let first = cell
                        .source
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .trim();
                    let mut name: String = first.chars().take(60).collect();
                    if name.is_empty() {
                        name = "(empty)".into();
                    }
                    let label = match cell.execution_count {
                        Some(n) => format!("[{n}] {name}"),
                        None => name,
                    };
                    out.push((label, "cell".to_string(), cell.proj_line, end));
                }
                CellKind::Raw => {}
            }
        }
        out
    }
}

// -- ANSI ---------------------------------------------------------------------

/// Split text carrying ANSI SGR escapes into `(run, color)` spans, where color
/// is a 0–15 palette index (30–37 → 0–7, 90–97 → 8–15; the last-seen foreground
/// wins; reset clears). Non-SGR escapes are stripped.
pub fn ansi_spans(text: &str) -> Vec<(String, Option<u8>)> {
    let mut spans: Vec<(String, Option<u8>)> = Vec::new();
    let mut current: Option<u8> = None;
    let mut run = String::new();
    let mut chars = text.chars().peekable();
    let flush = |run: &mut String, color: Option<u8>, spans: &mut Vec<(String, Option<u8>)>| {
        if !run.is_empty() {
            spans.push((std::mem::take(run), color));
        }
    };
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            run.push(c);
            continue;
        }
        // An escape: only CSI `…m` (SGR) changes color; everything else is
        // dropped. Consume up to the final byte (@–~).
        if chars.peek() != Some(&'[') {
            continue;
        }
        chars.next();
        let mut params = String::new();
        let mut fin = ' ';
        for c in chars.by_ref() {
            if ('\x40'..='\x7e').contains(&c) {
                fin = c;
                break;
            }
            params.push(c);
        }
        if fin != 'm' {
            continue;
        }
        let next = sgr_color(&params, current);
        if next != current {
            flush(&mut run, current, &mut spans);
            current = next;
        }
    }
    flush(&mut run, current, &mut spans);
    spans
}

/// Apply an SGR parameter list to the current foreground color.
fn sgr_color(params: &str, current: Option<u8>) -> Option<u8> {
    let mut color = current;
    let mut nums = params.split(';').map(|p| p.trim().parse::<u32>());
    while let Some(n) = nums.next() {
        match n.unwrap_or(0) {
            0 | 39 => color = None,
            n @ 30..=37 => color = Some((n - 30) as u8),
            n @ 90..=97 => color = Some((n - 90 + 8) as u8),
            // 38;5;N / 38;2;r;g;b: extended color — approximate 16-color cube
            // membership, else drop to default.
            38 => match nums.next() {
                Some(Ok(5)) => {
                    if let Some(Ok(idx)) = nums.next() {
                        color = (idx < 16).then_some(idx as u8);
                    }
                }
                Some(Ok(2)) => {
                    let _ = (nums.next(), nums.next(), nums.next());
                    color = None;
                }
                _ => {}
            },
            _ => {}
        }
    }
    color
}

// -- base64 -------------------------------------------------------------------

/// Minimal standard-alphabet base64 decoder (padding optional) — enough for
/// notebook image payloads without pulling in a dependency.
fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    fn val(c: u8) -> Result<u32, ()> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(()),
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for &b in chunk {
            acc = (acc << 6) | val(b)?;
        }
        match chunk.len() {
            4 => out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]),
            3 => {
                let acc = acc << 6;
                out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8]);
            }
            2 => {
                let acc = acc << 12;
                out.push((acc >> 16) as u8);
            }
            _ => return Err(()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NB: &str = r##"{
      "nbformat": 4,
      "metadata": { "kernelspec": { "language": "python3" } },
      "cells": [
        { "cell_type": "markdown", "source": ["# Title\n", "Some prose."] },
        { "cell_type": "code", "execution_count": 2,
          "source": "x = 1\nprint(x)",
          "outputs": [
            { "output_type": "stream", "name": "stdout", "text": ["1\n"] },
            { "output_type": "display_data",
              "data": { "image/png": "aGVsbG8=", "text/plain": "<Figure>" } },
            { "output_type": "execute_result",
              "data": { "text/html": "<table></table>", "text/plain": ["df head"] } }
          ] },
        { "cell_type": "code", "source": [], "outputs": [
            { "output_type": "execute_result", "data": { "application/vnd.plotly.v1+json": {} } }
        ] }
      ]
    }"##;

    #[test]
    fn parses_cells_language_and_outputs() {
        let nb = parse(NB).expect("parses");
        assert_eq!(nb.language, "python");
        assert_eq!(nb.cells.len(), 3);
        assert_eq!(nb.cells[0].kind, CellKind::Markdown);
        assert_eq!(nb.cells[0].source, "# Title\nSome prose.");
        let code = &nb.cells[1];
        assert_eq!(code.execution_count, Some(2));
        // stream text, image (decoded), and the text/plain side of the html result.
        assert_eq!(code.outputs.len(), 3);
        assert!(matches!(
            &code.outputs[0],
            Output::Text { stderr: false, .. }
        ));
        assert!(matches!(&code.outputs[1], Output::Image { data } if data == b"hello"));
        assert!(
            matches!(&code.outputs[2], Output::Text { spans, .. } if spans[0].0.contains("df head"))
        );
        // Plotly-only output becomes a named placeholder.
        assert!(matches!(&nb.cells[2].outputs[0], Output::Placeholder(l) if l.contains("plotly")));
    }

    #[test]
    fn projection_maps_cells_to_lines() {
        let nb = parse(NB).expect("parses");
        let lines: Vec<&str> = nb.projection.lines().collect();
        assert_eq!(lines[0], "# %% [markdown]");
        assert_eq!(lines[1], "# # Title");
        assert_eq!(lines[2], "# Some prose.");
        // Cells know their first content line, and lookup inverts it.
        assert_eq!(nb.cells[0].proj_line, 2);
        assert_eq!(
            nb.cells[1].proj_line,
            lines.iter().position(|l| *l == "x = 1").unwrap() + 1
        );
        assert_eq!(nb.cell_at_line(2), 0);
        assert_eq!(nb.cell_at_line(nb.cells[1].proj_line + 1), 1);
        assert_eq!(nb.cell_at_line(9999), 2);
    }

    #[test]
    fn outline_lists_headings_and_code_cells() {
        let nb = parse(NB).expect("parses");
        let outline = nb.outline();
        assert_eq!(outline[0].0, "Title");
        assert_eq!(outline[0].1, "section");
        assert!(outline[1].0.contains("[2] x = 1"));
        assert_eq!(outline[1].1, "cell");
    }

    #[test]
    fn ansi_spans_color_and_strip() {
        let spans = ansi_spans("plain \x1b[31mred\x1b[0m \x1b[92mgreen\x1b[39m end");
        assert_eq!(
            spans,
            vec![
                ("plain ".to_string(), None),
                ("red".to_string(), Some(1)),
                (" ".to_string(), None),
                ("green".to_string(), Some(10)),
                (" end".to_string(), None),
            ]
        );
        // Non-SGR escapes (cursor moves) are stripped silently.
        let spans = ansi_spans("a\x1b[2Kb");
        assert_eq!(spans, vec![("ab".to_string(), None)]);
    }

    #[test]
    fn base64_roundtrip() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
        assert!(base64_decode("!!!").is_err());
    }

    #[test]
    fn non_notebook_json_is_rejected_gracefully() {
        assert!(parse("not json").is_none());
        // Valid JSON but no cells: parses to an empty notebook rather than None
        // (nbformat tolerates it), which renders as an empty file.
        let nb = parse("{}").expect("tolerant");
        assert!(nb.cells.is_empty());
    }
}
