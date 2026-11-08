//! The settings' languages of the open project: what its language servers
//! are given, and what the user chooses for them — the Python interpreter,
//! the Cargo features and target, the Go build tags and platform, the C/C++
//! compilation database and flags, a toolchain or a server program of their
//! own.
//!
//! The choices are the user's, for this project on this machine
//! (`clew_core::lsp::choices`, in clew's data directory — never the
//! project's `.clew/`, which is the team's): read when the project opens,
//! before any language server starts (`App::ensure_lsp` waits for them),
//! sent with every server and debugger set up for it, and saved the moment
//! one is made. What there is to choose from is found where the project is —
//! this machine, or the remote host it lives on — by reading files, never by
//! running anything (`clew_core::lsp::langopts`).

use crate::app::prelude::*;
use crate::*;

use clew_protocol::{CChoices, GoChoices, LanguageChoices, RustChoices, ServerChoice};

/// A language's name as a reader knows it ("C++"), for its key ("cpp"):
/// what a sentence says of it, never the key.
pub(crate) fn language_name(key: &str) -> &str {
    clew_core::highlight::lang_name(key).unwrap_or(key)
}

/// Languages by name, in a list ("C++, Rust").
pub(crate) fn language_names<S: AsRef<str>>(keys: &[S]) -> String {
    keys.iter()
        .map(|k| language_name(k.as_ref()))
        .collect::<Vec<_>>()
        .join(", ")
}

impl App {
    /// The languages Settings → Language servers lists, each with its
    /// server: those the project's code is written in, those given a server
    /// program of the user's own, and any other of the project's whose
    /// server was asked for (a file of it opened) — not every language one
    /// stray file is in.
    pub(crate) fn listed_languages(&self) -> Vec<String> {
        let mut rows = self.proj.code_languages.clone();
        if let Some(servers) = &self.proj.lang_choices.servers {
            rows.extend(servers.keys().cloned());
        }
        rows.extend(
            self.proj
                .project_languages
                .iter()
                .filter(|l| self.proj.link.lsp.contains_key(l.as_str()))
                .cloned(),
        );
        rows.sort();
        rows.dedup();
        rows
    }

    /// Read the user's choices for the open project's languages, off the UI
    /// thread; they land as `LanguagesMsg::ChoicesLoaded`. Language servers
    /// asked for meanwhile wait for them (`ensure_lsp`).
    pub(crate) fn load_language_choices(&mut self) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        self.proj.inflight.choices_loading = true;
        let host = self.connection.approval_host().map(str::to_string);
        let stamp = self.stamp();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || lsp::choices::load(host.as_deref(), &root))
                    .await
                    .unwrap_or_default()
            },
            move |choices| {
                Message::Languages(LanguagesMsg::ChoicesLoaded {
                    stamp: stamp.clone(),
                    choices,
                })
            },
        )
    }

    /// The server `language` runs in the open project: the project's
    /// `lsp.toml` over clew's defaults, with a server program the user chose
    /// over both (`ProjectLspConfig::resolve_with`).
    pub(crate) fn effective_server(&self, language: &str) -> Option<lsp::config::EffectiveServer> {
        self.proj
            .lsp_config
            .resolve_with(language, &self.proj.lang_choices)
    }

    pub(crate) fn update_languages(&mut self, message: LanguagesMsg) -> Task<Message> {
        match message {
            LanguagesMsg::ChoicesLoaded { choices, .. } => {
                self.proj.lang_choices = choices;
                self.proj.inflight.choices_loading = false;
                // The servers the setup sheet asked for, now that the
                // choices that pick them are in — before those a file asked
                // for meanwhile: on their way, they are waited for, where
                // asked first they each popped the very download question
                // the sheet's tick had answered, and the tick was skipped.
                let mut tasks = vec![
                    self.refresh_build_facts(),
                    self.apply_pending_setup(),
                    self.look_for_missing_setup(),
                ];
                // The servers asked for meanwhile — each waits on, if the
                // project's other state is still loading, that instead.
                for language in std::mem::take(&mut self.proj.inflight.deferred_lsp) {
                    tasks.push(self.ensure_lsp(&language));
                }
                Task::batch(tasks)
            }
            LanguagesMsg::FactsLoaded { result, .. } => {
                match result {
                    Ok(facts) => {
                        self.proj.build_facts = Arc::new(facts);
                        self.redim_open_panes();
                    }
                    // The dimming stays as the target alone decides it.
                    Err(e) => {
                        self.status = tr!(
                            "Could not tell which code the build excludes: {error}",
                            error = e
                        );
                    }
                }
                Task::none()
            }
            LanguagesMsg::Refresh => self.refresh_language_options(),
            LanguagesMsg::Listed { result, .. } => {
                self.proj.lang_options = Some(result);
                Task::none()
            }
            LanguagesMsg::ChoosePython(python) => {
                let mut next = self.proj.lang_choices.clone();
                next.python = python;
                self.proj.lang_drafts.python_path.clear();
                self.choose(next)
            }
            LanguagesMsg::Choose(next) => self.choose(*next),
            LanguagesMsg::Draft(field, text) => {
                *self.proj.lang_drafts.field_mut(field) = text;
                Task::none()
            }
            LanguagesMsg::UseDraft(field) => self.use_draft(field),
            LanguagesMsg::ServerLanguage(language) => {
                // The program already chosen for it, to edit.
                let chosen = self
                    .proj
                    .lang_choices
                    .servers
                    .as_ref()
                    .and_then(|s| s.get(&language))
                    .cloned();
                let drafts = &mut self.proj.lang_drafts;
                drafts.server_command = chosen
                    .as_ref()
                    .map(|c| c.command.clone())
                    .unwrap_or_default();
                drafts.server_args = chosen
                    .and_then(|c| c.args)
                    .map(|args| join_args(&args))
                    .unwrap_or_default();
                drafts.server_language = Some(language);
                Task::none()
            }
            LanguagesMsg::Browse(field) => {
                // The dialog shows this machine's files: a remote project's
                // paths are on its host, typed or picked from the lists.
                if self.connection.is_remote() || self.proj.project.is_none() {
                    return Task::none();
                }
                let stamp = self.stamp();
                let Some((folder, title)) = browse_for(field) else {
                    return Task::none();
                };
                let picked = if folder {
                    crate::app::tasks::pick_folder(self.main_window, title.into())
                } else {
                    crate::app::tasks::pick_file(self.main_window, title.into())
                };
                picked.map(move |picked| match picked {
                    Ok(path) => Message::Languages(LanguagesMsg::Browsed {
                        stamp: stamp.clone(),
                        field,
                        path,
                    }),
                    Err(_) => Message::Window(WindowMsg::NoFileDialog),
                })
            }
            LanguagesMsg::Browsed { field, path, .. } => match path {
                Some(path) => {
                    *self.proj.lang_drafts.field_mut(field) = path.to_string_lossy().into_owned();
                    match field {
                        // The program also wants its language and arguments:
                        // it waits in the field for "Use".
                        LangField::ServerCommand | LangField::ServerArgs => Task::none(),
                        _ => self.use_draft(field),
                    }
                }
                None => Task::none(),
            },
            LanguagesMsg::Saved { result, .. } => {
                if let Err(e) = result {
                    self.status = tr!("Could not save the language settings: {error}", error = e);
                }
                Task::none()
            }
        }
    }

    /// Use what is typed in `field`: the choice it makes, made.
    fn use_draft(&mut self, field: LangField) -> Task<Message> {
        let text = self.proj.lang_drafts.field(field).trim().to_string();
        let mut next = self.proj.lang_choices.clone();
        match field {
            LangField::PythonPath => {
                if text.is_empty() {
                    return Task::none();
                }
                next.python = Some(text);
            }
            LangField::RustTarget => {
                let rust = next.rust.get_or_insert_with(RustChoices::default);
                rust.target = (!text.is_empty()).then_some(text);
            }
            LangField::GoTag => {
                let go = next.go.get_or_insert_with(GoChoices::default);
                for tag in text
                    .split([',', ' '])
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    if !lsp::langenv::is_go_tag(tag) {
                        self.status = tr!(
                            "{tag} is not a build tag: use only letters, digits, _ and .",
                            tag = format!("{tag:?}")
                        );
                        return Task::none();
                    }
                    if !go.tags.iter().any(|t| t == tag) {
                        go.tags.push(tag.to_string());
                    }
                }
            }
            LangField::CDatabase => {
                let c = next.c.get_or_insert_with(CChoices::default);
                c.compile_commands = (!text.is_empty()).then_some(text);
            }
            LangField::CFlags => {
                let c = next.c.get_or_insert_with(CChoices::default);
                c.flags = split_args(&text);
            }
            LangField::TypeScriptPath => {
                if text.is_empty() {
                    return Task::none();
                }
                next.typescript = Some(text);
            }
            LangField::ZigPath => {
                if text.is_empty() {
                    return Task::none();
                }
                next.zig = Some(text);
            }
            LangField::JavaHome | LangField::PhpVersion | LangField::CSharpSolution => {
                if text.is_empty() {
                    return Task::none();
                }
                *match field {
                    LangField::JavaHome => &mut next.java,
                    LangField::PhpVersion => &mut next.php,
                    _ => &mut next.csharp,
                } = Some(text);
            }
            LangField::ServerCommand | LangField::ServerArgs => {
                let Some(language) = self.proj.lang_drafts.server_language.clone() else {
                    self.status = tr!("Pick the language the server program is for").into();
                    return Task::none();
                };
                let servers = next.servers.get_or_insert_with(Default::default);
                let command = self.proj.lang_drafts.server_command.trim().to_string();
                if command.is_empty() {
                    servers.remove(&language);
                } else {
                    let args = self.proj.lang_drafts.server_args.trim();
                    servers.insert(
                        language,
                        ServerChoice {
                            command,
                            args: (!args.is_empty()).then(|| split_args(args)),
                        },
                    );
                }
                self.proj.lang_drafts.server_command.clear();
                self.proj.lang_drafts.server_args.clear();
                self.proj.lang_drafts.server_language = None;
                return self.choose(next);
            }
        }
        self.proj.lang_drafts.field_mut(field).clear();
        self.choose(next)
    }

    /// List what there is to choose from for the open project's languages,
    /// where the project is: here, off the UI thread, or on its remote host.
    /// Lands as `LanguagesMsg::Listed`.
    pub(crate) fn refresh_language_options(&mut self) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let languages = self.proj.project_languages.clone();
        // Nothing the view would show a list for.
        if languages.is_empty() && self.proj.lang_choices.python.is_none() {
            return Task::none();
        }
        let choices = self.proj.lang_choices.clone();
        let stamp = self.stamp();
        let listed = move |result| {
            Message::Languages(LanguagesMsg::Listed {
                stamp: stamp.clone(),
                result,
            })
        };
        if self.connection.is_remote() {
            let ai = self.ai_client();
            return Task::perform(
                async move {
                    match ai
                        .request(clew_protocol::Request::LanguageOptions { languages, choices })
                        .await?
                    {
                        clew_protocol::Event::LanguageOptions { options, .. } => Ok(options),
                        other => Err(format!(
                            "unexpected reply to LanguageOptions: {}",
                            crate::app::rpc::event_name(&other)
                        )),
                    }
                },
                listed,
            );
        }
        // The project's `lsp.toml` as loaded with its state, approved or
        // not: unapproved, it is still what the servers start with once the
        // user allows it — and what a choice wins over.
        let config = self.proj.lsp_config.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    lsp::langopts::language_options(&root, &languages, &choices, Some(&config))
                })
                .await
                .map_err(|_| tr!("listing the language options failed").to_string())
            },
            listed,
        )
    }

    /// Make `next` the open project's choices: saved for the project, off
    /// the UI thread, and taken by each language's server whose choices
    /// changed — restarted now if one is running — and by the next debug
    /// session. A Python choice changes what is in effect, so the Settings
    /// view lists it again.
    fn choose(&mut self, next: LanguageChoices) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let next = normalized(next);
        if next == self.proj.lang_choices {
            return Task::none();
        }
        let changed = changed_languages(&self.proj.lang_choices, &next);
        let python_changed = self.proj.lang_choices.python != next.python;
        let build_changed = self.proj.lang_choices.rust != next.rust
            || self.proj.lang_choices.go != next.go
            || self.proj.lang_choices.c != next.c;
        self.proj.lang_choices = next.clone();
        let host = self.connection.approval_host().map(str::to_string);
        let stamp = self.stamp();
        // A server program of the user's takes nothing to set up; clew's
        // own again may.
        let look = self.look_for_missing_setup();
        let mut tasks = vec![
            look,
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        lsp::choices::save(host.as_deref(), &root, &next)
                    })
                    .await
                    .unwrap_or_else(|_| Err(tr!("the save failed").to_string()))
                },
                move |result| {
                    Message::Languages(LanguagesMsg::Saved {
                        stamp: stamp.clone(),
                        result,
                    })
                },
            ),
        ];
        for language in changed {
            if self.proj.link.lsp.contains_key(&language) {
                tasks.push(self.update_lsp(LspMsg::Restart(language)));
            }
        }
        // What is in effect now, for the view the choice was made in (it is
        // listed afresh whenever Settings opens).
        if python_changed && self.settings.open {
            tasks.push(self.refresh_language_options());
        }
        // What the build leaves out, and so what is dimmed, follows.
        if build_changed {
            tasks.push(self.refresh_build_facts());
        }
        Task::batch(tasks)
    }

    /// Read what the open project's build leaves out with the user's
    /// choices, where the project is: here, off the UI thread (its
    /// `lsp.toml` read afresh with it), or on its remote host. Lands as
    /// `LanguagesMsg::FactsLoaded`, which re-dims the open files.
    pub(crate) fn refresh_build_facts(&mut self) -> Task<Message> {
        let Some(project) = self.proj.project.as_ref() else {
            return Task::none();
        };
        let root = project.root.clone();
        let choices = self.proj.lang_choices.clone();
        let stamp = self.stamp();
        let loaded = move |result| {
            Message::Languages(LanguagesMsg::FactsLoaded {
                stamp: stamp.clone(),
                result,
            })
        };
        if self.connection.is_remote() {
            let ai = self.ai_client();
            return Task::perform(
                async move {
                    match ai
                        .request(clew_protocol::Request::BuildFacts { choices })
                        .await?
                    {
                        clew_protocol::Event::BuildFacts { facts, .. } => Ok(facts),
                        other => Err(format!(
                            "unexpected reply to BuildFacts: {}",
                            crate::app::rpc::event_name(&other)
                        )),
                    }
                },
                loaded,
            );
        }
        let crates =
            clew_core::buildfacts::crate_dirs(project.files.iter().map(|f| f.rel.as_str()));
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let config = lsp::config::ProjectLspConfig::load(&root).ok();
                    clew_core::buildfacts::read(&root, &crates, &choices, config.as_ref())
                })
                .await
                .map_err(|_| tr!("reading the project's build settings failed").to_string())
            },
            loaded,
        )
    }

    /// The lines of `source` (the file `abs`, in `lang`) the build leaves
    /// out, for the reading target and what is known of the project's build.
    pub(crate) fn inactive_for(&self, source: &str, lang: &str, abs: &Path) -> HashSet<usize> {
        inactive_of(
            source,
            lang,
            abs,
            self.proj.project.as_ref().map(|p| p.root.as_path()),
            &self.proj.reading_target,
            &self.proj.build_facts,
        )
    }

    /// Evaluate the dimming of every open pane again: the reading target or
    /// what the build leaves out changed.
    pub(crate) fn redim_open_panes(&mut self) {
        let target = self.proj.reading_target.clone();
        let facts = self.proj.build_facts.clone();
        let root = self.proj.project.as_ref().map(|p| p.root.clone());
        for v in self.proj.panes.iter_mut().flatten() {
            if let Some(lang) = v.lang_key {
                let src = v.source.clone();
                v.set_inactive_lines(inactive_of(
                    &src,
                    lang,
                    &v.abs.clone(),
                    root.as_deref(),
                    &target,
                    &facts,
                ));
            }
        }
    }
}

/// The lines of `source` (the file `abs` of the project at `root`, in
/// `lang`) the build leaves out for `target`, with what `facts` say.
pub(crate) fn inactive_of(
    source: &str,
    lang: &str,
    abs: &Path,
    root: Option<&Path>,
    target: &inactive::Target,
    facts: &clew_protocol::BuildFacts,
) -> HashSet<usize> {
    let rel = root
        .and_then(|root| abs.strip_prefix(root).ok())
        .map(|rel| rel.to_string_lossy().replace('\\', "/"));
    inactive::inactive_lines_in(source, lang, target, facts, rel.as_deref())
}

/// Whether the project file `rel` decides what its build leaves out: a Cargo
/// manifest, a compilation database or flags file, the project's `lsp.toml`.
pub(crate) fn is_build_manifest(rel: &str) -> bool {
    let name = rel.rsplit(['/', '\\']).next().unwrap_or(rel);
    matches!(
        name,
        "Cargo.toml" | "compile_commands.json" | "compile_flags.txt"
    ) || rel.replace('\\', "/") == ".clew/lsp.toml"
}

/// `choices` with a language's choices that choose nothing taken for none,
/// so "nothing chosen" has one spelling — the one saved as no entry at all.
fn normalized(mut choices: LanguageChoices) -> LanguageChoices {
    if choices.rust.as_ref() == Some(&RustChoices::default()) {
        choices.rust = None;
    }
    if choices.go.as_ref() == Some(&GoChoices::default()) {
        choices.go = None;
    }
    if choices.c.as_ref() == Some(&CChoices::default()) {
        choices.c = None;
    }
    if choices.servers.as_ref().is_some_and(|s| s.is_empty()) {
        choices.servers = None;
    }
    for path in [
        &mut choices.typescript,
        &mut choices.zig,
        &mut choices.java,
        &mut choices.php,
        &mut choices.csharp,
    ] {
        if path.as_deref().is_some_and(|p| p.trim().is_empty()) {
            *path = None;
        }
    }
    choices
}

/// The languages whose servers are set up differently under `next` than
/// under `before`: each is restarted.
fn changed_languages(before: &LanguageChoices, next: &LanguageChoices) -> Vec<String> {
    let mut changed = Vec::new();
    if before.python != next.python {
        changed.push("python".to_string());
    }
    if before.rust != next.rust {
        changed.push("rust".to_string());
    }
    if before.go != next.go {
        changed.push("go".to_string());
    }
    if before.c != next.c {
        changed.extend(["c".to_string(), "cpp".to_string()]);
    }
    if before.typescript != next.typescript {
        changed.extend(["typescript", "tsx", "javascript"].map(String::from));
    }
    if before.zig != next.zig {
        changed.push("zig".to_string());
    }
    for (language, a, b) in [
        ("java", &before.java, &next.java),
        ("php", &before.php, &next.php),
        ("csharp", &before.csharp, &next.csharp),
    ] {
        if a != b {
            changed.push(language.to_string());
        }
    }
    let none = Default::default();
    let (a, b) = (
        before.servers.as_ref().unwrap_or(&none),
        next.servers.as_ref().unwrap_or(&none),
    );
    for language in a.keys().chain(b.keys()) {
        if a.get(language) != b.get(language) && !changed.contains(language) {
            changed.push(language.clone());
        }
    }
    changed
}

/// Arguments typed on one line: split at spaces, a quoted run (`'…'` or
/// `"…"`) kept whole, a backslash keeping the character after it.
pub(crate) fn split_args(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, c) if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (Some(q), c) if c == q => quote = None,
            (_, '\\') if quote != Some('\'') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                started = true;
            }
            (_, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

/// `args` on one line, as [`split_args`] reads them back: one with a space,
/// a quote or a backslash in it quoted.
pub(crate) fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty() && !a.contains([' ', '\t', '\'', '"', '\\']) {
                a.clone()
            } else {
                format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What Browse… looks for on `field`'s behalf — a folder or a file — and
/// the dialog's title; `None` for a field no file names (a version, flags).
/// Every one was a file picker titled "Choose an SSH private key", and a
/// JDK's home folder could not be picked at all.
pub(crate) fn browse_for(field: LangField) -> Option<(bool, &'static str)> {
    Some(match field {
        LangField::PythonPath => (false, tr!("Choose a Python interpreter")),
        LangField::CDatabase => (true, tr!("Choose the folder with compile_commands.json")),
        LangField::TypeScriptPath => (false, tr!("Choose tsserver.js")),
        LangField::ZigPath => (false, tr!("Choose the zig program")),
        LangField::JavaHome => (true, tr!("Choose a JDK's home folder")),
        LangField::CSharpSolution => (false, tr!("Choose a .sln or .slnx file")),
        LangField::ServerCommand => (false, tr!("Choose a language server program")),
        LangField::RustTarget
        | LangField::GoTag
        | LangField::CFlags
        | LangField::PhpVersion
        | LangField::ServerArgs => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Typed arguments split as a shell would split simple ones, and a list
    /// shown on one line reads back as itself.
    #[test]
    fn typed_arguments_split_and_join_back() {
        assert_eq!(
            split_args(r#"-DNAME="a b" -I'inc dir'  -std=c++20 \"x"#),
            ["-DNAME=a b", "-Iinc dir", "-std=c++20", "\"x"]
        );
        assert_eq!(split_args("  "), Vec::<String>::new());
        assert_eq!(split_args(r#""""#), [""]);
        for args in [
            vec!["--stdio".to_string()],
            vec!["a b".into(), "c\"d".into(), "e\\f".into(), "".into()],
        ] {
            assert_eq!(split_args(&join_args(&args)), args);
        }
    }

    /// Only the languages whose servers a change sets up differently are
    /// restarted — C's choices set up both of clangd's languages.
    #[test]
    fn a_change_restarts_only_the_languages_it_concerns() {
        let before = LanguageChoices {
            rust: Some(RustChoices {
                all_features: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut next = before.clone();
        next.c = Some(CChoices {
            flags: vec!["-DX".into()],
            ..Default::default()
        });
        next.servers = Some(
            [(
                "lean".to_string(),
                ServerChoice {
                    command: "/e/lake".into(),
                    args: None,
                },
            )]
            .into(),
        );
        assert_eq!(changed_languages(&before, &next), ["c", "cpp", "lean"]);
        let mut ts = before.clone();
        ts.typescript = Some("/p/node_modules/typescript".into());
        ts.zig = Some("zig".into());
        assert_eq!(
            changed_languages(&before, &ts),
            ["typescript", "tsx", "javascript", "zig"]
        );
        assert!(changed_languages(&next, &next).is_empty());
        for (language, set) in [
            (
                "java",
                (|c: &mut LanguageChoices| c.java = Some("/jdk".into())) as fn(&mut _),
            ),
            ("php", |c| c.php = Some("8.2".into())),
            ("csharp", |c| c.csharp = Some("/p/a.sln".into())),
        ] {
            let mut one = before.clone();
            set(&mut one);
            assert_eq!(changed_languages(&before, &one), [language]);
        }
    }

    /// A language's choices that choose nothing are no choices at all.
    #[test]
    fn choices_of_nothing_are_none() {
        let empty = LanguageChoices {
            rust: Some(RustChoices::default()),
            go: Some(GoChoices::default()),
            c: Some(CChoices::default()),
            servers: Some(Default::default()),
            typescript: Some(" ".into()),
            zig: Some(String::new()),
            java: Some(" ".into()),
            php: Some(String::new()),
            csharp: Some("\t".into()),
            ..Default::default()
        };
        assert_eq!(normalized(empty), LanguageChoices::default());
        let kept = LanguageChoices {
            java: Some("/jdk".into()),
            php: Some("8.2".into()),
            csharp: Some("a.sln".into()),
            ..Default::default()
        };
        let kept = normalized(kept);
        assert_eq!(kept.java.as_deref(), Some("/jdk"));
        assert_eq!(kept.php.as_deref(), Some("8.2"));
        assert_eq!(kept.csharp.as_deref(), Some("a.sln"));
    }
}
