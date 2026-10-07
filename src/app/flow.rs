//! The FLOW tab: where a value comes from and where it goes. The language
//! server finds every occurrence of the identifier (its definition and its
//! references, project-wide); the text of each occurrence's line says what
//! that line does with it ([`flow::classify`]): declares or assigns it,
//! binds it as a parameter, passes it to a call, returns it, branches on it,
//! reaches into it, or reads it. A `Passed` node follows the value into the
//! callee: its definition is looked up, the argument's parameter read off
//! the declaration from the callee's name on, over the lines a wrapped
//! parameter list runs onto ([`flow::parameter_in`]), and that parameter's
//! occurrences become the node's children — as deep as the reader cares to
//! go.
//!
//! Lines are classified as the file has them, untrimmed, so the server's
//! columns land where they point. When a traced file changes, each
//! occurrence follows its line by the line's text (`App::reanchor_flow`);
//! one whose line reads differently now is marked changed, moved as far as
//! its nearest followed neighbour, and the tab offers to trace again.
//!
//! Line text comes from the open panes when the file is open, else off disk
//! for a local project. A remote project's occurrences in files not open
//! here are shown by location, unclassified (the tab says so): reading
//! every file of a remote project for one trace is not worth the round
//! trips.
//!
//! Its messages, [`FlowMsg`], arrive through `App::update_flow`.

use crate::app::navigation::read_location_lines;
use crate::app::prelude::*;
use crate::graph::tree;
use crate::*;

pub(crate) use crate::flow::{Role, Use};

/// Occurrences a trace keeps at most (per level).
pub(crate) const MAX_FLOW_NODES: usize = 400;

/// Lines a callee's declaration is read over to find a parameter: a
/// parameter list wrapped one per line, as rustfmt and black wrap long ones.
pub(crate) const SIGNATURE_LINES: usize = 32;

/// One occurrence of a traced identifier.
#[derive(Debug, Clone)]
pub struct FlowNode {
    /// The identifier this node is an occurrence of (a followed parameter's
    /// name differs from the root's).
    pub symbol: String,
    pub role: Role,
    /// The callee for a `Passed`, what was assigned for an `Assigned`.
    pub detail: String,
    /// For a `Passed`: the argument's 0-based index, and the callee's name
    /// and char column on the line, for following the value in.
    pub argument: Option<usize>,
    pub callee: Option<(String, usize)>,
    pub abs: PathBuf,
    pub rel: String,
    /// 0-based line.
    pub line: usize,
    /// The occurrence's position in the server's encoding (a column once
    /// the line's text is known: `col`).
    pub character: usize,
    pub col: usize,
    /// The line's text, trimmed; empty until read.
    pub text: String,
    /// Whether `text` was read and the role classified from it.
    pub classified: bool,
    /// The raw line's leading whitespace, in chars: `col`, the callee's
    /// column and the server's `character` are on the raw line, `text` is
    /// trimmed.
    pub indent: usize,
    /// The file changed since and this line could not be found in it any
    /// more: the row keeps what it read, at about where the line went.
    pub changed: bool,
    pub depth: usize,
    pub parent: Option<usize>,
    /// `None` until followed (a `Passed` node), else the children.
    pub children: Option<Vec<usize>>,
    pub expanded: bool,
    pub loading: bool,
}

impl tree::TreeNode for FlowNode {
    fn parent(&self) -> Option<usize> {
        self.parent
    }
    fn children(&self) -> Option<&[usize]> {
        self.children.as_deref()
    }
    fn expanded(&self) -> bool {
        self.expanded
    }
}

/// The trace: an arena of nodes, the root occurrences first.
#[derive(Debug, Clone)]
pub struct FlowTree {
    pub token: u64,
    pub symbol: String,
    pub lang: &'static str,
    /// Where the trace was asked for: the file and the 0-based line, and the
    /// identifier's char column on it (`origin_col`).
    pub origin: (PathBuf, usize),
    pub origin_col: usize,
    /// A note for the reader (occurrences left unclassified, a cap hit).
    pub note: Option<String>,
    /// A traced file changed and some occurrence could not be followed to
    /// its new line (see `App::reanchor_flow`).
    pub stale: bool,
    nodes: Vec<FlowNode>,
    roots: Vec<usize>,
}

impl FlowTree {
    pub fn new(token: u64, symbol: String, lang: &'static str, origin: (PathBuf, usize)) -> Self {
        FlowTree {
            token,
            symbol,
            lang,
            origin,
            origin_col: 0,
            note: None,
            stale: false,
            nodes: Vec::new(),
            roots: Vec::new(),
        }
    }

    pub fn push_root(&mut self, node: FlowNode) -> usize {
        let id = self.nodes.len();
        self.nodes.push(node);
        self.roots.push(id);
        id
    }

    pub fn node(&self, id: usize) -> &FlowNode {
        &self.nodes[id]
    }

    pub fn get(&self, id: usize) -> Option<&FlowNode> {
        self.nodes.get(id)
    }

    pub fn node_mut(&mut self, id: usize) -> Option<&mut FlowNode> {
        self.nodes.get_mut(id)
    }

    pub fn roots(&self) -> &[usize] {
        &self.roots
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Whether an occurrence of the trace is in `path`.
    pub fn depends_on(&self, path: &Path) -> bool {
        self.origin.0 == path || self.nodes.iter().any(|n| n.abs == path)
    }

    /// The root occurrences under each role, in the roles' order, roles
    /// without any left out.
    pub fn grouped_roots(&self) -> Vec<(Role, Vec<usize>)> {
        Role::ALL
            .iter()
            .filter_map(|&role| {
                let ids: Vec<usize> = self
                    .roots
                    .iter()
                    .copied()
                    .filter(|&id| self.nodes[id].role == role)
                    .collect();
                (!ids.is_empty()).then_some((role, ids))
            })
            .collect()
    }

    /// The node's children, display-ordered under it; a `Passed` node's
    /// followed parameter first.
    pub fn set_children(&mut self, id: usize, children: Vec<FlowNode>) -> Vec<usize> {
        let Some(node) = self.nodes.get_mut(id) else {
            return Vec::new();
        };
        node.loading = false;
        if node.children.is_some() {
            node.expanded = true;
            return Vec::new();
        }
        let depth = node.depth + 1;
        let ids: Vec<usize> = children
            .into_iter()
            .take(MAX_FLOW_NODES)
            .map(|mut child| {
                child.depth = depth;
                child.parent = Some(id);
                let cid = self.nodes.len();
                self.nodes.push(child);
                cid
            })
            .collect();
        let node = &mut self.nodes[id];
        node.children = Some(ids.clone());
        node.expanded = true;
        ids
    }

    pub fn toggle(&mut self, id: usize) {
        if let Some(n) = self.nodes.get_mut(id)
            && n.children.is_some()
        {
            n.expanded = !n.expanded;
        }
    }

    /// The nodes under `root`, display-ordered (the root itself first).
    pub fn visible_under(&self, root: usize) -> Vec<usize> {
        tree::visible(&self.nodes, &[root])
    }

    /// How many occurrences are still unclassified (their line unread).
    pub fn unclassified(&self) -> usize {
        self.nodes.iter().filter(|n| !n.classified).count()
    }
}

/// What following a `Passed` node found: the callee's parameter, where it
/// is declared, and its occurrences with their lines' texts where read.
#[derive(Debug, Clone)]
pub struct Followed {
    pub symbol: String,
    pub declared: lsp::client::Target,
    pub decl_text: String,
    pub refs: Vec<lsp::client::Target>,
    /// Line texts read off disk, by index into `refs`.
    pub texts: Vec<(usize, String)>,
}

/// A node for `target`, classified when `text` (the line, untrimmed) is
/// known; `role_hint` overrides the classification (a definition is
/// `Declared` whatever its line says).
fn node_for(
    symbol: &str,
    target: &lsp::client::Target,
    rel: String,
    text: Option<&str>,
    encoding: clew_core::lsp::client::PositionEncoding,
    role_hint: Option<Role>,
) -> FlowNode {
    let mut node = FlowNode {
        symbol: symbol.to_string(),
        role: role_hint.unwrap_or(Role::Read),
        detail: String::new(),
        argument: None,
        callee: None,
        abs: target.path.clone(),
        rel,
        line: target.line,
        character: target.character,
        col: 0,
        text: String::new(),
        classified: false,
        indent: 0,
        changed: false,
        depth: 0,
        parent: None,
        children: None,
        expanded: false,
        loading: false,
    };
    if let Some(text) = text {
        classify_node(&mut node, text, encoding, role_hint);
    }
    node
}

fn classify_node(
    node: &mut FlowNode,
    text: &str,
    encoding: clew_core::lsp::client::PositionEncoding,
    role_hint: Option<Role>,
) {
    let col = viewer::Col::from_offset(text, node.character, encoding).0;
    node.col = col;
    node.indent = text.chars().take_while(|c| c.is_whitespace()).count();
    node.text = text.trim().to_string();
    node.classified = true;
    // A definition is `Declared` whatever its line says — unless the line
    // reassigns (`x += …`): Python's server lists every assignment among a
    // name's definitions.
    let hint = role_hint
        .filter(|&r| !(r == Role::Declared && crate::flow::reassigns(text, col, &node.symbol)));
    if let Some(role) = hint {
        node.role = role;
        return;
    }
    let Use {
        role,
        detail,
        argument,
        callee,
    } = crate::flow::classify(text, col, &node.symbol);
    node.role = role;
    node.detail = detail;
    node.argument = argument;
    node.callee = callee;
}

impl App {
    /// Trace the identifier at `(line, col)` of `pane`: its definition and
    /// references from the language server, then their lines.
    pub(crate) fn flow_at(&mut self, pane: usize, line: usize, col: usize) -> Task<Message> {
        let Some((lang, path, word, start, source_line)) = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .and_then(|v| {
                let lang = v.lang_key?;
                let word = analyze::word_at(&v.lines, line, col)?;
                let (start, _) = analyze::word_range_at(&v.lines, line, col)?;
                Some((
                    lang,
                    v.abs.clone(),
                    word,
                    start,
                    v.source_line(line).unwrap_or("").to_string(),
                ))
            })
        else {
            self.status = "Put the cursor on an identifier to trace it".into();
            return Task::none();
        };
        let client = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready — a value trace needs one");
                return Task::none();
            }
        };
        let character = viewer::Col(start).to_offset(&source_line, client.encoding);
        self.status = format!("Tracing `{word}`…");
        self.flow_token += 1;
        let token = self.flow_token;
        self.proj.flow_pending = Some(token);
        let mut tree = FlowTree::new(token, word.clone(), lang, (path.clone(), line));
        tree.origin_col = start;
        self.proj.flow = Some(tree);
        self.sidebar = SidebarTab::Flow;
        let stamp = self.stamp();
        let reveal = ui::reveal_sidebar_tab(SidebarTab::Flow);
        let found = Task::perform(
            async move {
                let defs = client
                    .navigate("textDocument/definition", &path, line, character)
                    .await
                    .unwrap_or_default();
                let refs = client
                    .navigate("textDocument/references", &path, line, character)
                    .await?;
                Ok((defs, refs))
            },
            move |result| {
                Message::Flow(FlowMsg::Found {
                    stamp: stamp.clone(),
                    token,
                    symbol: word.clone(),
                    result,
                })
            },
        );
        Task::batch([reveal, found])
    }

    pub(crate) fn on_flow_found(
        &mut self,
        token: u64,
        symbol: String,
        result: Result<(Vec<lsp::client::Target>, Vec<lsp::client::Target>), String>,
    ) -> Task<Message> {
        if self.proj.flow_pending != Some(token) {
            return Task::none();
        }
        self.proj.flow_pending = None;
        let Some(lang) = self.proj.flow.as_ref().map(|t| t.lang) else {
            return Task::none();
        };
        let encoding = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.encoding,
            _ => clew_core::lsp::client::PositionEncoding::Utf16,
        };
        let (defs, refs) = match result {
            Ok(found) => found,
            Err(e) => {
                self.status = format!("Trace failed: {e}");
                self.proj.flow = None;
                return Task::none();
            }
        };
        let mut nodes: Vec<FlowNode> = Vec::new();
        let mut to_read: Vec<(usize, PathBuf, usize)> = Vec::new();
        let mut seen: HashSet<(PathBuf, usize, usize)> = HashSet::new();
        for (target, hint) in defs
            .iter()
            .map(|t| (t, Some(Role::Declared)))
            .chain(refs.iter().map(|t| (t, None)))
            .take(MAX_FLOW_NODES)
        {
            if !seen.insert((target.path.clone(), target.line, target.character)) {
                continue;
            }
            let rel = self.rel_of(&target.path);
            let text = self.pane_line(&target.path, target.line);
            if text.is_none() {
                to_read.push((nodes.len(), target.path.clone(), target.line));
            }
            nodes.push(node_for(
                &symbol,
                target,
                rel,
                text.as_deref(),
                encoding,
                hint,
            ));
        }
        let local = self.local_project_state();
        let Some(tree) = self.proj.flow.as_mut() else {
            return Task::none();
        };
        let n = nodes.len();
        for node in nodes {
            tree.push_root(node);
        }
        // Definitions and references share the cap: the note counts both.
        let total = defs.len() + refs.len();
        if total > MAX_FLOW_NODES {
            tree.note = Some(format!(
                "{total} occurrences; the first {MAX_FLOW_NODES} are shown"
            ));
        } else if !local && !to_read.is_empty() {
            tree.note = Some(format!(
                "{} occurrences in files not open here are listed by location only; open a \
                 file to classify its lines",
                to_read.len()
            ));
        }
        self.status = format!("`{symbol}`: {n} occurrences");
        if to_read.is_empty() || !local {
            return Task::none();
        }
        let stamp = self.stamp();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || read_location_lines(to_read))
                    .await
                    .unwrap_or_default()
            },
            move |lines| {
                Message::Flow(FlowMsg::Lines {
                    stamp: stamp.clone(),
                    token,
                    lines,
                })
            },
        )
    }

    /// The text of line `line` (0-based) of `path`, from an open pane.
    fn pane_line(&self, path: &Path, line: usize) -> Option<String> {
        self.proj
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == path)
            .and_then(|v| v.source_line(line).map(str::to_string))
    }

    pub(crate) fn on_flow_lines(
        &mut self,
        token: u64,
        lines: Vec<(usize, String)>,
    ) -> Task<Message> {
        let Some(tree) = self.proj.flow.as_mut().filter(|t| t.token == token) else {
            return Task::none();
        };
        let encoding = match self.proj.link.lsp.get(tree.lang) {
            Some(LspSlot::Ready(c)) => c.encoding,
            _ => clew_core::lsp::client::PositionEncoding::Utf16,
        };
        for (id, text) in lines {
            if let Some(node) = tree.node_mut(id)
                && !node.classified
            {
                let hint = (node.role == Role::Declared).then_some(Role::Declared);
                classify_node(node, &text, encoding, hint);
            }
        }
        Task::none()
    }

    /// Follow a `Passed` node into the callee's parameter.
    pub(crate) fn on_flow_expand(&mut self, token: u64, id: usize) -> Task<Message> {
        // What the node says, read and released before anything else of
        // the app is touched.
        let (lang, symbol, callee, callee_col, argument, abs, line, line_text, indent) = {
            let Some(tree) = self.proj.flow.as_mut().filter(|t| t.token == token) else {
                return Task::none();
            };
            let lang = tree.lang;
            let symbol = tree.symbol.clone();
            let Some(node) = tree.node_mut(id) else {
                return Task::none();
            };
            if node.children.is_some() {
                node.expanded = !node.expanded;
                return Task::none();
            }
            let (Some((callee, callee_col)), Some(argument)) = (node.callee.clone(), node.argument)
            else {
                return Task::none();
            };
            if node.loading {
                return Task::none();
            }
            (
                lang,
                symbol,
                callee,
                callee_col,
                argument,
                node.abs.clone(),
                node.line,
                node.text.clone(),
                node.indent,
            )
        };
        let client = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready");
                return Task::none();
            }
        };
        let local = self.local_project_state();
        // The callee's column is on the raw line the node was classified
        // from: the pane's when the file is open here, else the node's text
        // put back behind its indentation (whitespace is one unit in every
        // encoding, so spaces stand in for tabs).
        let base = self
            .pane_line(&abs, line)
            .unwrap_or_else(|| format!("{}{line_text}", " ".repeat(indent)));
        let character = viewer::Col(callee_col).to_offset(&base, client.encoding);
        // Signature lines of the files open here, in case the callee is in one.
        let open_lines: Vec<(PathBuf, Vec<String>)> = self
            .proj
            .panes
            .iter()
            .flatten()
            .map(|v| {
                (
                    v.abs.clone(),
                    (0..v.lines.len())
                        .map(|l| v.source_line(l).unwrap_or("").to_string())
                        .collect(),
                )
            })
            .collect();
        if let Some(node) = self.proj.flow.as_mut().and_then(|t| t.node_mut(id)) {
            node.loading = true;
        }
        let stamp = self.stamp();
        self.status = format!("Following `{symbol}` into `{callee}`…");
        Task::perform(
            async move {
                let defs = client
                    .navigate("textDocument/definition", &abs, line, character)
                    .await?;
                let declared = defs
                    .into_iter()
                    .next()
                    .ok_or_else(|| format!("no definition of `{callee}` found"))?;
                // The declaration from the callee's name on, over the lines
                // its parameter list may wrap onto.
                let sig_lines: Vec<String> =
                    match open_lines.iter().find(|(p, _)| *p == declared.path) {
                        Some((_, lines)) => lines
                            .iter()
                            .skip(declared.line)
                            .take(SIGNATURE_LINES)
                            .cloned()
                            .collect(),
                        None if local => {
                            let wanted: Vec<(usize, PathBuf, usize)> = (0..SIGNATURE_LINES)
                                .map(|k| (k, declared.path.clone(), declared.line + k))
                                .collect();
                            // The unbroken run from the declaration's line (a
                            // file shorter than the window ends it early).
                            read_location_lines(wanted)
                                .into_iter()
                                .enumerate()
                                .take_while(|(i, (k, _))| i == k)
                                .map(|(_, (_, text))| text)
                                .collect()
                        }
                        None => Vec::new(),
                    };
                let Some(first) = sig_lines.first() else {
                    return Err(format!(
                        "open {} to follow into `{callee}`",
                        declared
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ));
                };
                let name_col =
                    viewer::Col::from_offset(first, declared.character, client.encoding).0;
                let (param, offset, param_col) =
                    crate::flow::parameter_in(&sig_lines, name_col, argument, lang)
                        .ok_or_else(|| format!("`{callee}` has no parameter {}", argument + 1))?;
                let param_text = sig_lines[offset].clone();
                let param_line = declared.line + offset;
                let param_character =
                    viewer::Col(param_col).to_offset(&param_text, client.encoding);
                let refs = client
                    .navigate(
                        "textDocument/references",
                        &declared.path,
                        param_line,
                        param_character,
                    )
                    .await?;
                let texts = if local {
                    let wanted: Vec<(usize, PathBuf, usize)> = refs
                        .iter()
                        .enumerate()
                        .map(|(i, t)| (i, t.path.clone(), t.line))
                        .collect();
                    read_location_lines(wanted)
                } else {
                    Vec::new()
                };
                Ok(Followed {
                    symbol: param,
                    declared: lsp::client::Target {
                        path: declared.path,
                        line: param_line,
                        character: param_character,
                    },
                    decl_text: param_text,
                    refs,
                    texts,
                })
            },
            move |result| {
                Message::Flow(FlowMsg::Expanded {
                    stamp: stamp.clone(),
                    token,
                    id,
                    result,
                })
            },
        )
    }

    pub(crate) fn on_flow_expanded(
        &mut self,
        token: u64,
        id: usize,
        result: Result<Followed, String>,
    ) -> Task<Message> {
        let Some(lang) = self
            .proj
            .flow
            .as_ref()
            .filter(|t| t.token == token)
            .map(|t| t.lang)
        else {
            return Task::none();
        };
        let encoding = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.encoding,
            _ => clew_core::lsp::client::PositionEncoding::Utf16,
        };
        let followed = match result {
            Ok(f) => f,
            Err(e) => {
                if let Some(node) = self.proj.flow.as_mut().and_then(|t| t.node_mut(id)) {
                    node.loading = false;
                }
                self.status = format!("Couldn't follow the value: {e}");
                return Task::none();
            }
        };
        let mut children: Vec<FlowNode> = Vec::new();
        let rel = self.rel_of(&followed.declared.path);
        children.push(node_for(
            &followed.symbol,
            &followed.declared,
            rel,
            Some(&followed.decl_text),
            encoding,
            Some(Role::Parameter),
        ));
        let texts: HashMap<usize, String> = followed.texts.into_iter().collect();
        let mut seen: HashSet<(PathBuf, usize, usize)> = HashSet::new();
        seen.insert((
            followed.declared.path.clone(),
            followed.declared.line,
            followed.declared.character,
        ));
        for (i, target) in followed.refs.iter().enumerate() {
            if !seen.insert((target.path.clone(), target.line, target.character)) {
                continue;
            }
            let rel = self.rel_of(&target.path);
            let text = texts
                .get(&i)
                .cloned()
                .or_else(|| self.pane_line(&target.path, target.line));
            children.push(node_for(
                &followed.symbol,
                target,
                rel,
                text.as_deref(),
                encoding,
                None,
            ));
        }
        let n = children.len();
        if let Some(tree) = self.proj.flow.as_mut() {
            tree.set_children(id, children);
        }
        self.status = format!("`{}`: {n} occurrences", followed.symbol);
        Task::none()
    }

    /// Follow the trace's occurrences in `path` to their lines in its new
    /// `content` (`None`: the file is gone): each by its own line's text,
    /// nearest where it was, so a row still opens where its line now is. An
    /// occurrence whose line reads differently now is marked changed, moved
    /// as far as its nearest followed neighbour, and the trace stale — the
    /// tab offers to trace again.
    pub(crate) fn reanchor_flow(&mut self, path: &Path, content: Option<&str>) {
        let Some(lang) = self
            .proj
            .flow
            .as_ref()
            .filter(|t| t.depends_on(path))
            .map(|t| t.lang)
        else {
            return;
        };
        let encoding = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.encoding,
            _ => clew_core::lsp::client::PositionEncoding::Utf16,
        };
        let lines: Vec<&str> = content
            .map(|c| c.lines().map(|l| l.trim_end_matches('\r')).collect())
            .unwrap_or_default();
        let Some(tree) = self.proj.flow.as_mut() else {
            return;
        };
        let place = |node: &mut FlowNode, line: usize| {
            let raw = lines[line];
            let col = crate::flow::nearest_word(raw, &node.symbol, node.col)
                .unwrap_or_else(|| node.col.min(raw.chars().count()));
            node.line = line;
            node.col = col;
            node.character = viewer::Col(col).to_offset(raw, encoding);
            node.indent = raw.chars().take_while(|c| c.is_whitespace()).count();
        };
        // Rows whose line still reads the same move to it; the rest are
        // changed, and move as far as their nearest found neighbour did. A
        // row never read (a remote project's, listed by location) has no
        // text to look for: it moves with its neighbours and is not said to
        // have changed.
        let mut anchors = Vec::new();
        let mut lost = Vec::new();
        let mut unread = Vec::new();
        for (i, node) in tree.nodes.iter_mut().enumerate() {
            if node.abs != path {
                continue;
            }
            if !node.classified && content.is_some() {
                unread.push(i);
                continue;
            }
            let found = content
                .filter(|_| node.classified)
                .and_then(|_| crate::flow::moved_line(&lines, &node.text, node.line));
            match found {
                Some(line) => {
                    anchors.push((node.line, line));
                    place(node, line);
                    node.changed = false;
                }
                None => lost.push(i),
            }
        }
        tree.stale |= !lost.is_empty();
        for i in lost {
            let node = &mut tree.nodes[i];
            node.changed = true;
            if let Some(line) = crate::flow::carried_line(&anchors, node.line, lines.len()) {
                place(node, line);
            }
        }
        for i in unread {
            let node = &mut tree.nodes[i];
            if let Some(line) = crate::flow::carried_line(&anchors, node.line, lines.len()) {
                place(node, line);
            }
        }
    }

    /// Trace the same identifier again from where it was asked for, found
    /// in the file as it is now (nearest the line and column it was at).
    fn retrace_flow(&mut self) -> Task<Message> {
        let Some((path, line, col, symbol)) = self.proj.flow.as_ref().map(|t| {
            (
                t.origin.0.clone(),
                t.origin.1,
                t.origin_col,
                t.symbol.clone(),
            )
        }) else {
            return Task::none();
        };
        let Some(pane) = self
            .proj
            .panes
            .iter()
            .position(|p| p.as_ref().is_some_and(|v| v.abs == path))
        else {
            self.status = format!(
                "Open {} to trace `{symbol}` again",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            );
            return Task::none();
        };
        let found = self.proj.panes[pane].as_ref().and_then(|v| {
            let n = v.lines.len();
            (0..n.max(line + 1)).find_map(|d| {
                [line + d, line.wrapping_sub(d)]
                    .into_iter()
                    .filter(|&l| l < n)
                    .find_map(|l| {
                        v.source_line(l)
                            .and_then(|text| crate::flow::nearest_word(text, &symbol, col))
                            .map(|c| (l, c))
                    })
            })
        });
        match found {
            Some((l, c)) => self.flow_at(pane, l, c),
            None => {
                self.status = format!("`{symbol}` is no longer in that file");
                Task::none()
            }
        }
    }

    pub(crate) fn update_flow(&mut self, message: FlowMsg) -> Task<Message> {
        match message {
            FlowMsg::FromMenu => {
                let Some(menu) = self.proj.context_menu.take() else {
                    return Task::none();
                };
                self.flow_at(menu.pane, menu.line, menu.col)
            }
            FlowMsg::AtCursor => {
                let pane = self.proj.active;
                let Some((line, col)) = self.active_viewer().and_then(|v| v.caret) else {
                    self.status = "Put the cursor on a name to trace its value".into();
                    return Task::none();
                };
                self.flow_at(pane, line, col)
            }
            FlowMsg::Found {
                token,
                symbol,
                result,
                ..
            } => self.on_flow_found(token, symbol, result),
            FlowMsg::Lines { token, lines, .. } => self.on_flow_lines(token, lines),
            FlowMsg::Expand { token, id } => self.on_flow_expand(token, id),
            FlowMsg::Expanded {
                token, id, result, ..
            } => self.on_flow_expanded(token, id, result),
            FlowMsg::Retrace => self.retrace_flow(),
            FlowMsg::Toggle { token, id } => {
                if let Some(tree) = self.proj.flow.as_mut().filter(|t| t.token == token) {
                    tree.toggle(id);
                }
                Task::none()
            }
            FlowMsg::Clear => {
                self.proj.flow = None;
                self.proj.flow_pending = None;
                Task::none()
            }
        }
    }
}
