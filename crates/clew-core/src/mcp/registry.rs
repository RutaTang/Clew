//! The official MCP registry (registry.modelcontextprotocol.io): searching
//! it, and turning an entry into what Clew's own MCP config holds — a
//! command to start (`npx`, `uvx`, `docker`, `dnx`) with its environment,
//! or a URL with its headers.
//!
//! The registry is a list of what publishers say about their servers; it
//! vets nothing. So an entry is only ever turned into a *proposal*: the
//! Settings form is filled with it, and the user reads the command, fills
//! in the secrets, and adds it — or does not. Nothing here starts or saves
//! a server.

use std::time::Duration;

use serde::Deserialize;

/// The registry's API.
pub const BASE: &str = "https://registry.modelcontextprotocol.io/v0.1/servers";

/// Entries one search asks for.
pub const PAGE: usize = 30;

/// Largest answer read.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// How long a search may take.
const DEADLINE: Duration = Duration::from_secs(20);

/// The URL of a search for `query` — the latest version of each server.
pub fn search_url(query: &str) -> String {
    let mut q = String::new();
    for b in query.trim().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                q.push(b as char)
            }
            b' ' => q.push('+'),
            _ => q.push_str(&format!("%{b:02X}")),
        }
    }
    format!("{BASE}?search={q}&limit={PAGE}&version=latest")
}

/// Search the registry (blocking: run it off the UI thread).
pub fn search(query: &str) -> Result<Vec<Entry>, String> {
    let body = crate::net::get(
        &search_url(query),
        &[("Accept", "application/json")],
        crate::net::Limits {
            max_bytes: MAX_BYTES,
            deadline: DEADLINE,
        },
    )?;
    let text = String::from_utf8(body).map_err(|_| "the registry's answer is not text")?;
    parse(&text)
}

/// One server the registry lists, as Clew would add it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The registry's name: `io.github.owner/server`.
    pub name: String,
    pub description: String,
    pub version: String,
    /// Where its source is, when it says.
    pub repository: Option<String>,
    /// How it would run.
    pub install: Install,
}

impl Entry {
    /// A name Clew's config accepts, from the registry's: its last part.
    pub fn short_name(&self) -> String {
        let last = self.name.rsplit(['/', '.']).next().unwrap_or(&self.name);
        let last = last
            .strip_prefix("mcp-server-")
            .or_else(|| last.strip_suffix("-mcp-server"))
            .or_else(|| last.strip_prefix("mcp-"))
            .or_else(|| last.strip_suffix("-mcp"))
            .filter(|s| !s.is_empty())
            .unwrap_or(last);
        super::config::clean_name(last)
    }
}

/// How an entry runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// A package Clew starts: `program args…`, with these variables.
    Stdio {
        /// What the package is: "npm", "pypi", "oci", "nuget".
        kind: String,
        program: String,
        args: Vec<String>,
        env: Vec<Var>,
    },
    /// A server already running somewhere, at `url`.
    Remote { url: String, headers: Vec<Var> },
    /// Nothing Clew can run (a bundle, a package served over HTTP): why.
    Unsupported(String),
}

/// An environment variable or a header the server reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Var {
    pub name: String,
    pub description: String,
    /// Its value as the registry gives it: a default, or a template such
    /// as `Bearer {api_key}` with the part to fill in braces; empty for
    /// none.
    pub value: String,
    pub required: bool,
    pub secret: bool,
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    servers: Vec<Listed>,
}

#[derive(Deserialize)]
struct Listed {
    server: RawServer,
    #[serde(default, rename = "_meta")]
    meta: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawServer {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    repository: Option<RawRepo>,
    #[serde(default)]
    packages: Vec<RawPackage>,
    #[serde(default)]
    remotes: Vec<RawRemote>,
}

#[derive(Deserialize)]
struct RawRepo {
    #[serde(default)]
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPackage {
    #[serde(default)]
    registry_type: String,
    #[serde(default)]
    identifier: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    runtime_hint: Option<String>,
    #[serde(default)]
    transport: Option<RawTransport>,
    #[serde(default)]
    runtime_arguments: Vec<RawArg>,
    #[serde(default)]
    package_arguments: Vec<RawArg>,
    #[serde(default)]
    environment_variables: Vec<RawVar>,
}

#[derive(Deserialize)]
struct RawTransport {
    #[serde(default, rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawArg {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    value_hint: Option<String>,
    #[serde(default)]
    default: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVar {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    is_required: bool,
    #[serde(default)]
    is_secret: bool,
}

#[derive(Deserialize)]
struct RawRemote {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    headers: Vec<RawVar>,
}

/// The entries in a search's answer, each server once (its latest version:
/// the registry marks it, and the first listed stands when none is
/// marked), in the registry's order.
pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let page: Page =
        serde_json::from_str(text).map_err(|e| format!("the registry's answer: {e}"))?;
    let mut out: Vec<Entry> = Vec::new();
    for listed in page.servers {
        let latest = listed
            .meta
            .as_ref()
            .and_then(|m| m.get("io.modelcontextprotocol.registry/official"))
            .and_then(|o| o.get("isLatest"))
            .and_then(serde_json::Value::as_bool);
        let entry = entry(listed.server);
        match out.iter().position(|e| e.name == entry.name) {
            Some(at) if latest == Some(true) => out[at] = entry,
            Some(_) => {}
            None => out.push(entry),
        }
    }
    Ok(out)
}

fn entry(server: RawServer) -> Entry {
    Entry {
        install: install(&server),
        name: server.name,
        description: server.description.trim().to_string(),
        version: server.version,
        repository: server
            .repository
            .map(|r| r.url)
            .filter(|u| u.starts_with("https://")),
    }
}

fn var(v: &RawVar) -> Var {
    Var {
        name: v.name.trim().to_string(),
        description: v.description.trim().to_string(),
        value: v
            .value
            .clone()
            .or_else(|| v.default.clone())
            .unwrap_or_default(),
        required: v.is_required,
        secret: v.is_secret,
    }
}

/// An argument's words: a named one's name and value, a positional one's
/// value — a value not given shown as `<what it is>`, to be filled in.
fn arg_words(arg: &RawArg) -> Vec<String> {
    let value = arg
        .value
        .clone()
        .or_else(|| arg.default.clone())
        .or_else(|| arg.value_hint.as_ref().map(|h| format!("<{h}>")));
    match arg.kind.as_str() {
        "named" => {
            let mut words: Vec<String> = arg.name.iter().cloned().collect();
            words.extend(value);
            words
        }
        _ => value.into_iter().collect(),
    }
}

/// How `server` would run: its first package Clew can start over stdio,
/// else its first remote; why not, when neither.
fn install(server: &RawServer) -> Install {
    let mut why = None;
    for p in &server.packages {
        let transport = p.transport.as_ref().map_or("stdio", |t| t.kind.as_str());
        if transport != "stdio" {
            why = Some(format!("its package is served over {transport}"));
            continue;
        }
        let at_version = |sep: &str| match p.version.as_deref().filter(|v| !v.is_empty()) {
            Some(v) => format!("{}{sep}{v}", p.identifier),
            None => p.identifier.clone(),
        };
        let env: Vec<Var> = p.environment_variables.iter().map(var).collect();
        let runtime: Vec<String> = p.runtime_arguments.iter().flat_map(arg_words).collect();
        let package: Vec<String> = p.package_arguments.iter().flat_map(arg_words).collect();
        let (program, mut args) = match p.registry_type.as_str() {
            "npm" => {
                let program = p.runtime_hint.clone().unwrap_or_else(|| "npx".into());
                let mut args = runtime;
                if program == "npx" && !args.iter().any(|a| a == "-y" || a == "--yes") {
                    args.insert(0, "-y".into());
                }
                args.push(at_version("@"));
                (program, args)
            }
            "pypi" => {
                let mut args = runtime;
                args.push(at_version("=="));
                (p.runtime_hint.clone().unwrap_or_else(|| "uvx".into()), args)
            }
            "nuget" => {
                let mut args = runtime;
                args.push(at_version("@"));
                if !args.iter().any(|a| a == "--yes") {
                    args.push("--yes".into());
                }
                (p.runtime_hint.clone().unwrap_or_else(|| "dnx".into()), args)
            }
            "oci" => {
                let mut args: Vec<String> = vec!["run".into(), "-i".into(), "--rm".into()];
                // The variables go into the container by name, from Clew's.
                for v in &env {
                    args.push("-e".into());
                    args.push(v.name.clone());
                }
                args.extend(runtime);
                let image = match p.version.as_deref().filter(|v| !v.is_empty()) {
                    Some(v) if !p.identifier.contains(':') => format!("{}:{v}", p.identifier),
                    _ => p.identifier.clone(),
                };
                args.push(image);
                ("docker".to_string(), args)
            }
            other => {
                why = Some(format!("it is a {other} package"));
                continue;
            }
        };
        args.extend(package);
        return Install::Stdio {
            kind: p.registry_type.clone(),
            program,
            args,
            env,
        };
    }
    for r in &server.remotes {
        if matches!(r.kind.as_str(), "streamable-http" | "sse") && r.url.starts_with("https://") {
            return Install::Remote {
                url: r.url.clone(),
                headers: r.headers.iter().map(var).collect(),
            };
        }
    }
    Install::Unsupported(why.unwrap_or_else(|| "it lists no package or remote".into()))
}

/// What the Settings form's fields get from `entry`: the server's name,
/// the command line or URL, and its variables or headers as `KEY=value`
/// (`Name: value`) lines — what has no value left empty to be filled in.
/// `None` for an entry Clew cannot run.
pub fn form_fields(entry: &Entry) -> Option<(String, String, String)> {
    let quote = |w: &str| {
        if w.is_empty() || w.contains([' ', '"', '\'', '\\']) {
            format!("\"{}\"", w.replace('\\', "\\\\").replace('"', "\\\""))
        } else {
            w.to_string()
        }
    };
    match &entry.install {
        Install::Stdio {
            program, args, env, ..
        } => {
            let mut line = quote(program);
            for a in args {
                line.push(' ');
                line.push_str(&quote(a));
            }
            let extra: Vec<String> = env
                .iter()
                .filter(|v| v.required || !v.value.is_empty())
                .map(|v| format!("{}={}", v.name, v.value))
                .collect();
            Some((entry.short_name(), line, extra.join("\n")))
        }
        Install::Remote { url, headers } => {
            let extra: Vec<String> = headers
                .iter()
                .map(|h| format!("{}: {}", h.name, h.value))
                .collect();
            Some((entry.short_name(), url.clone(), extra.join("\n")))
        }
        Install::Unsupported(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = r#"{"servers": [
      {"server": {"name": "io.github.acme/mcp-server-notes", "version": "1.1.0",
        "packages": [{"registryType": "npm", "identifier": "@acme/notes", "version": "1.1.0"}]},
       "_meta": {"io.modelcontextprotocol.registry/official": {"isLatest": false}}},
      {"server": {"name": "io.github.acme/mcp-server-notes", "description": " Notes. ",
        "version": "1.2.0", "repository": {"url": "https://github.com/acme/notes"},
        "packages": [{"registryType": "npm", "identifier": "@acme/notes", "version": "1.2.0",
          "transport": {"type": "stdio"},
          "packageArguments": [{"type": "named", "name": "--root", "valueHint": "dir"}],
          "environmentVariables": [
            {"name": "NOTES_TOKEN", "description": "API token", "isRequired": true, "isSecret": true},
            {"name": "NOTES_LEVEL", "default": "info"},
            {"name": "NOTES_OPTIONAL"}]}]},
       "_meta": {"io.modelcontextprotocol.registry/official": {"isLatest": true}}},
      {"server": {"name": "com.example/py-tool", "version": "0.3.0",
        "packages": [{"registryType": "pypi", "identifier": "py-tool", "version": "0.3.0",
          "runtimeHint": "uvx"}]}},
      {"server": {"name": "com.example/boxed", "version": "2",
        "packages": [{"registryType": "oci", "identifier": "docker.io/ex/boxed", "version": "2",
          "environmentVariables": [{"name": "KEY", "isRequired": true}]}]}},
      {"server": {"name": "ai.smithery/remote-one", "version": "1",
        "remotes": [{"type": "streamable-http", "url": "https://x.example/mcp",
          "headers": [{"name": "Authorization", "value": "Bearer {api_key}",
                       "isRequired": true, "isSecret": true}]}]}},
      {"server": {"name": "com.example/bundle", "version": "1",
        "packages": [{"registryType": "mcpb", "identifier": "https://x/b.mcpb"}]}}
    ]}"#;

    /// Each server once, at its latest version; a package becomes the
    /// command its runtime starts it with; a remote, its URL and headers;
    /// a bundle Clew cannot run says why.
    #[test]
    fn entries_become_what_clew_would_run() {
        let entries = parse(ANSWER).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "io.github.acme/mcp-server-notes",
                "com.example/py-tool",
                "com.example/boxed",
                "ai.smithery/remote-one",
                "com.example/bundle"
            ]
        );
        let notes = &entries[0];
        assert_eq!(notes.version, "1.2.0", "the latest");
        assert_eq!(notes.description, "Notes.");
        assert_eq!(
            notes.repository.as_deref(),
            Some("https://github.com/acme/notes")
        );
        assert_eq!(notes.short_name(), "notes");
        let Install::Stdio {
            program, args, env, ..
        } = &notes.install
        else {
            panic!("{:?}", notes.install)
        };
        assert_eq!(program, "npx");
        assert_eq!(args, &["-y", "@acme/notes@1.2.0", "--root", "<dir>"]);
        assert!(env[0].required && env[0].secret);
        assert_eq!(env[1].value, "info");

        let Install::Stdio { program, args, .. } = &entries[1].install else {
            panic!()
        };
        assert_eq!(
            (program.as_str(), args.as_slice()),
            ("uvx", &["py-tool==0.3.0".to_string()][..])
        );
        let Install::Stdio { program, args, .. } = &entries[2].install else {
            panic!()
        };
        assert_eq!(program, "docker");
        assert_eq!(
            args,
            &["run", "-i", "--rm", "-e", "KEY", "docker.io/ex/boxed:2"]
        );
        let Install::Remote { url, headers } = &entries[3].install else {
            panic!()
        };
        assert_eq!(url, "https://x.example/mcp");
        assert_eq!(headers[0].value, "Bearer {api_key}");
        assert!(matches!(&entries[4].install, Install::Unsupported(why) if why.contains("mcpb")));
    }

    /// The form gets the name, the command line (quoted where a word needs
    /// it) or URL, and the variables to fill in: the required ones, empty,
    /// and those with a value; an entry Clew cannot run fills nothing.
    #[test]
    fn an_entry_fills_the_add_form() {
        let entries = parse(ANSWER).unwrap();
        let (name, line, extra) = form_fields(&entries[0]).unwrap();
        assert_eq!(name, "notes");
        assert_eq!(line, "npx -y @acme/notes@1.2.0 --root <dir>");
        assert_eq!(extra, "NOTES_TOKEN=\nNOTES_LEVEL=info");
        let (_, url, headers) = form_fields(&entries[3]).unwrap();
        assert_eq!(url, "https://x.example/mcp");
        assert_eq!(headers, "Authorization: Bearer {api_key}");
        assert!(form_fields(&entries[4]).is_none());
    }

    /// A query is sent encoded; the latest version of each is asked for.
    #[test]
    fn a_search_is_encoded() {
        assert_eq!(
            search_url(" git hub/π "),
            format!("{BASE}?search=git+hub%2F%CF%80&limit={PAGE}&version=latest")
        );
    }

    /// The real registry answers, and what it lists is read: run by hand
    /// (`--ignored`), as it needs the network.
    #[test]
    #[ignore = "needs the network"]
    fn the_real_registry_is_searched() {
        let found = search("filesystem").expect("the registry answers");
        assert!(!found.is_empty());
        let runnable = found.iter().filter(|e| form_fields(e).is_some()).count();
        assert!(runnable > 0, "{found:#?}");
        for e in &found {
            assert!(
                super::super::config::valid_name(&e.short_name()),
                "{}",
                e.name
            );
        }
    }
}
