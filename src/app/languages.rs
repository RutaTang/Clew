//! The settings' languages of the open project: what its language servers
//! are given, and the Python interpreter the user picks for it.
//!
//! The choices are the user's, for this project on this machine
//! (`clew_core::lsp::choices`, in clew's data directory — never the
//! project's `.clew/`, which is the team's): read when the project opens,
//! before any language server starts (`App::ensure_lsp` waits for them),
//! sent with every server and debugger set up for it, and saved the moment
//! one is made. The Python interpreters to pick from are found where the
//! project is — this machine, or the remote host it lives on — by looking,
//! never by running one.

use crate::app::prelude::*;
use crate::*;

use clew_protocol::LanguageChoices;

impl App {
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

    pub(crate) fn update_languages(&mut self, message: LanguagesMsg) -> Task<Message> {
        match message {
            LanguagesMsg::ChoicesLoaded { choices, .. } => {
                self.proj.lang_choices = choices;
                self.proj.inflight.choices_loading = false;
                // The servers asked for meanwhile — each waits on, if the
                // project's other state is still loading, that instead.
                let mut tasks = Vec::new();
                for language in std::mem::take(&mut self.proj.inflight.deferred_lsp) {
                    tasks.push(self.ensure_lsp(&language));
                }
                Task::batch(tasks)
            }
            LanguagesMsg::Refresh => self.refresh_python_envs(),
            LanguagesMsg::Listed { result, .. } => {
                self.proj.python_envs = Some(result);
                Task::none()
            }
            LanguagesMsg::ChoosePython(python) => self.choose_python(python),
            LanguagesMsg::PathDraft(text) => {
                self.proj.python_path_draft = text;
                Task::none()
            }
            LanguagesMsg::UsePath => {
                let path = self.proj.python_path_draft.trim().to_string();
                if path.is_empty() {
                    return Task::none();
                }
                self.choose_python(Some(path))
            }
            LanguagesMsg::Browse => {
                // The dialog shows this machine's files: a remote project's
                // interpreter is a path on its host, typed or picked from
                // the list.
                if self.connection.is_remote() || self.proj.project.is_none() {
                    return Task::none();
                }
                let stamp = self.stamp();
                crate::app::tasks::pick_file(self.main_window).map(move |path| {
                    Message::Languages(LanguagesMsg::Browsed {
                        stamp: stamp.clone(),
                        path,
                    })
                })
            }
            LanguagesMsg::Browsed { path, .. } => match path {
                Some(path) => self.choose_python(Some(path.to_string_lossy().into_owned())),
                None => Task::none(),
            },
            LanguagesMsg::Saved { result, .. } => {
                if let Err(e) = result {
                    self.status = format!("The Python interpreter was not saved: {e}");
                }
                Task::none()
            }
        }
    }

    /// List the open project's Python interpreters and the one in effect,
    /// where the project is: here, off the UI thread, or on its remote host.
    /// Lands as `LanguagesMsg::Listed`.
    pub(crate) fn refresh_python_envs(&mut self) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        // Only for a project the view shows a Python section for.
        if !self.proj.project_languages.iter().any(|l| l == "python")
            && self.proj.lang_choices.python.is_none()
        {
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
                        .request(clew_protocol::Request::PythonEnvironments { choices })
                        .await?
                    {
                        clew_protocol::Event::PythonEnvironments { environments, .. } => {
                            Ok(environments)
                        }
                        other => Err(format!(
                            "unexpected reply to PythonEnvironments: {}",
                            crate::app::rpc::event_name(&other)
                        )),
                    }
                },
                listed,
            );
        }
        // What the project's `lsp.toml` names, approved or not: unapproved,
        // it is still what the server starts with once the user allows it.
        let lsp_toml = self
            .proj
            .lsp_config
            .resolve("python")
            .and_then(|server| server.init_options);
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    lsp::langenv::python_environments(&root, &choices, lsp_toml.as_ref())
                })
                .await
                .map_err(|_| "listing the Python environments failed".to_string())
            },
            listed,
        )
    }

    /// Make `python` the open project's interpreter (`None`: let clew find
    /// one): saved for the project, off the UI thread, and taken by its
    /// Python server — restarted now if one is running — and by the next
    /// debug session; the Settings view lists what is in effect again.
    fn choose_python(&mut self, python: Option<String>) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let choices = LanguageChoices { python };
        if choices == self.proj.lang_choices {
            return Task::none();
        }
        self.proj.lang_choices = choices.clone();
        self.proj.python_path_draft.clear();
        let host = self.connection.approval_host().map(str::to_string);
        let stamp = self.stamp();
        let save = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    lsp::choices::save(host.as_deref(), &root, &choices)
                })
                .await
                .unwrap_or_else(|_| Err("the save failed".to_string()))
            },
            move |result| {
                Message::Languages(LanguagesMsg::Saved {
                    stamp: stamp.clone(),
                    result,
                })
            },
        );
        let restart = if self.proj.link.lsp.contains_key("python") {
            self.update_lsp(LspMsg::Restart("python".into()))
        } else {
            Task::none()
        };
        // What is in effect now, for the view the choice was made in (it is
        // listed afresh whenever Settings opens).
        let listed = if self.settings.open {
            self.refresh_python_envs()
        } else {
            Task::none()
        };
        Task::batch([save, restart, listed])
    }
}
