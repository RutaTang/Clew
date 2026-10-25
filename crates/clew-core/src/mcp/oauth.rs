//! Signing in to a remote MCP server: the authorization the MCP specification
//! defines for its HTTP transport — OAuth 2.1 with PKCE, the server's
//! authorization server found through its protected resource metadata.
//!
//! [`sign_in`] runs the whole flow, as the specification orders it:
//!
//! 1. **Protected resource metadata** (RFC 9728): the URL a 401's
//!    `WWW-Authenticate: Bearer resource_metadata=…` names, else the
//!    well-known documents at the endpoint's path and at its root. It names
//!    the authorization server.
//! 2. **Authorization server metadata** (RFC 8414, or OpenID Connect
//!    discovery), from the well-known locations in the specified order, its
//!    `issuer` checked against the server it was asked of.
//! 3. **A client id**: one the user registered themselves and configured, or
//!    one registered dynamically (RFC 7591), as a native app whose only
//!    redirect is a loopback address (RFC 8252). A registration belongs to
//!    the authorization server that issued it and is never used with another.
//! 4. **The authorization request**, opened in the user's browser: the code
//!    flow with a PKCE S256 challenge, a `state`, the scopes the server asked
//!    for, and the MCP server as the `resource` (RFC 8707). The browser comes
//!    back to a one-shot listener on 127.0.0.1, whose answer is checked —
//!    its `state`, and its `iss` against the recorded issuer (RFC 9207) —
//!    before the code goes anywhere.
//! 5. **The token request**, with the code verifier and the resource.
//!
//! The tokens are kept in `mcp-oauth.json` in clew's data directory, readable
//! by the user alone, keyed by the server's URL. [`bearer`] hands the access
//! token to the transport, refreshing it when it has expired; a 401 that a
//! refresh cannot cure means signing in again.
//!
//! Every endpoint of the flow must be https — a token over plain http would
//! cross the network in the clear — except on this machine's loopback.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long the user has to finish signing in in the browser.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

/// How long one request of the flow may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest metadata or token response read.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// An access token this close to its expiry is refreshed before use.
const EXPIRY_MARGIN_SECS: u64 = 60;

/// The store's file in clew's data directory.
const STORE_FILE: &str = "mcp-oauth.json";

/// What a `WWW-Authenticate: Bearer …` challenge said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Challenge {
    /// Where the protected resource metadata is.
    pub resource_metadata: Option<String>,
    /// The scopes the request needs.
    pub scope: Option<String>,
    /// `invalid_token`, `insufficient_scope`, …
    pub error: Option<String>,
}

/// The Bearer challenge of a `WWW-Authenticate` header, if it carries one.
pub fn parse_challenge(header: &str) -> Option<Challenge> {
    let start = find_scheme(header, "bearer")?;
    let mut rest = header[start + "bearer".len()..].trim_start();
    let mut challenge = Challenge::default();
    while let Some(eq) = rest.find('=') {
        let name = rest[..eq].trim().trim_start_matches(',').trim();
        // A token with a space before its `=` is the next challenge's scheme.
        if name.is_empty() || name.contains(char::is_whitespace) {
            break;
        }
        let after = rest[eq + 1..].trim_start();
        let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let mut end = quoted.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, next)) = chars.next() {
                            value.push(next);
                        }
                    }
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    c => value.push(c),
                }
            }
            (value, &quoted[end.min(quoted.len())..])
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].trim().to_string(), &after[end..])
        };
        match name.to_ascii_lowercase().as_str() {
            "resource_metadata" => challenge.resource_metadata = Some(value),
            "scope" => challenge.scope = Some(value),
            "error" => challenge.error = Some(value),
            _ => {}
        }
        rest = tail.trim_start().trim_start_matches(',').trim_start();
    }
    Some(challenge)
}

/// Where scheme `scheme` (lower case) starts a challenge in `header`.
fn find_scheme(header: &str, scheme: &str) -> Option<usize> {
    let lower = header.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(scheme) {
        let at = from + at;
        let before_ok = at == 0 || matches!(lower.as_bytes()[at - 1], b' ' | b',');
        let after_ok = lower
            .as_bytes()
            .get(at + scheme.len())
            .is_none_or(|b| *b == b' ');
        if before_ok && after_ok {
            return Some(at);
        }
        from = at + scheme.len();
    }
    None
}

/// The server URL as the store keys it and the `resource` parameter names
/// it: no fragment, no query, no trailing slash.
pub fn canonical_resource(url: &str) -> Result<String, String> {
    let mut parsed = url::Url::parse(url.trim()).map_err(|e| format!("{url}: {e}"))?;
    parsed.set_fragment(None);
    parsed.set_query(None);
    let mut text = parsed.to_string();
    while text.ends_with('/') {
        text.pop();
    }
    Ok(text)
}

// ------------------------------------------------------------------ store

/// One server's sign-in, as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// The authorization server that issued all of this.
    pub issuer: String,
    pub token_endpoint: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// The redirect the client was registered with (dynamic registration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds; `None` when the server named no lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    servers: BTreeMap<String, Session>,
}

/// Serializes the store's read-modify-writes within this process.
static STORE_LOCK: Mutex<()> = Mutex::new(());

fn store_path() -> Option<std::path::PathBuf> {
    Some(crate::lsp::store::data_root()?.join(STORE_FILE))
}

fn read_store() -> Store {
    let Some(path) = store_path() else {
        return Store::default();
    };
    match crate::statefile::read_capped_checked(&path, MAX_RESPONSE_BYTES) {
        Ok(Some(text)) => serde_json::from_str(&text).unwrap_or_default(),
        _ => Store::default(),
    }
}

fn edit_store<T>(f: impl FnOnce(&mut Store) -> T) -> Result<T, String> {
    let _held = STORE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let path = store_path().ok_or("no data directory")?;
    let mut store = read_store();
    let out = f(&mut store);
    if let Some(dir) = path.parent() {
        crate::derived::ensure_private_dir(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&store).map_err(|e| e.to_string())?;
    crate::statefile::write_atomic_secret(&path, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(out)
}

/// The stored sign-in of the server at `url`.
pub fn session(url: &str) -> Option<Session> {
    let key = canonical_resource(url).ok()?;
    read_store().servers.get(&key).cloned()
}

/// Whether the server at `url` has a stored sign-in.
pub fn signed_in(url: &str) -> bool {
    session(url).is_some()
}

/// Forget the sign-in of the server at `url`.
pub fn sign_out(url: &str) -> Result<(), String> {
    let key = canonical_resource(url)?;
    edit_store(|store| {
        store.servers.remove(&key);
    })
}

/// Change the stored sign-in of `url` (tests).
#[cfg(test)]
pub(crate) fn edit_store_for_tests(url: &str, f: impl FnOnce(&mut Session)) -> Result<(), String> {
    let key = canonical_resource(url)?;
    edit_store(|store| {
        if let Some(session) = store.servers.get_mut(&key) {
            f(session);
        }
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The access token to send to the server at `url`: the stored one while it
/// is fresh, else a refreshed one. `None` when there is no sign-in, or it
/// has expired past refreshing. Blocking (a refresh is a request).
pub fn bearer(url: &str) -> Option<String> {
    let session = session(url)?;
    let fresh = session
        .expires_at
        .is_none_or(|at| now_secs() + EXPIRY_MARGIN_SECS < at);
    if fresh {
        return Some(session.access_token);
    }
    refresh(url).ok()
}

/// Exchange the stored refresh token of the server at `url` for a new
/// access token, and store it. Blocking.
pub fn refresh(url: &str) -> Result<String, String> {
    let key = canonical_resource(url)?;
    let session = session(url).ok_or("not signed in")?;
    let refresh_token = session
        .refresh_token
        .clone()
        .ok_or("the sign-in cannot be renewed — sign in again")?;
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.clone()),
        ("client_id", session.client_id.clone()),
        ("resource", key.clone()),
    ];
    if let Some(scope) = &session.scope {
        form.push(("scope", scope.clone()));
    }
    let tokens = token_request(
        &session.token_endpoint,
        &form,
        session.client_secret.as_deref(),
    )?;
    let mut updated = session;
    apply_tokens(&mut updated, &tokens);
    // A server that rotates refresh tokens sends a new one; one that does
    // not leaves the old one good.
    if updated.refresh_token.is_none() {
        updated.refresh_token = Some(refresh_token);
    }
    let access = updated.access_token.clone();
    edit_store(|store| {
        store.servers.insert(key, updated);
    })?;
    Ok(access)
}

// ------------------------------------------------------------------ HTTP

/// An endpoint of the flow may be https, or plain http on this machine.
fn check_endpoint(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("{url}: {e}"))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if crate::net::is_loopback_http(url) => Ok(()),
        _ => Err(format!(
            "{url}: an authorization endpoint must be https — refusing to send credentials in the clear"
        )),
    }
}

fn agent(url: &str) -> Result<(ureq::Agent, ureq::http::Uri), String> {
    check_endpoint(url)?;
    let (agent, _) = crate::llm::agent_for(url, crate::llm::Limits::streaming(REQUEST_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let uri = crate::net::request_uri(url)?;
    Ok((agent, uri))
}

fn read_json(response: ureq::http::Response<ureq::Body>) -> Result<(u16, Value), String> {
    let status = response.status().as_u16();
    let mut text = String::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_RESPONSE_BYTES)
        .read_to_string(&mut text)
        .map_err(|e| crate::net::describe_io(&e))?;
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, value))
}

/// GET a JSON document; `Ok(None)` for a 404 (or any non-success).
fn get_json(url: &str) -> Result<Option<Value>, String> {
    let (agent, uri) = agent(url)?;
    let response =
        crate::net::guarded(|| agent.get(uri).header("Accept", "application/json").call())
            .map_err(|e| crate::net::describe(&e))?;
    let (status, value) = read_json(response)?;
    Ok(((200..300).contains(&status) && value.is_object()).then_some(value))
}

fn form_encode(pairs: &[(&str, String)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish()
}

/// What a token endpoint answered.
#[derive(Debug, Clone)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
}

fn token_request(
    endpoint: &str,
    form: &[(&str, String)],
    secret: Option<&str>,
) -> Result<Tokens, String> {
    let (agent, uri) = agent(endpoint)?;
    let body = form_encode(form);
    let client_id = form
        .iter()
        .find(|(k, _)| *k == "client_id")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let response = crate::net::guarded(|| {
        let mut request = agent
            .post(uri)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json");
        if let Some(secret) = secret {
            let basic =
                base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{secret}"));
            request = request.header("Authorization", format!("Basic {basic}"));
        }
        request.send(body.as_str())
    })
    .map_err(|e| crate::net::describe(&e))?;
    let (status, value) = read_json(response)?;
    if !(200..300).contains(&status) {
        return Err(oauth_error(&value, status));
    }
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or("the token endpoint gave no access token")?
        .to_string();
    if let Some(kind) = value.get("token_type").and_then(Value::as_str)
        && !kind.eq_ignore_ascii_case("bearer")
    {
        return Err(format!("a `{kind}` token, which MCP does not use"));
    }
    Ok(Tokens {
        access_token,
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_in: value.get("expires_in").and_then(Value::as_u64),
        scope: value
            .get("scope")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn apply_tokens(session: &mut Session, tokens: &Tokens) {
    session.access_token = tokens.access_token.clone();
    session.refresh_token = tokens.refresh_token.clone();
    session.expires_at = tokens.expires_in.map(|s| now_secs() + s);
    if tokens.scope.is_some() {
        session.scope = tokens.scope.clone();
    }
}

/// An OAuth error response in words.
fn oauth_error(value: &Value, status: u16) -> String {
    let error = value.get("error").and_then(Value::as_str);
    let detail = value.get("error_description").and_then(Value::as_str);
    match (error, detail) {
        (Some(e), Some(d)) => format!("{e}: {d}"),
        (Some(e), None) => e.to_string(),
        _ => format!("the authorization server answered HTTP {status}"),
    }
}

// -------------------------------------------------------------- discovery

/// The protected resource metadata of the server at `resource`.
fn resource_metadata(resource: &str, challenge: Option<&Challenge>) -> Result<Value, String> {
    let mut candidates = Vec::new();
    if let Some(url) = challenge.and_then(|c| c.resource_metadata.clone()) {
        candidates.push(url);
    }
    let parsed = url::Url::parse(resource).map_err(|e| e.to_string())?;
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/');
    if !path.is_empty() {
        candidates.push(format!(
            "{origin}/.well-known/oauth-protected-resource{path}"
        ));
    }
    candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));
    for url in candidates {
        if let Some(doc) = get_json(&url)? {
            return Ok(doc);
        }
    }
    Err("the server does not say where to sign in (no protected resource metadata)".into())
}

/// The metadata of authorization server `issuer`, its `issuer` checked.
fn server_metadata(issuer: &str) -> Result<Value, String> {
    let parsed = url::Url::parse(issuer).map_err(|e| format!("{issuer}: {e}"))?;
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/');
    let candidates = if path.is_empty() {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server"),
            format!("{origin}/.well-known/openid-configuration"),
        ]
    } else {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            format!("{origin}/.well-known/openid-configuration{path}"),
            format!("{origin}{path}/.well-known/openid-configuration"),
        ]
    };
    for url in candidates {
        if let Some(doc) = get_json(&url)? {
            let named = doc
                .get("issuer")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if named != issuer && named.trim_end_matches('/') != issuer.trim_end_matches('/') {
                return Err(format!(
                    "the authorization server's metadata names another issuer ({named}, not {issuer})"
                ));
            }
            return Ok(doc);
        }
    }
    Err(format!(
        "{issuer} publishes no authorization server metadata"
    ))
}

/// A client registered dynamically: its id, its secret if it got one, and
/// the redirect it was registered with.
fn register(endpoint: &str, redirect_uri: &str) -> Result<(String, Option<String>), String> {
    let (agent, uri) = agent(endpoint)?;
    let body = serde_json::json!({
        "client_name": "Clew",
        "client_uri": "https://github.com/RutaTang/Clew",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    })
    .to_string();
    let response = crate::net::guarded(|| {
        agent
            .post(uri)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .send(body.as_str())
    })
    .map_err(|e| crate::net::describe(&e))?;
    let (status, value) = read_json(response)?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "registering clew with the authorization server failed: {}",
            oauth_error(&value, status)
        ));
    }
    let id = value
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or("the registration gave no client_id")?
        .to_string();
    let secret = value
        .get("client_secret")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((id, secret))
}

// ----------------------------------------------------------------- PKCE

/// `n` random bytes, base64url without padding.
fn random_token(n: usize) -> Result<String, String> {
    use ring::rand::SecureRandom;
    let mut bytes = vec![0u8; n];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "no randomness available".to_string())?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// The S256 challenge of a PKCE verifier.
pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::Digest;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()))
}

// -------------------------------------------------------------- callback

/// Wait on `listener` for the browser's redirect to `/callback` and return
/// its query parameters; every other request is answered and ignored.
fn await_redirect(
    listener: TcpListener,
    state: &str,
    deadline: Instant,
) -> Result<BTreeMap<String, String>, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    loop {
        if Instant::now() >= deadline {
            return Err("the sign-in was not finished in the browser in time".into());
        }
        let (stream, _) = match listener.accept() {
            Ok(conn) => conn,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut line = String::new();
        if reader.by_ref().take(8192).read_line(&mut line).is_err() {
            continue;
        }
        // The rest of the head, unread, is dropped with the connection.
        let target = line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string();
        let mut stream = stream;
        let Some(query) = target.strip_prefix("/callback") else {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            continue;
        };
        let params: BTreeMap<String, String> =
            url::form_urlencoded::parse(query.trim_start_matches('?').as_bytes())
                .into_owned()
                .collect();
        // Only the answer to this sign-in ends the wait: anything else that
        // reaches the port (a page, another program) is turned away, and
        // cannot make the sign-in fail.
        if params.get("state").map(String::as_str) != Some(state) {
            let _ = stream.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            continue;
        }
        let page = "<!doctype html><meta charset=utf-8><title>Clew</title>\
                    <body style=\"font:16px system-ui;margin:3em\">\
                    <p>Signed in. You can close this tab and go back to Clew.</p>";
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        return Ok(params);
    }
}

// ---------------------------------------------------------------- the flow

/// A client id the user registered with the authorization server themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preregistered {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// Sign in to the MCP server at `url`: the whole flow (see the module docs),
/// `open` showing the user the authorization page. Blocking — run it on a
/// blocking thread; it waits at most [`SIGN_IN_TIMEOUT`] for the browser.
pub fn sign_in(
    url: &str,
    preregistered: Option<Preregistered>,
    open: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    let resource = canonical_resource(url)?;
    // What the server itself says it needs, from an unauthenticated probe.
    let challenge = probe_challenge(url);
    let prm = resource_metadata(&resource, challenge.as_ref())?;
    if let Some(named) = prm.get("resource").and_then(Value::as_str) {
        let named = canonical_resource(named)?;
        if !(resource == named || resource.starts_with(&format!("{named}/"))) {
            return Err(format!(
                "the server's metadata describes another resource ({named})"
            ));
        }
    }
    let issuer = prm
        .get("authorization_servers")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .ok_or("the server names no authorization server")?
        .to_string();
    let meta = server_metadata(&issuer)?;
    let endpoint = |key: &str| -> Result<String, String> {
        let url = meta
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("the authorization server has no {key}"))?
            .to_string();
        check_endpoint(&url)?;
        Ok(url)
    };
    let authorization_endpoint = endpoint("authorization_endpoint")?;
    let token_endpoint = endpoint("token_endpoint")?;
    if let Some(methods) = meta
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
        && !methods.iter().any(|m| m.as_str() == Some("S256"))
    {
        return Err("the authorization server does not support PKCE (S256)".into());
    }
    let iss_supported = meta
        .get("authorization_response_iss_parameter_supported")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let scope = challenge
        .as_ref()
        .and_then(|c| c.scope.clone())
        .or_else(|| {
            prm.get("scopes_supported")
                .and_then(Value::as_array)
                .map(|s| {
                    s.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|s| !s.is_empty())
        });

    // The redirect: the port the last registration with this server used,
    // when it is free, so that registration can serve again.
    let previous = session(url).filter(|s| s.issuer == issuer);
    let previous_port = previous
        .as_ref()
        .and_then(|s| s.redirect_uri.as_deref())
        .and_then(|r| url::Url::parse(r).ok())
        .and_then(|r| r.port());
    let listener = previous_port
        .and_then(|port| TcpListener::bind(("127.0.0.1", port)).ok())
        .map_or_else(|| TcpListener::bind(("127.0.0.1", 0)), Ok)
        .map_err(|e| format!("no local port for the sign-in to come back to: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let (client_id, client_secret, registered_redirect) = match preregistered {
        Some(p) => (p.client_id, p.client_secret, None),
        None => match previous
            .as_ref()
            .filter(|s| s.redirect_uri.as_deref() == Some(redirect_uri.as_str()))
        {
            Some(s) => (
                s.client_id.clone(),
                s.client_secret.clone(),
                s.redirect_uri.clone(),
            ),
            None => {
                let registration = meta
                    .get("registration_endpoint")
                    .and_then(Value::as_str)
                    .ok_or(
                        "the authorization server does not let clew register itself — register an \
                         OAuth client there and set oauth_client_id for this server in config.toml",
                    )?;
                let (id, secret) = register(registration, &redirect_uri)?;
                (id, secret, Some(redirect_uri.clone()))
            }
        },
    };

    let verifier = random_token(48)?;
    let state = random_token(24)?;
    let mut auth_url = url::Url::parse(&authorization_endpoint)
        .map_err(|e| format!("{authorization_endpoint}: {e}"))?;
    {
        let mut q = auth_url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("code_challenge", &pkce_challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("resource", &resource);
        if let Some(scope) = &scope {
            q.append_pair("scope", scope);
        }
    }
    open(auth_url.as_str())?;
    let params = await_redirect(listener, &state, Instant::now() + SIGN_IN_TIMEOUT)?;

    // RFC 9207: a present `iss` must be the recorded issuer; an absent one
    // only when the server does not promise to send it. On a mismatch
    // nothing of the response — its error included — is acted on.
    match params.get("iss") {
        Some(iss) if iss != &issuer => {
            return Err("the sign-in came back from another authorization server — refused".into());
        }
        None if iss_supported => {
            return Err("the sign-in response did not say who sent it — refused".into());
        }
        _ => {}
    }
    if params.get("state") != Some(&state) {
        return Err("the sign-in response does not match the request — refused".into());
    }
    if let Some(error) = params.get("error") {
        return Err(match params.get("error_description") {
            Some(d) => format!("the sign-in was refused: {error}: {d}"),
            None => format!("the sign-in was refused: {error}"),
        });
    }
    let code = params.get("code").ok_or("the sign-in gave no code")?;
    let tokens = token_request(
        &token_endpoint,
        &[
            ("grant_type", "authorization_code".to_string()),
            ("code", code.clone()),
            ("redirect_uri", redirect_uri.clone()),
            ("client_id", client_id.clone()),
            ("code_verifier", verifier),
            ("resource", resource.clone()),
        ],
        client_secret.as_deref(),
    )?;
    let mut session = Session {
        issuer,
        token_endpoint,
        client_id,
        client_secret,
        redirect_uri: registered_redirect,
        scope,
        ..Session::default()
    };
    apply_tokens(&mut session, &tokens);
    edit_store(|store| {
        store.servers.insert(resource, session);
    })
}

/// The Bearer challenge the server at `url` answers an unauthenticated
/// request with, if it does.
fn probe_challenge(url: &str) -> Option<Challenge> {
    let (agent, uri) = agent(url).ok()?;
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "server/discover",
        "params": super::with_meta(serde_json::json!({}), super::MODERN_VERSION),
    })
    .to_string();
    let response = crate::net::guarded(|| {
        agent
            .post(uri)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", super::MODERN_VERSION)
            .header("Mcp-Method", "server/discover")
            .send(body.as_str())
    })
    .ok()?;
    let header = response.headers().get("www-authenticate")?.to_str().ok()?;
    parse_challenge(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_are_read_from_www_authenticate() {
        let c = parse_challenge(
            r#"Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", scope="files:read files:write""#,
        )
        .unwrap();
        assert_eq!(
            c.resource_metadata.as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(c.scope.as_deref(), Some("files:read files:write"));
        let c = parse_challenge(
            r#"Basic realm="x", Bearer error="insufficient_scope", scope=admin, error_description="needs \"admin\"""#,
        )
        .unwrap();
        assert_eq!(c.error.as_deref(), Some("insufficient_scope"));
        assert_eq!(c.scope.as_deref(), Some("admin"));
        assert!(parse_challenge(r#"Basic realm="x""#).is_none());
        assert_eq!(parse_challenge("Bearer").unwrap(), Challenge::default());
    }

    #[test]
    fn resources_are_canonical() {
        assert_eq!(
            canonical_resource("https://MCP.example.com/mcp/?x=1#f").unwrap(),
            "https://mcp.example.com/mcp"
        );
        assert_eq!(
            canonical_resource("https://mcp.example.com/").unwrap(),
            "https://mcp.example.com"
        );
    }

    #[test]
    fn the_pkce_challenge_is_s256_of_the_verifier() {
        // base64url(SHA-256("abc")), unpadded.
        assert_eq!(
            pkce_challenge("abc"),
            "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0"
        );
        let a = random_token(32).unwrap();
        assert_ne!(a, random_token(32).unwrap());
        assert_eq!(a.len(), 43);
    }

    #[test]
    fn endpoints_must_be_https_off_this_machine() {
        assert!(check_endpoint("https://auth.example.com/token").is_ok());
        assert!(check_endpoint("http://127.0.0.1:9/token").is_ok());
        assert!(check_endpoint("http://auth.example.com/token").is_err());
    }
}
