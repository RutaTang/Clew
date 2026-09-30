//! Outbound HTTP for clew-core: the one transport every request it makes goes
//! through — model calls ([`crate::llm`]) and embeddings ([`crate::embed`]) as
//! much as the downloads here (release metadata, update images, prebuilt
//! clew-server binaries, language servers for [`crate::lsp`]'s store).
//!
//! ureq writes each request and reads its response. The connection under it
//! is made here, through ureq's connector API (`agent`): the name lookup, TCP,
//! a proxy's handshake and TLS, for plain http as much as for https. What
//! every request shares:
//!
//! - **Trust**: the bundled Mozilla roots PLUS the operating system's store
//!   (`tls_config`), for the target and for an `https://` proxy alike.
//! - **Proxies**: the conventional environment variables, and when they name
//!   none the system's proxy settings (macOS), with `NO_PROXY`, the system's
//!   exceptions (address ranges included) and a loopback bypass
//!   (`via`). http, `https://` and SOCKS (4, 4a, 5, 5h) proxies all
//!   carry http and https targets, and a proxy may be an IPv6 address. A
//!   proxy that is configured but cannot be used is an error (`proxy`), never
//!   a reason to connect directly.
//! - **One URL parser**: every decision about a URL — is it HTTPS, is its host
//!   loopback, which proxy applies — is made on the `url` crate's (WHATWG)
//!   reading, and ureq is handed that reading's own serialization, refused
//!   unless ureq's parser finds the same scheme, host and port in it
//!   (`request_uri`). A hand-rolled split disagreed with the parser:
//!   `http://evil.com\@127.0.0.1/` read as loopback here while the request
//!   went to `evil.com`, in the clear.
//! - **A byte meter on every connection** (`Metered`): what one response may
//!   deliver, all it sends counted — status line, headers, chunked framing,
//!   trailers, TLS records — so no peer keeps a connection going past the
//!   bound the decoded-body caps set. Response headers are held to 64 KiB,
//!   and a line the parser cannot end to the 128 KiB input buffer.
//! - **Time**: a connect timeout (the lookup, TCP, a proxy's handshake, TLS),
//!   an idle limit on every read and write, and for some requests an overall
//!   deadline (`Profile`) — one instant every read of a TLS record shares,
//!   and every write of what one call sends, so a peer that trickles a
//!   record, or takes a request in a little at a time, cannot stretch it
//!   (`Bound`) but, on macOS, by the rest of one write of at most 128 KiB
//!   (`Tcp`'s `transmit_output`).
//! - **Addresses in turns**: a name with several addresses is connected to as
//!   RFC 8305's Happy Eyeballs does it, a family that does not answer costing
//!   a quarter of a second (`connect_to`); `localhost` is loopback without a
//!   resolver asked (`lookup`).
//! - **An abandon reaches the socket**: a request made on a thread tethered
//!   to a `Line` — a transfer's worker, a model call's, an embeddings
//!   batch's — has its connection's socket on that line before anything is
//!   sent, so the abandon (a cancel, a deadline, a stopped answer) shuts the
//!   connection down under whatever it is blocked in: the connect, a proxy's
//!   or a TLS handshake, the wait for headers, a stalled body. A connection
//!   kept for the next request is on each request's line in turn, for that
//!   request's time on it alone (`Lease`). The one wait an abandon cannot
//!   cut short is a name lookup inside the system resolver (see `lookup`).
//! - **A kept connection holds nothing unasked-for**: none of what the peer
//!   sent past a response, wherever between the socket and ureq it waits
//!   (`Tls::is_open`).
//! - **A TLS response ends with TLS**: a connection that ends without the
//!   peer's close_notify is an error, not the end of a body delimited by the
//!   close (`Tls::await_input`) — though a chunked body whose last chunk had
//!   come is whole (`Exchange`).
//! - **A panic inside ureq** is an error of the request (`guarded`).
//!
//! Downloads ([`get`], [`get_cancellable`], [`download`]) add their own
//! policy on top of that:
//!
//! - **HTTPS only**, redirects included. The one exception is plain http to a
//!   LOOPBACK host (`localhost`, `127.0.0.0/8`, `[::1]`) — how the tests and a
//!   local mirror serve — and that follows no redirects at all, so it cannot
//!   be bounced to another host in the clear.
//! - **Redirects are followed here, hop by hop** (`transfer_with`), each
//!   hop with an agent of its own (`hop_agent`): the proxy (and `NO_PROXY`)
//!   is decided for the host that hop goes to. ureq itself follows none: it
//!   would take the first hop's route, and the caller's headers, to wherever
//!   one pointed.
//! - **The caller's headers stay with the caller's origin**: once a chain
//!   leaves it, only the headers that carry nothing of the caller's
//!   (`FOLLOWS_EVERY_HOP`) go on.
//! - **An overall deadline and the caller's cancel flag** bound the whole
//!   transfer: it runs on a worker thread, and the caller stops waiting the
//!   moment either fires. The worker is then abandoned (`Wire`), which ends
//!   it too — its connection is shut down under it.
//! - **A byte cap**, checked against `Content-Length` up front and enforced
//!   while reading, so an oversized (or endless) body costs at most the cap.
//!
//! What remains out of reach, accepted:
//!
//! - **A name lookup** cannot be interrupted. The request stops waiting for
//!   it at the connect timeout or the abandon; its thread stays inside the
//!   system resolver until the resolver answers. One lookup per name is under
//!   way at a time, which every request for the name waits on, and at most
//!   sixteen names are looked up at once (`MAX_LOOKUPS`): a resolver that
//!   answers nothing holds that many threads, and a new name is refused.
//! - **A proxy's password in the macOS keychain is not read**, nor an
//!   automatic proxy configuration (PAC) evaluated (`ProxyConfig::from_system`):
//!   a proxy that asks for credentials fails saying so, and how to give them
//!   or bypass it (`Via::advice`).

use std::cell::RefCell;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use ureq::http::{Response, Uri};
use ureq::unversioned::resolver::ResolvedSocketAddrs;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, LazyBuffers, NextTimeout, Transport,
};
use ureq::{Body, Timeout};

/// How long to wait for a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long any single read may stall.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Redirects a download follows. Release assets redirect (GitHub → its object
/// store); a chain longer than this is not a download.
const MAX_REDIRECTS: u32 = 8;
/// Every request identifies itself (GitHub answers 403 without one).
const USER_AGENT: &str = concat!("clew/", env!("CARGO_PKG_VERSION"));
/// How often a caller waiting on a transfer re-tests its cancel flag and its
/// deadline.
const POLL: Duration = Duration::from_millis(100);
/// How often a connect or a name lookup in progress looks at its request's
/// line, so an abandon ends the wait that soon.
const TICK: Duration = Duration::from_millis(50);

/// What one transfer may cost.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest body accepted.
    pub max_bytes: u64,
    /// Wall-clock budget for the whole transfer.
    pub deadline: Duration,
}

// ------------------------------------------------------------------- trust

/// Trust anchors for every HTTPS request: the bundled Mozilla roots (ureq's
/// default) PLUS the operating system's store.
///
/// The union, because either alone breaks someone. Bundled-only rejects a
/// private or corporate CA the user installed in their OS store (an internal
/// gateway, a TLS-inspecting proxy) — an endpoint behind one could not be
/// reached at all, with no override. OS-only (ureq's `native-certs` feature,
/// which REPLACES the bundled set) fails outright on a minimal host with no CA
/// bundle, which is exactly where a remote clew-server may run. Built once, on
/// the first request.
pub(crate) fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let config = rustls::ClientConfig::builder_with_provider(
                rustls::crypto::ring::default_provider().into(),
            )
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
            // Infallible for the ring provider, which ships suites for both
            // versions (ureq's own default config makes the same call).
            .expect("the ring provider supports TLS 1.2 and 1.3")
            .with_root_certificates(trust_roots())
            .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// The root store behind [`tls_config`]: bundled, then the system's.
fn trust_roots() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    // An unreadable store, or entries rustls cannot parse, only cost the
    // extra roots; the bundled set above still stands.
    let native = rustls_native_certs::load_native_certs();
    let _ = roots.add_parsable_certificates(native.certs);
    roots
}

// -------------------------------------------------------------------- URLs

/// `(scheme, host)` of `url` as ureq will read it (see the module docs):
/// both lowercase, an IPv6 host without its brackets, a domain without a
/// trailing dot. `None` for what ureq could not send at all.
pub(crate) fn url_scheme_host(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = match parsed.host()? {
        url::Host::Domain(domain) => domain.trim_end_matches('.').to_ascii_lowercase(),
        url::Host::Ipv4(ip) => ip.to_string(),
        url::Host::Ipv6(ip) => ip.to_string(),
    };
    Some((parsed.scheme().to_string(), host))
}

/// `(host, path)` of `url` when it is `https` on the default port, as ureq
/// will send it: the host as `url_scheme_host` gives it, the path with its
/// dot segments — `%2e%2e` spellings included — already resolved. `None` for
/// anything else. For a check on WHERE a URL leads: a prefix test on the raw
/// text passes `…/releases/download/%2e%2e/%2e%2e/…`, which the parser turns
/// into another path entirely.
pub fn https_host_path(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.scheme() != "https" || parsed.port().is_some() {
        return None;
    }
    let (_, host) = url_scheme_host(url)?;
    Some((host, parsed.path().to_string()))
}

/// Whether `url` is plain http to a loopback address — the only non-HTTPS
/// target a download accepts (`hop_agent`). Stricter than the proxy bypass:
/// `localhost` itself or a loopback IP, not every `*.localhost` name. Those
/// reach this machine as surely (`lookup` takes them for loopback, with no
/// resolver asked), but the exception is for a local mirror and the tests,
/// which need no more.
pub fn is_loopback_http(url: &str) -> bool {
    url_scheme_host(url).is_some_and(|(scheme, host)| {
        scheme == "http"
            && (host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback()))
    })
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

// ----------------------------------------------------------------- proxies

/// The process environment as the proxy rules read it: a variable set to
/// something that is not UTF-8 is read lossily, not as unset. Unset would
/// connect directly around the proxy the user configured; the lossy value
/// fails as an unusable proxy (or, in `NO_PROXY`, exempts nothing), which is
/// the direction [`proxy`] errs in.
pub(crate) fn env_var(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
}

/// What a proxy decision reads: the process environment, and the operating
/// system's own proxy settings. Injected, so the rules can be tested without
/// touching either; every request clew-core makes passes
/// [`ProxySources::LIVE`] — downloads ([`transfer_on`]) as much as model
/// calls and embeddings ([`crate::llm::agent_for`]).
#[derive(Clone, Copy)]
pub(crate) struct ProxySources<'a> {
    /// One variable of the process environment ([`env_var`]).
    pub(crate) env: &'a dyn Fn(&str) -> Option<String>,
    /// The system's proxy settings; `None` when no proxy is on
    /// ([`system_proxies`]).
    pub(crate) system: &'a dyn Fn() -> Option<ProxyConfig>,
}

impl ProxySources<'static> {
    /// This process's environment, and this machine's settings.
    pub(crate) const LIVE: ProxySources<'static> = ProxySources {
        env: &env_var,
        system: &system_proxies,
    };
}

/// Proxy settings as one source states them, in the one shape the rules of
/// [`via`] apply to: the proxy for each scheme, and the hosts that
/// bypass it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProxyConfig {
    /// The proxy for `http` URLs, a setting [`proxy`] reads.
    http: Option<String>,
    /// The proxy for `https` URLs.
    https: Option<String>,
    /// The hosts that connect directly, each an entry
    /// [`bypass_entry_matches`] reads.
    bypass: Vec<String>,
    /// Whether a host name without a dot connects directly too (macOS's
    /// "Exclude simple hostnames").
    bypass_simple: bool,
}

impl ProxyConfig {
    /// The environment's settings, per the conventional variables: `https`
    /// URLs use `https_proxy` / `HTTPS_PROXY`, `http` URLs `http_proxy` /
    /// `HTTP_PROXY`, each falling back to `all_proxy` / `ALL_PROXY`; and
    /// `no_proxy` / `NO_PROXY` lists the hosts that bypass them,
    /// comma-separated. The lowercase spelling wins when both are set; a
    /// variable that is empty counts as unset.
    fn from_env(env: &dyn Fn(&str) -> Option<String>) -> ProxyConfig {
        let first = |names: &[&str]| {
            names
                .iter()
                .filter_map(|name| env(name))
                .map(|v| v.trim().to_string())
                .find(|v| !v.is_empty())
        };
        let bypass = first(&["no_proxy", "NO_PROXY"])
            .map(|list| {
                list.split(',')
                    .map(|entry| entry.trim().to_string())
                    .filter(|entry| !entry.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        ProxyConfig {
            http: first(&["http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"]),
            https: first(&["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"]),
            bypass,
            bypass_simple: false,
        }
    }

    /// The settings a system proxy dictionary states — the
    /// `kSCPropNetProxies*` keys macOS's System Settings › Network › Proxies
    /// writes — each value read through `get`:
    ///
    /// - `HTTPSEnable`, `HTTPSProxy`, `HTTPSPort`: the proxy for `https` URLs
    ///   (macOS's "Secure web proxy": an http proxy, reached with `CONNECT`);
    /// - `HTTPEnable`, `HTTPProxy`, `HTTPPort`: the one for `http` URLs;
    /// - `SOCKSEnable`, `SOCKSProxy`, `SOCKSPort`: a SOCKS proxy, for either
    ///   scheme whose own proxy is off — SOCKS5, handed the host name to
    ///   resolve (`socks5h`), as the system's own clients hand it;
    /// - `ExceptionsList`: the hosts that bypass them;
    /// - `ExcludeSimpleHostnames`: whether a name without a dot does too.
    ///
    /// A switch may be a number or a CFBoolean. A port is taken as it is
    /// stated: one no connection can use makes the setting unusable, never a
    /// reason to use another port. A proxy that asks for a password is used
    /// without one: macOS keeps it in the keychain, which clew does not read
    /// (see [`Via::advice`]).
    ///
    /// `None` when no proxy is on. An automatic configuration — a PAC file
    /// (`ProxyAutoConfigEnable`) or auto-discovery — is not evaluated, which
    /// takes a JavaScript engine: under one alone, requests connect directly
    /// as they always did, and `https_proxy` is the way to name its proxy.
    ///
    /// Built where there is such a dictionary to read (macOS), and for the
    /// tests, which drive it on every system.
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn from_system(get: &dyn Fn(&str) -> Option<SystemValue>) -> Option<ProxyConfig> {
        // A switch is a number, or a CFBoolean.
        let on = |key: &str| match get(key) {
            Some(SystemValue::Number(n)) => n != 0,
            Some(SystemValue::Bool(on)) => on,
            _ => false,
        };
        let proxy = |kind: &str, scheme: &str| {
            if !on(&format!("{kind}Enable")) {
                return None;
            }
            let Some(SystemValue::Text(host)) = get(&format!("{kind}Proxy")) else {
                return None;
            };
            let host = host.trim();
            if host.is_empty() {
                return None;
            }
            // An IPv6 address in brackets, as a URL spells it.
            let host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
                format!("[{host}]")
            } else {
                host.to_string()
            };
            // As it stands: a port no connection can use (0, or past 65535)
            // makes the setting unusable (`proxy`), rather than a reason to
            // use the scheme's default port instead.
            let port = match get(&format!("{kind}Port")) {
                Some(SystemValue::Number(port)) => format!(":{port}"),
                Some(SystemValue::Text(port)) => format!(":{}", port.trim()),
                _ => String::new(),
            };
            Some(format!("{scheme}://{host}{port}"))
        };
        let socks = proxy("SOCKS", "socks5h");
        let https = proxy("HTTPS", "http").or_else(|| socks.clone());
        let http = proxy("HTTP", "http").or(socks);
        if http.is_none() && https.is_none() {
            return None;
        }
        let bypass = match get("ExceptionsList") {
            Some(SystemValue::List(entries)) => entries
                .into_iter()
                .map(|entry| entry.trim().to_string())
                .filter(|entry| !entry.is_empty())
                .collect(),
            _ => Vec::new(),
        };
        Some(ProxyConfig {
            http,
            https,
            bypass,
            bypass_simple: on("ExcludeSimpleHostnames"),
        })
    }

    /// Whether `host` (as [`url_scheme_host`] gives it) connects directly.
    fn bypasses(&self, host: &str) -> bool {
        (self.bypass_simple && !host.contains('.') && host.parse::<IpAddr>().is_err())
            || self
                .bypass
                .iter()
                .any(|entry| bypass_entry_matches(host, entry))
    }
}

/// One value of a system proxy dictionary, as [`ProxyConfig::from_system`]
/// reads it.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SystemValue {
    Number(i64),
    Bool(bool),
    Text(String),
    List(Vec<String>),
}

/// This machine's proxy settings (`ProxyConfig::from_system`): on macOS,
/// the ones System Settings › Network › Proxies holds for the network in
/// use, as `SCDynamicStoreCopyProxies` reports them; `None` elsewhere, and
/// when no proxy is on.
///
/// Read afresh on every call, which is one request to `configd`: a proxy
/// switched on or off — a VPN connecting, another network — applies to the
/// next request, not after a restart.
pub(crate) fn system_proxies() -> Option<ProxyConfig> {
    #[cfg(target_os = "macos")]
    {
        macos_proxies::read()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Reading the macOS proxy dictionary (see [`system_proxies`]).
#[cfg(target_os = "macos")]
mod macos_proxies {
    use super::{ProxyConfig, SystemValue};
    use system_configuration::core_foundation::array::CFArray;
    use system_configuration::core_foundation::base::{CFType, TCFType};
    use system_configuration::core_foundation::boolean::CFBoolean;
    use system_configuration::core_foundation::dictionary::CFDictionary;
    use system_configuration::core_foundation::number::CFNumber;
    use system_configuration::core_foundation::string::CFString;
    use system_configuration::dynamic_store::SCDynamicStoreBuilder;

    /// The settings in force now.
    pub(super) fn read() -> Option<ProxyConfig> {
        let store = SCDynamicStoreBuilder::new("clew").build()?;
        from_dictionary(&store.get_proxies()?)
    }

    /// [`ProxyConfig::from_system`] over the dictionary CoreFoundation
    /// hands out.
    pub(super) fn from_dictionary(dict: &CFDictionary<CFString, CFType>) -> Option<ProxyConfig> {
        ProxyConfig::from_system(&|key| value(dict, key))
    }

    /// The value under `key`, when it is one of the kinds a proxy setting
    /// takes: a number, a boolean, a string, or a list of strings.
    fn value(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<SystemValue> {
        let item = dict.find(CFString::new(key))?;
        if let Some(flag) = item.downcast::<CFBoolean>() {
            return Some(SystemValue::Bool(flag.into()));
        }
        if let Some(number) = item.downcast::<CFNumber>() {
            return number.to_i64().map(SystemValue::Number);
        }
        if let Some(text) = item.downcast::<CFString>() {
            return Some(SystemValue::Text(text.to_string()));
        }
        let list = item.downcast::<CFArray>()?;
        let entries = list
            .iter()
            .filter_map(|element| {
                // SAFETY: the arrays of a property list hold CoreFoundation
                // objects, which they keep alive; retaining one under the get
                // rule is how an element is taken out of one.
                let element = unsafe { CFType::wrap_under_get_rule(*element) };
                element.downcast::<CFString>().map(|text| text.to_string())
            })
            .collect();
        Some(SystemValue::List(entries))
    }
}

/// The proxy a request to `url` should go through — or `None` to connect
/// directly — per `sources`, and where its setting came from ([`Via`]).
///
/// ureq's own reading of the environment is deliberately not used (every
/// agent is built with no proxy of ureq's, `agent`): it takes the first proxy
/// variable it finds for every scheme, and knows neither the system's
/// settings nor address ranges in `NO_PROXY` — ureq 2's applied that one
/// proxy to EVERY host, so behind a corporate proxy a Custom endpoint on
/// `localhost` (Ollama, LM Studio, a llama.cpp server) went to a proxy that
/// cannot reach it.
///
/// The rules:
/// - loopback hosts (`localhost`, `*.localhost`, `127.0.0.0/8`, `::1`) always
///   connect directly;
/// - the environment comes first ([`ProxyConfig::from_env`], the common curl
///   subset): when it names any proxy, it alone decides;
/// - otherwise the system's settings apply ([`system_proxies`]), and the
///   hosts `NO_PROXY` lists bypass them as well as the system's own
///   exceptions do — so `NO_PROXY=*` still means no proxy at all;
/// - a host the list in force covers ([`bypass_entry_matches`]) connects
///   directly, and any other takes its scheme's proxy.
pub(crate) fn via(url: &str, sources: &ProxySources<'_>) -> Option<Via> {
    let (scheme, host) = url_scheme_host(url)?;
    if is_loopback_host(&host) {
        return None;
    }
    let env = ProxyConfig::from_env(sources.env);
    let (config, system) = if env.http.is_some() || env.https.is_some() {
        (env, false)
    } else {
        let mut system = (sources.system)()?;
        system.bypass.extend(env.bypass);
        (system, true)
    };
    if config.bypasses(&host) {
        return None;
    }
    let spec = match scheme.as_str() {
        "https" => config.https.clone(),
        "http" => config.http.clone(),
        _ => None,
    }?;
    Some(Via {
        spec,
        system: system.then_some(config),
        for_https: scheme == "https",
    })
}

/// Whether one bypass entry — of `NO_PROXY`, or of the system's exceptions —
/// covers `host` (as [`url_scheme_host`] gives it: lowercase, an IPv6
/// address without its brackets). An entry is one of:
///
/// - `*`: every host;
/// - an address range, `address/prefix-length`: `10.0.0.0/8`, `fd00::/8`
///   (brackets allowed), or an IPv4 network with its trailing zero octets
///   left out, as macOS writes its own default `169.254/16`. It covers the
///   hosts written as an address inside it (a name is not resolved to find
///   out), an IPv4 address spelled as IPv6 (`::ffff:10.1.2.3`) included;
/// - an address, which covers itself however it is spelled;
/// - a host name, which covers itself and every name under it
///   (`example.com` covers `api.example.com`; a leading `.` or `*.` is
///   accepted), with a `*` anywhere else matching any run of characters
///   (`192.168.*`, `*.corp.*`), as the macOS list allows.
///
/// A port on an entry is ignored; letters match whatever their case.
fn bypass_entry_matches(host: &str, entry: &str) -> bool {
    let entry = entry.trim();
    if entry == "*" {
        return true;
    }
    let host_ip = host.parse::<IpAddr>().ok();
    if let Some((network, bits)) = address_range(entry) {
        return host_ip.is_some_and(|ip| {
            in_range(ip, network, bits) || in_range(ip.to_canonical(), network, bits)
        });
    }
    // Ports are ignored. A bare IPv6 address has colons of its own, so only
    // strip a trailing `:digits` from something that is not an address
    // already.
    let entry = if entry.parse::<IpAddr>().is_ok() {
        entry
    } else if let Some(v6) = entry.strip_prefix('[') {
        v6.split(']').next().unwrap_or_default()
    } else {
        match entry.rsplit_once(':') {
            Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
            _ => entry,
        }
    };
    let entry = entry.trim_end_matches('.').to_ascii_lowercase();
    if let Ok(ip) = entry.parse::<IpAddr>() {
        return host_ip.is_some_and(|host_ip| host_ip.to_canonical() == ip.to_canonical());
    }
    let domain = entry.trim_start_matches("*.").trim_start_matches('.');
    if domain.is_empty() {
        return false;
    }
    if domain.contains('*') {
        return glob_matches(&entry, host);
    }
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// `entry` as an address range — `(network, prefix length)` — when it is one
/// (see [`bypass_entry_matches`]). A port after the prefix length
/// (`10.0.0.0/8:443`) is ignored, as on any entry, and an IPv4 range written
/// as IPv4-mapped IPv6 (`::ffff:10.0.0.0/104`) is the IPv4 range it maps.
fn address_range(entry: &str) -> Option<(IpAddr, u8)> {
    let (address, bits) = entry.rsplit_once('/')?;
    let bits = match bits.split_once(':') {
        Some((bits, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => bits,
        Some(_) => return None,
        None => bits,
    };
    if bits.is_empty() || !bits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let bits: u8 = bits.parse().ok()?;
    let address = address
        .strip_prefix('[')
        .and_then(|a| a.strip_suffix(']'))
        .unwrap_or(address);
    let network = match address.parse::<IpAddr>() {
        Ok(ip) => ip,
        Err(_) => IpAddr::V4(abbreviated_ipv4(address)?),
    };
    let width = if network.is_ipv4() { 32 } else { 128 };
    if bits > width {
        return None;
    }
    if let IpAddr::V6(v6) = network
        && let Some(v4) = v6.to_ipv4_mapped()
        && bits >= 96
    {
        return Some((IpAddr::V4(v4), bits - 96));
    }
    Some((network, bits))
}

/// An IPv4 network written with its trailing zero octets left out — `169.254`
/// for `169.254.0.0`, `10` for `10.0.0.0` — as BSD's `inet_net_pton` reads
/// one.
fn abbreviated_ipv4(text: &str) -> Option<std::net::Ipv4Addr> {
    let mut octets = [0u8; 4];
    for (at, part) in text.split('.').enumerate() {
        if at == octets.len()
            || part.is_empty()
            || part.len() > 3
            || !part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        octets[at] = part.parse().ok()?;
    }
    Some(octets.into())
}

/// Whether `ip` is inside the range `network/bits` (of its own family).
fn in_range(ip: IpAddr, network: IpAddr, bits: u8) -> bool {
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(network)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            u32::from(ip) & mask == u32::from(network) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(network)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            u128::from(ip) & mask == u128::from(network) & mask
        }
        _ => false,
    }
}

/// Whether `text` matches `pattern`, where a `*` stands for any run of
/// characters (none included) and everything else for itself.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t) = (0, 0);
    // The last `*` seen, and where in `text` the run it stands for ends.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if pattern.get(p) == Some(&b'*') {
            star = Some((p, t));
            p += 1;
        } else if pattern.get(p) == Some(&text[t]) {
            p += 1;
            t += 1;
        } else if let Some((at, end)) = star {
            // Let that `*` stand for one more character, and go on from
            // there.
            star = Some((at, end + 1));
            p = at + 1;
            t = end + 1;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&b| b == b'*')
}

/// The proxy a request goes through ([`via`]): its setting, which of a
/// source's proxies it is — and, when it is the system's (macOS's) rather
/// than the environment's, the system's settings as a whole: which is where
/// a host would be exempted from it, and what a proxy named in the
/// environment instead would have to restate.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Via {
    /// The setting, as [`proxy`] reads it.
    pub(crate) spec: String,
    /// The system's settings, when the setting is one of them — the hosts
    /// `NO_PROXY` adds to their exceptions included ([`via`]).
    pub(crate) system: Option<ProxyConfig>,
    /// Whether it is the proxy for `https` URLs, rather than for plain
    /// `http` ones ([`ProxyConfig`]).
    pub(crate) for_https: bool,
}

/// Credentials stay out of a `Via` printed for debugging.
impl fmt::Debug for Via {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Via")
            .field("spec", &redact_userinfo(&self.spec))
            .field("system", &self.system.is_some())
            .field("for_https", &self.for_https)
            .finish()
    }
}

/// How [`Via::advice`] says a user name and password go into a setting.
const PERCENT_ENCODING: &str =
    "percent-encoding any character in them that is not a letter or a digit";

impl Via {
    /// What a failure of this proxy's adds to its message, which has said
    /// what went wrong (`fault`): what the user can do about it. That is how
    /// to reach the host without the proxy — and when the proxy asked for a
    /// user name and password, or refused the ones its setting names, how to
    /// give them, in a setting of the proxy's own kind; when it gave no
    /// answer to them, which may or may not have been a refusal, what to do
    /// either way; when it takes only logins clew cannot make, what can make
    /// them.
    ///
    /// A system proxy that asks for a password is used without one: macOS
    /// keeps the password in the keychain, which clew does not read (the
    /// keychain would ask the user to let clew in, from under a background
    /// request). The environment can name it instead — and a proxy named
    /// there replaces ALL of the system's settings ([`via`]), so the
    /// advice restates every one of them, the exceptions included
    /// ([`Via::environment`]): the one variable of the failed request's
    /// scheme, set alone, sent the other scheme's requests around the proxy,
    /// and every host the system exempts through it. A Dock-launched app has
    /// no environment of its own to set, so the system's own bypass list
    /// comes first.
    ///
    /// The environment's own way around the proxy is `no_proxy`, lowercase
    /// as the proxy variables are: of `no_proxy` and `NO_PROXY`, the
    /// lowercase one is read, the other only while it is unset
    /// ([`ProxyConfig::from_env`]). A host listed in `NO_PROXY` as advised
    /// went through the proxy still in a shell that set `no_proxy`; and a
    /// `no_proxy` set to the host alone, in a shell that set `NO_PROXY`,
    /// sends every host that list exempts through the proxy — so the advice
    /// says which list the one it names takes the place of.
    pub(crate) fn advice(&self, fault: ProxyFault) -> String {
        let bypass = if self.system.is_some() {
            "list the host under \u{201c}Bypass proxy settings for these hosts\u{201d} in \
             System Settings \u{203a} Network \u{203a} Details \u{203a} Proxies, or in no_proxy, \
             which clew reads in place of NO_PROXY"
        } else {
            "list the host in no_proxy, which clew reads in place of NO_PROXY"
        };
        match fault {
            ProxyFault::Credentials => {}
            // The proxy said nothing of the login: it may be wrong, or it
            // may be right and the proxy have failed on its own.
            ProxyFault::LoginUnanswered if self.names_credentials() => {
                return format!(
                    " — if they are wrong, correct them in its setting ({}, {PERCENT_ENCODING}); \
                     if they are right, try again later, or {bypass}",
                    self.example()
                );
            }
            // NTLM and Negotiate are made by a relay on this machine, which
            // logs in to the proxy for the clients it serves.
            ProxyFault::LoginScheme => {
                return format!(
                    " — name a local relay that makes such logins for clew, such as cntlm or \
                     px, as the proxy instead, or {bypass}"
                );
            }
            _ => return format!(" — to reach the host without the proxy, {bypass}"),
        }
        let example = self.example();
        let give = if self.names_credentials() {
            format!("correct them in its setting ({example}, {PERCENT_ENCODING})")
        } else if let Some(system) = &self.system {
            format!(
                "clew does not read proxy passwords from the keychain: name them in the \
                 environment ({PERCENT_ENCODING}), which clew then reads instead of all of the \
                 system's proxy settings — so set {}",
                self.environment(system)
            )
        } else {
            format!("name them in its setting, as {example}")
        };
        format!(" — {give}, or {bypass}")
    }

    /// The failure of a request its http proxy answered with a `407` (Proxy
    /// Authentication Required), `headers` the answer's: nothing reached the
    /// target. Which logins the proxy takes its `Proxy-Authenticate`
    /// challenges say: when Basic, the one clew makes, is not among them, no
    /// user name and password in any setting lets clew in.
    pub(crate) fn answered_407(&self, headers: &ureq::http::HeaderMap) -> String {
        let challenges = headers
            .get_all("proxy-authenticate")
            .iter()
            .map(|value| String::from_utf8_lossy(value.as_bytes()));
        self.login_failure(&login_schemes(challenges))
    }

    /// [`Via::answered_407`], of a proxy whose challenges offer the login
    /// schemes `schemes` ([`login_schemes`]).
    fn login_failure(&self, schemes: &[String]) -> String {
        let (what, fault) = refused_login(schemes, self.names_credentials());
        format!(
            "the proxy {} answered HTTP 407 (Proxy Authentication Required): {what}{}",
            redact_userinfo(&self.spec),
            self.advice(fault)
        )
    }

    /// Whether the setting names a user name and password.
    fn names_credentials(&self) -> bool {
        match proxy(&self.spec) {
            Ok(ProxySetting::Http(proxy)) => proxy.credentials.is_some(),
            Ok(ProxySetting::Socks(proxy)) => proxy.credentials.is_some(),
            Err(_) => false,
        }
    }

    /// The setting with credentials in it, as a user would write it: the
    /// proxy's own scheme, host and port, and `user:password` — the one
    /// user id of SOCKS4.
    fn example(&self) -> String {
        let (scheme, host, port) = match proxy(&self.spec) {
            Ok(ProxySetting::Http(proxy)) => {
                let scheme = if proxy.tls { "https" } else { "http" };
                (scheme, proxy.host, proxy.port)
            }
            Ok(ProxySetting::Socks(proxy)) => (proxy.version.scheme(), proxy.host, proxy.port),
            Err(_) => return "http://user:password@host:port".into(),
        };
        let credentials = match scheme {
            "socks4" | "socks4a" => "user",
            _ => "user:password",
        };
        format!("{scheme}://{credentials}@{}", host_port(&host, port))
    }

    /// The system's settings, `system`, as the environment names them. A
    /// proxy named there replaces all of them ([`via`]), so all of
    /// them: the proxy of the failed request's scheme first, with the
    /// credentials in it; the other scheme's, with them too when it is the
    /// same proxy (one SOCKS proxy for both is `ALL_PROXY`, as macOS's SOCKS
    /// setting covers both); and the exceptions, as `no_proxy`.
    ///
    /// Lowercase, as the proxy variables are: of `no_proxy` and `NO_PROXY`,
    /// the lowercase one is read when both are set
    /// ([`ProxyConfig::from_env`]), so a `NO_PROXY` set as advised went
    /// unread in a shell that already set `no_proxy`. The list restates
    /// every exception in force — the ones the environment's own list adds
    /// included — so it replaces that list without losing any of it.
    fn environment(&self, system: &ProxyConfig) -> String {
        let named = |spec: &String| {
            if *spec == self.spec {
                self.example()
            } else {
                spec.clone()
            }
        };
        let (https, http) = (("https_proxy", &system.https), ("http_proxy", &system.http));
        let (mine, other) = if self.for_https {
            (https, http)
        } else {
            (http, https)
        };
        let socks = |spec: &String| matches!(proxy(spec), Ok(ProxySetting::Socks(_)));
        let mut set = Vec::new();
        match (mine.1, other.1) {
            (Some(one), Some(both)) if one == both && socks(one) => {
                set.push(format!("ALL_PROXY={}", named(one)));
            }
            _ => {
                for (variable, spec) in [mine, other] {
                    if let Some(spec) = spec {
                        set.push(format!("{variable}={}", named(spec)));
                    }
                }
            }
        }
        if !system.bypass.is_empty() {
            set.push(format!("no_proxy={}", system.bypass.join(",")));
        }
        // No entry of no_proxy's stands for every name without a dot.
        if system.bypass_simple {
            set.push(
                "(no_proxy also listing each name without a dot you use, which the system \
                 exempts)"
                    .into(),
            );
        }
        set.join(" ")
    }
}

/// The longest login scheme [`login_schemes`] takes: the ones in use are
/// short words (`Basic`, `NTLM`, `Negotiate`, `SCRAM-SHA-256`), and each
/// goes into a message.
const MAX_SCHEME: usize = 40;

/// The most login schemes [`login_schemes`] collects: a proxy offers a few
/// (Negotiate, NTLM, Basic, Digest), and a `407` offering more is read no
/// further. The bound keeps the read linear in the challenges' length, too:
/// each scheme read is looked for among the ones collected, which it keeps
/// to a few comparisons — unbounded, a 64 KiB challenge of distinct names
/// collected thousands of them, each compared with all before it.
const MAX_SCHEMES: usize = 16;

/// The login schemes a proxy's `Proxy-Authenticate` values offer, each once,
/// in the order offered — the first [`MAX_SCHEMES`] of them. Each value is
/// a list of challenges (RFC 9110 §11.6.1): a scheme, then its token68 or
/// its parameters — `name=value` elements of the same list, whose values
/// may be quoted strings with commas of their own ([`list_elements`]).
fn login_schemes(values: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<String> {
    let mut schemes: Vec<String> = Vec::new();
    for value in values {
        for element in list_elements(value.as_ref()) {
            let element = element.trim_matches([' ', '\t']);
            let end = element.find([' ', '\t', '=']).unwrap_or(element.len());
            let (token, rest) = element.split_at(end);
            // A parameter of the challenge before it (`realm="x"`, with
            // spaces allowed around the `=`).
            let parameter = rest.trim_start_matches([' ', '\t']).starts_with('=');
            if parameter
                || token.is_empty()
                || token.len() > MAX_SCHEME
                || !token.bytes().all(is_tchar)
            {
                continue;
            }
            if !schemes.iter().any(|seen| seen.eq_ignore_ascii_case(token)) {
                schemes.push(token.to_string());
                if schemes.len() == MAX_SCHEMES {
                    return schemes;
                }
            }
        }
    }
    schemes
}

/// The elements of `value`, a comma-separated list of an HTTP field: a
/// comma inside a quoted string (`realm="a, b"`) separates nothing.
fn list_elements(value: &str) -> Vec<&str> {
    let mut elements = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (at, c) in value.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                elements.push(&value[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    elements.push(&value[start..]);
    elements
}

/// Whether `b` may be in an HTTP token (RFC 9110 §5.6.2).
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// The login schemes of a `407`'s challenges (`schemes`), as a message names
/// them, when none is Basic — the one login clew makes. `None` when one is,
/// or when there are none at all: a `407` must name its schemes, and one
/// that does not says nothing against Basic.
fn unusable_login(schemes: &[String]) -> Option<String> {
    if schemes
        .iter()
        .any(|scheme| scheme.eq_ignore_ascii_case("basic"))
    {
        return None;
    }
    let named: Vec<&str> = schemes.iter().take(4).map(String::as_str).collect();
    let (last, rest) = named.split_last()?;
    Some(if rest.is_empty() {
        last.to_string()
    } else {
        format!("{} or {last}", rest.join(", "))
    })
}

/// A proxy setting, read (see [`proxy`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProxySetting {
    /// An http proxy ([`HttpProxy`]).
    Http(HttpProxy),
    /// A SOCKS proxy ([`Socks`]).
    Socks(Socks),
}

/// `spec` as a proxy setting. A proxy the user configured but that cannot be
/// used is an error, not a reason to connect directly: the network it guards
/// may not allow that, and the user asked for the proxy.
///
/// `spec` is read as curl reads it — a URL, or a bare `host:port` for an http
/// proxy — by the `url` crate: the host is a name or an address (an IPv6 one
/// in brackets), and the credentials are percent-decoded.
///
/// - `http://` is an http proxy (port 80 unless named), `https://` one that
///   is spoken to over TLS (443), verified like any peer.
/// - `socks5://` (or `socks://`) is SOCKS5, the target's name resolved here;
///   `socks5h://` is SOCKS5 with the name sent for the proxy to resolve.
/// - `socks4://` is SOCKS4 (the name resolved here, to IPv4), `socks4a://`
///   SOCKS4 with the name sent. SOCKS proxies default to port 1080.
/// - Anything else is refused, as is a port of 0.
pub(crate) fn proxy(spec: &str) -> Result<ProxySetting, String> {
    let unusable = |why: String| format!("unusable proxy setting {}: {why}", redact_userinfo(spec));
    let full = if spec.contains("://") {
        spec.to_string()
    } else {
        format!("http://{spec}")
    };
    let parsed = url::Url::parse(&full).map_err(|e| unusable(e.to_string()))?;
    let socks = match parsed.scheme() {
        "http" | "https" => None,
        "socks5" | "socks" => Some(SocksVersion::V5),
        "socks5h" => Some(SocksVersion::V5h),
        "socks4" => Some(SocksVersion::V4),
        "socks4a" => Some(SocksVersion::V4a),
        other => return Err(unusable(format!("clew cannot use {other}:// proxies"))),
    };
    if !matches!(parsed.path(), "" | "/") || parsed.query().is_some() || parsed.fragment().is_some()
    {
        return Err(unusable("a proxy address has no path".into()));
    }
    if parsed.port() == Some(0) {
        return Err(unusable("port 0 is no port to connect to".into()));
    }
    let host = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(unusable("no proxy host".into())),
    };
    let user = percent_decode(parsed.username());
    let password = parsed.password().map(percent_decode);
    let credentials =
        (!user.is_empty() || password.is_some()).then(|| (user, password.unwrap_or_default()));
    if let Some(version) = socks {
        return Ok(ProxySetting::Socks(Socks {
            version,
            host,
            port: parsed.port().unwrap_or(1080),
            credentials,
        }));
    }
    // Basic authentication ends the user name at the first colon.
    if credentials
        .as_ref()
        .is_some_and(|(user, _)| user.contains(':'))
    {
        return Err(unusable("a proxy user name cannot hold a `:`".into()));
    }
    let tls = parsed.scheme() == "https";
    Ok(ProxySetting::Http(HttpProxy {
        tls,
        host,
        port: parsed
            .port_or_known_default()
            .unwrap_or(if tls { 443 } else { 80 }),
        credentials,
    }))
}

/// `user:***` for a debug print of a proxy's credentials.
fn redacted(credentials: &Option<(String, String)>) -> Option<String> {
    credentials.as_ref().map(|(user, _)| format!("{user}:***"))
}

/// An http proxy, which clew speaks to itself ([`Dialer`]): `CONNECT` for an
/// https target, and for a plain-http one the request itself, addressed in
/// absolute form ([`Exchange`]). An `https://` proxy is spoken to over TLS,
/// verified with the trust every peer is, before either.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct HttpProxy {
    /// Spoken to over TLS (`https://`).
    tls: bool,
    /// A name, or an address — an IPv6 one without its brackets.
    host: String,
    port: u16,
    /// A user name and password (decoded), when the setting names them.
    credentials: Option<(String, String)>,
}

/// The password stays out of a debug print.
impl fmt::Debug for HttpProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpProxy")
            .field("tls", &self.tls)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("credentials", &redacted(&self.credentials))
            .finish()
    }
}

impl HttpProxy {
    /// The `Proxy-Authorization` every request to the proxy carries, when the
    /// setting names a user.
    fn authorization(&self) -> Option<String> {
        use base64::Engine as _;
        self.credentials.as_ref().map(|(user, password)| {
            let pair = format!("{user}:{password}");
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(pair)
            )
        })
    }
}

/// How the connections of a request reach its target — decided per request
/// ([`route`]) and carried by its agent ([`agent`]): directly, or through the
/// proxy whose handshake [`Dialer`] opens on each new connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// Straight to the target.
    Direct,
    /// Through an http proxy.
    Http(HttpProxy),
    /// Through a SOCKS proxy.
    Socks(Socks),
}

/// The route through the proxy `spec` names — directly, for none.
pub(crate) fn route(spec: Option<&str>) -> Result<Route, String> {
    let Some(spec) = spec else {
        return Ok(Route::Direct);
    };
    Ok(match proxy(spec)? {
        ProxySetting::Http(proxy) => Route::Http(proxy),
        ProxySetting::Socks(proxy) => Route::Socks(proxy),
    })
}

impl Route {
    /// Where the route's connections go first, `(host, port)`: its proxy.
    /// `None` when that is the target itself.
    fn first_hop(&self) -> Option<(String, u16)> {
        match self {
            Route::Direct => None,
            Route::Http(proxy) => Some((proxy.host.clone(), proxy.port)),
            Route::Socks(proxy) => Some((proxy.host.clone(), proxy.port)),
        }
    }
}

/// A SOCKS proxy, which clew speaks to itself, on the request's own
/// connection ([`Dialer`]): every read and write of the handshake bounded by
/// the connect's deadline, and the socket on the request's line, so neither a
/// proxy that stalls mid-handshake nor an abandon leaves anything behind.
///
/// ureq has its own SOCKS support, and it cannot be bounded: it runs the
/// `socks` crate — which sets no socket timeout — on a helper thread inside
/// `thread::scope`, whose end the scope waits for, so its connect timeout
/// cannot return before the handshake does. A proxy that accepted the
/// connection and never answered held the request for as long as it kept
/// the connection open.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Socks {
    version: SocksVersion,
    /// A name, or an address — an IPv6 one without its brackets.
    host: String,
    port: u16,
    /// A user name and password (decoded), when the setting names them.
    credentials: Option<(String, String)>,
}

/// The password stays out of a debug print.
impl fmt::Debug for Socks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Socks")
            .field("version", &self.version)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("credentials", &redacted(&self.credentials))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocksVersion {
    /// SOCKS4: the target by address, a name resolved here (to IPv4).
    V4,
    /// SOCKS4a: a name sent for the proxy to resolve.
    V4a,
    /// SOCKS5 (RFC 1928), with user-name and password authentication (RFC
    /// 1929) when the setting names a user: the target by address, a name
    /// resolved here.
    V5,
    /// SOCKS5, a name sent for the proxy to resolve (which also keeps the
    /// local resolver from learning where requests go).
    V5h,
}

impl SocksVersion {
    /// The scheme a setting names this version with ([`proxy`]).
    fn scheme(self) -> &'static str {
        match self {
            SocksVersion::V4 => "socks4",
            SocksVersion::V4a => "socks4a",
            SocksVersion::V5 => "socks5",
            SocksVersion::V5h => "socks5h",
        }
    }
}

/// The target of a SOCKS `CONNECT`, as it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SocksAddr {
    Ip(IpAddr),
    /// A name, for the proxy to resolve.
    Name(String),
}

/// A name resolution for a SOCKS handshake that sends an address: the
/// addresses of `(host, port)`.
type ResolveTarget<'a> = dyn Fn(&str, u16) -> io::Result<Vec<SocketAddr>> + 'a;

impl Socks {
    /// The target `host` as this version sends it: an address as itself; a
    /// name as itself for the proxy to resolve (`socks5h`, `socks4a`), or
    /// resolved here through `resolve` — to IPv4, for SOCKS4.
    ///
    /// A name resolved here that does not resolve fails saying which scheme
    /// hands the name to the proxy instead: behind a proxy, the name may be
    /// one only the proxy's side of the network knows (an internal host),
    /// and a local resolver that knows nothing of it is no reason to give up.
    fn address(&self, host: &str, port: u16, resolve: &ResolveTarget<'_>) -> io::Result<SocksAddr> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(SocksAddr::Ip(ip));
        }
        let addrs = match self.version {
            SocksVersion::V5h | SocksVersion::V4a => return Ok(SocksAddr::Name(host.to_string())),
            SocksVersion::V5 | SocksVersion::V4 => {
                resolve(host, port).map_err(|e| self.unresolved(e))?
            }
        };
        let ip = if self.version == SocksVersion::V4 {
            addrs.iter().map(SocketAddr::ip).find(IpAddr::is_ipv4)
        } else {
            addrs.first().map(SocketAddr::ip)
        };
        ip.map(SocksAddr::Ip).ok_or_else(|| {
            let what = if self.version == SocksVersion::V4 {
                "no IPv4 address, which SOCKS4 needs"
            } else {
                "no address"
            };
            self.unresolved(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{host} has {what}"),
            ))
        })
    }

    /// `e`, a failure to find the target's address here, with the scheme
    /// that has the proxy find it instead — unless it is the request's
    /// abandon, which no setting changes.
    fn unresolved(&self, e: io::Error) -> io::Error {
        if is_abandon(&e) {
            return e;
        }
        let instead = match self.version {
            SocksVersion::V4 => "socks4a",
            _ => "socks5h",
        };
        io::Error::new(
            e.kind(),
            format!(
                "{e} — a {}:// proxy is handed the address clew looks up here; for a name only \
                 the proxy's side of the network knows, set it as {instead}://, which hands the \
                 proxy the name",
                self.version.scheme()
            ),
        )
    }

    /// The handshake that opens a tunnel to `target:port` over `io`, a
    /// connection to this proxy. An answer that is no — or none, to the user
    /// name and password it was sent — fails as the proxy's
    /// ([`Socks::answered`]): saying why, in its own words, and how it
    /// failed ([`ProxyFault`]), with `PermissionDenied` when that is about
    /// credentials. Any other failure is the connection's.
    fn handshake(
        &self,
        io: &mut (impl Read + Write),
        target: &SocksAddr,
        port: u16,
    ) -> io::Result<()> {
        match self.version {
            SocksVersion::V5 | SocksVersion::V5h => self.connect_v5(io, target, port),
            SocksVersion::V4 | SocksVersion::V4a => self.connect_v4(io, target, port),
        }
    }

    /// A failure of this proxy's, saying which proxy, and how it failed
    /// (`fault`): a [`ProxyFailed`], which [`Socks::tunnel`] passes on as
    /// it is — of the kind `PermissionDenied` when it is about credentials,
    /// `Other` for anything else.
    fn answered(&self, fault: ProxyFault, why: impl std::fmt::Display) -> io::Error {
        let kind = if matches!(fault, ProxyFault::Credentials | ProxyFault::LoginUnanswered) {
            io::ErrorKind::PermissionDenied
        } else {
            io::ErrorKind::Other
        };
        let why = format!("the SOCKS proxy {} {why}", host_port(&self.host, self.port));
        io::Error::new(kind, ProxyFailed { why, fault })
    }

    /// An answer that is no, or that clew cannot use — or a target it
    /// cannot be asked for: [`ProxyFault::Refused`].
    fn failed(&self, why: impl std::fmt::Display) -> io::Error {
        self.answered(ProxyFault::Refused, why)
    }

    /// A refusal to let clew in without credentials, or with the ones the
    /// setting names: [`ProxyFault::Credentials`].
    fn refused(&self, why: impl std::fmt::Display) -> io::Error {
        self.answered(ProxyFault::Credentials, why)
    }

    /// No answer to the user name and password sent — none of RFC 1929's
    /// two-byte reply, or its version alone — but `e` instead: the
    /// connection closed or reset, or a stall to the deadline. A proxy may
    /// refuse a login that way as surely as by saying no, and nothing tells
    /// the two apart: it counts as a refusal, and they are not sent again
    /// ([`ProxyFault::LoginUnanswered`]). A stall counts too, as it does not
    /// after an http proxy's `CONNECT` ([`tunnel`]): RFC 1929's answer comes
    /// before the proxy connects anywhere, so no slow target holds it up.
    fn login_unanswered(&self, e: &io::Error) -> io::Error {
        let how = match e.kind() {
            io::ErrorKind::UnexpectedEof => "the connection closed".to_string(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => "timed out".to_string(),
            _ => e.to_string(),
        };
        self.answered(
            ProxyFault::LoginUnanswered,
            format!(
                "gave no answer to the user name and password ({how}): it may have refused them"
            ),
        )
    }

    fn connect_v5(
        &self,
        io: &mut (impl Read + Write),
        target: &SocksAddr,
        port: u16,
    ) -> io::Result<()> {
        // The methods offered: none, and user name and password when the
        // setting has them.
        let greeting: &[u8] = if self.credentials.is_some() {
            &[5, 2, 0x00, 0x02]
        } else {
            &[5, 1, 0x00]
        };
        io.write_all(greeting)?;
        io.flush()?;
        let mut choice = [0u8; 2];
        io.read_exact(&mut choice)?;
        if choice[0] != 5 {
            return Err(self.failed("does not speak SOCKS5"));
        }
        match (choice[1], &self.credentials) {
            (0x00, _) => {}
            (0x02, Some((user, password))) => {
                let (user, password) = (user.as_bytes(), password.as_bytes());
                let (Ok(user_len), Ok(password_len)) =
                    (u8::try_from(user.len()), u8::try_from(password.len()))
                else {
                    return Err(self.failed("takes no user name or password over 255 bytes"));
                };
                let mut auth = vec![1, user_len];
                auth.extend_from_slice(user);
                auth.push(password_len);
                auth.extend_from_slice(password);
                io.write_all(&auth)?;
                io.flush()?;
                // RFC 1929: the version of the exchange (1), and the status.
                let mut status = [0u8; 2];
                io.read_exact(&mut status)
                    .map_err(|e| self.login_unanswered(&e))?;
                if status[0] != 1 {
                    return Err(self.failed(
                        "answered the user name and password in something other than RFC 1929",
                    ));
                }
                if status[1] != 0 {
                    return Err(self.refused("refused the user name and password"));
                }
            }
            (0xFF, None) => return Err(self.refused("asks for a user name and password")),
            // It takes no user name and password at all: no setting of
            // clew's can let it in.
            (0xFF, Some(_)) => {
                return Err(self.failed("accepts none of the ways to log in offered"));
            }
            (other, _) => {
                return Err(self.failed(format!("chose a way to log in not offered ({other})")));
            }
        }
        // CONNECT, to an address as one, and to a name BY NAME.
        let mut request = vec![5, 1, 0];
        match target {
            SocksAddr::Ip(IpAddr::V4(ip)) => {
                request.push(1);
                request.extend_from_slice(&ip.octets());
            }
            SocksAddr::Ip(IpAddr::V6(ip)) => {
                request.push(4);
                request.extend_from_slice(&ip.octets());
            }
            SocksAddr::Name(host) => {
                let Ok(len) = u8::try_from(host.len()) else {
                    return Err(self.failed("takes no host name over 255 bytes"));
                };
                request.push(3);
                request.push(len);
                request.extend_from_slice(host.as_bytes());
            }
        }
        request.extend_from_slice(&port.to_be_bytes());
        io.write_all(&request)?;
        io.flush()?;
        let mut reply = [0u8; 4];
        io.read_exact(&mut reply)?;
        if reply[0] != 5 {
            return Err(self.failed("answered the CONNECT in something other than SOCKS5"));
        }
        if reply[1] != 0 {
            // What another attempt may get past — the target did not answer
            // the proxy, refused it, or could not be reached from it — and
            // what it would only get again (RFC 1928 §6).
            let (why, fault) = match reply[1] {
                1 => ("a general failure", ProxyFault::Unavailable),
                2 => ("not allowed by its rules", ProxyFault::Refused),
                3 => ("network unreachable", ProxyFault::Unavailable),
                4 => ("host unreachable", ProxyFault::Unavailable),
                5 => ("connection refused", ProxyFault::Unavailable),
                6 => ("TTL expired", ProxyFault::Unavailable),
                7 => ("command not supported", ProxyFault::Refused),
                8 => ("address type not supported", ProxyFault::Refused),
                _ => ("an unknown error", ProxyFault::Refused),
            };
            let target = show(target, port);
            let what = if fault == ProxyFault::Unavailable {
                format!("could not reach {target}: {why}")
            } else {
                format!("refused the CONNECT to {target}: {why}")
            };
            return Err(self.answered(fault, what));
        }
        // Then the address the proxy bound for the tunnel, which clew has
        // no use for, and its port.
        let bound = match reply[3] {
            1 => 4,
            4 => 16,
            3 => {
                let mut len = [0u8; 1];
                io.read_exact(&mut len)?;
                usize::from(len[0])
            }
            other => return Err(self.failed(format!("answered with address type {other}"))),
        };
        io.read_exact(&mut vec![0u8; bound + 2])?;
        Ok(())
    }

    fn connect_v4(
        &self,
        io: &mut (impl Read + Write),
        target: &SocksAddr,
        port: u16,
    ) -> io::Result<()> {
        let user = self
            .credentials
            .as_ref()
            .map(|(user, _)| user.as_bytes())
            .unwrap_or_default();
        let mut request = vec![4, 1];
        request.extend_from_slice(&port.to_be_bytes());
        match target {
            SocksAddr::Ip(IpAddr::V4(ip)) => {
                request.extend_from_slice(&ip.octets());
                request.extend_from_slice(user);
                request.push(0);
            }
            SocksAddr::Ip(IpAddr::V6(_)) => {
                return Err(self.failed("cannot reach an IPv6 address (SOCKS4)"));
            }
            // SOCKS4a: an address the proxy takes as "resolve the name".
            SocksAddr::Name(host) => {
                request.extend_from_slice(&[0, 0, 0, 1]);
                request.extend_from_slice(user);
                request.push(0);
                request.extend_from_slice(host.as_bytes());
                request.push(0);
            }
        }
        io.write_all(&request)?;
        io.flush()?;
        // The version of the reply (0), the answer, then a port and an
        // address clew has no use for.
        let mut reply = [0u8; 8];
        io.read_exact(&mut reply)?;
        if reply[0] != 0 {
            return Err(self.failed("answered in something other than SOCKS4"));
        }
        if reply[1] != 90 {
            let target = show(target, port);
            // 91, "rejected or failed", is the one answer most SOCKS4 proxies
            // give any failure, a target they could not reach included: one
            // another attempt may get past, as SOCKS5's general failure. 92
            // and 93 are about the user id, which identd on this machine was
            // to confirm.
            return Err(match reply[1] {
                91 => self.answered(
                    ProxyFault::Unavailable,
                    format!("could not reach {target} (SOCKS4 answer 91: rejected or failed)"),
                ),
                other => self.failed(format!(
                    "refused the CONNECT to {target} (SOCKS4 answer {other})"
                )),
            });
        }
        Ok(())
    }
}

/// `target:port` for a message, an IPv6 address in brackets.
fn show(target: &SocksAddr, port: u16) -> String {
    match target {
        SocksAddr::Ip(IpAddr::V6(ip)) => format!("[{ip}]:{port}"),
        SocksAddr::Ip(ip) => format!("{ip}:{port}"),
        SocksAddr::Name(host) => format!("{host}:{port}"),
    }
}

/// A socket whose every read and write must end by `deadline`: a proxy's
/// handshake, which a proxy that stalls must not outlast.
struct Timed<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Timed<'_> {
    /// The time left, or a timeout once there is none.
    fn left(&self) -> io::Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        }
        Ok(left)
    }
}

impl Read for Timed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.left()?))?;
        let mut stream = self.stream;
        stream.read(buf)
    }
}

impl Write for Timed<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.left()?))?;
        let mut stream = self.stream;
        stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `%XX` escapes decoded (a URL's userinfo is percent-encoded); anything else
/// kept as it is.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied(),
            bytes.get(i + 2).copied(),
        ) {
            (b'%', Some(hi), Some(lo)) if hex(hi).is_some() && hex(lo).is_some() => {
                out.push((hex(hi).unwrap_or(0) * 16 + hex(lo).unwrap_or(0)) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A proxy URL with any `user:password@` removed, for messages. Up to the
/// LAST `@`: a password may hold one.
pub(crate) fn redact_userinfo(spec: &str) -> String {
    match spec.split_once("://") {
        Some((scheme, rest)) => match rest.rsplit_once('@') {
            Some((_, host)) => format!("{scheme}://…@{host}"),
            None => spec.to_string(),
        },
        None => match spec.rsplit_once('@') {
            Some((_, host)) => format!("…@{host}"),
            None => spec.to_string(),
        },
    }
}

// ------------------------------------------------------------- connections

/// The line of one request: the sockets of the connections it is on, shared
/// by the thread making the request and the one waiting for it, so that an
/// abandon can shut the request's connection down from outside
/// ([`Line::close`]).
///
/// `shutdown(Both)` fails a read or a write blocked on the socket at once —
/// a TLS handshake, a proxy's answer, the wait for the response headers, a
/// stalled body — and closes the connection, which is what tells the peer (a
/// model provider generating an answer nobody will read) to stop. A connect
/// in progress looks at the line every [`TICK`] as well ([`connect_to`]); a
/// name lookup is only no longer waited for ([`lookup`]).
///
/// A request finds its line through the thread that makes it
/// ([`Line::tether`]): ureq makes every connection on the thread sending the
/// request, and runs no thread of its own for one (the only one clew adds is
/// the lookup's).
///
/// A connection is on the line of the request using it, and for that
/// request's time on it alone ([`Lease`]): from its connect, or from the
/// moment a kept connection is taken up again, to the end of the request. A
/// connection kept for the next request (an embeddings batch's) moves on to
/// that request's line, and the abandon of a request that is over — a
/// caller that let go late — cannot shut down the request it serves now.
#[derive(Debug, Default)]
pub(crate) struct Line(Mutex<LineState>);

#[derive(Debug, Default)]
struct LineState {
    /// The connections the line's request is on — one; or, while one is
    /// being made, one per address tried at once ([`connect_to`]) — each
    /// with the term it was taken up for ([`Lease`]). Weak: a connection
    /// dropped is off the line with it.
    held: Vec<(Weak<Lease>, u64)>,
    /// Shut down: a connection taken onto it from now on is shut down as it
    /// arrives.
    closed: bool,
}

thread_local! {
    /// The line of the requests this thread makes ([`Line::tether`]).
    static TETHERED: RefCell<Option<Arc<Line>>> = const { RefCell::new(None) };
}

/// Keeps this thread's requests on a [`Line`] until it is dropped.
pub(crate) struct Tether {
    previous: Option<Arc<Line>>,
}

impl Drop for Tether {
    fn drop(&mut self) {
        let previous = self.previous.take();
        TETHERED.with(|slot| *slot.borrow_mut() = previous);
    }
}

impl Line {
    /// Put the requests this thread makes — the connections they are on — on
    /// `line`, until the returned guard drops: for a worker, its whole life.
    pub(crate) fn tether(line: Arc<Line>) -> Tether {
        let previous = TETHERED.with(|slot| slot.borrow_mut().replace(line));
        Tether { previous }
    }

    /// The line of the requests this thread makes, if it has one.
    fn current() -> Option<Arc<Line>> {
        TETHERED.with(|slot| slot.borrow().clone())
    }

    /// Hold `lease` for `term` — or, when the line is closed already (an
    /// abandon that came first), shut its connection down at once. What the
    /// line held that is gone, or whose term is over, it lets go of.
    fn hold(&self, lease: &Arc<Lease>, term: u64) {
        let mut state = self.state();
        if state.closed {
            lease.shut_down_in(term);
            return;
        }
        state.held.retain(|(kept, kept_term)| {
            kept.upgrade()
                .is_some_and(|kept| kept.state().term == *kept_term)
        });
        state.held.push((Arc::downgrade(lease), term));
    }

    /// Shut down (both ways) every connection the line holds, as long as the
    /// term it holds it for lasts — and any taken onto it after.
    pub(crate) fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        for (lease, term) in state.held.drain(..) {
            if let Some(lease) = lease.upgrade() {
                lease.shut_down_in(term);
            }
        }
    }

    /// Whether the line was closed: a failure on it from then on is the
    /// abandon's doing, not the peer's.
    pub(crate) fn is_closed(&self) -> bool {
        self.state().closed
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LineState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A connection's standing with the requests that use it: a second handle to
/// its socket, for an abandon to shut it down from another thread, and the
/// line of the request on it now.
///
/// Each request's time on the connection is a term, numbered: one begins
/// when a request takes the connection up ([`Lease::take_up`]: at its
/// connect, and as each request's first byte goes out), and another when it
/// lets the connection go ([`Lease::let_go`]: when ureq asks whether the
/// connection can carry another request). A line holds the connection for one term, and
/// shuts it down only while that term lasts ([`Lease::shut_down_in`]) —
/// checked and done under the lease's lock, so no new term can begin in
/// between. (Lines are locked before leases, never the other way round.)
#[derive(Debug)]
pub(crate) struct Lease {
    socket: TcpStream,
    state: Mutex<LeaseState>,
}

#[derive(Debug, Default)]
struct LeaseState {
    /// The current term.
    term: u64,
    /// The line of the request in the current term, if it is on one.
    line: Option<Arc<Line>>,
}

impl Lease {
    /// The lease of a new connection, on `socket` (a second handle to it),
    /// on no line yet.
    pub(crate) fn new(socket: TcpStream) -> Arc<Lease> {
        Arc::new(Lease {
            socket,
            state: Mutex::default(),
        })
    }

    /// Begin a term for a request made on `line` (none: a thread with no
    /// line, [`Line::tether`]): the connection goes on it, and is shut down
    /// at once when the line is closed already.
    pub(crate) fn take_up(self: &Arc<Lease>, line: Option<Arc<Line>>) {
        let term = {
            let mut state = self.state();
            state.term += 1;
            state.line = line.clone();
            state.term
        };
        if let Some(line) = line {
            line.hold(self, term);
        }
    }

    /// End the current term: the request is over, and the connection off its
    /// line — an abandon of it that comes now reaches nothing.
    fn let_go(&self) {
        let mut state = self.state();
        state.term += 1;
        state.line = None;
    }

    /// Shut the connection down, if `term` is the current one.
    fn shut_down_in(&self, term: u64) {
        let state = self.state();
        if state.term == term {
            let _ = self.socket.shutdown(Shutdown::Both);
        }
    }

    /// Whether the request on the connection now was abandoned: a failure
    /// from then on is the abandon's doing, not the peer's.
    fn abandoned(&self) -> bool {
        let line = self.state().line.clone();
        line.is_some_and(|line| line.is_closed())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LeaseState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What a request's I/O fails with once its line is closed. Of its own kind
/// (`Other`): ureq takes a reset, an abort or an early end for the peer
/// ending the body, which an abandon is not.
const ABANDONED: &str = "the request was abandoned";

/// The error of an abandon ([`abandoned`]), a type of its own so that it can
/// be told from every other failure ([`is_abandon`]).
#[derive(Debug)]
struct Abandoned;

impl fmt::Display for Abandoned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(ABANDONED)
    }
}

impl std::error::Error for Abandoned {}

fn abandoned() -> io::Error {
    io::Error::other(Abandoned)
}

/// Whether `e` is an abandon's ([`abandoned`]).
fn is_abandon(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Abandoned>())
}

/// How the requests of one kind are sent: their time bounds, what one
/// response may deliver, and whether a connection outlives its request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Profile {
    /// Making a connection: the name lookup, and then TCP, a proxy's
    /// handshake and TLS, each within this.
    pub(crate) connect: Duration,
    /// The longest one read may wait for a byte — the wait for the response
    /// headers included, when the request may already be with the peer.
    pub(crate) read_idle: Option<Duration>,
    /// The longest one write may wait for the peer to take a byte.
    pub(crate) write_idle: Option<Duration>,
    /// The whole request, from the lookup to the end of the body.
    pub(crate) total: Option<Duration>,
    /// A pace the answer must keep ([`Pace`]), for a request whose answer
    /// may be large and whose link may be slow — in place of `total`, which
    /// a paced profile leaves off: a timeout of the whole request is then
    /// the pace's.
    pub(crate) pace: Option<Pace>,
    /// The most one response may deliver on its connection, all it sends
    /// counted ([`Metered`]).
    pub(crate) response_cap: u64,
    /// Whether a connection is kept for the next request to its host. A
    /// kept connection is on the line of each request that takes it up, for
    /// that request's time on it alone ([`Lease`]).
    pub(crate) pooled: bool,
    /// Whether a plain-http request is refused before anything is sent.
    pub(crate) https_only: bool,
}

/// The pace a request's answer must keep: once `grace` from the request's
/// first byte out is spent, `floor` bytes a second, held over windows of
/// `window` each — a low-speed limit, as curl's `LOW_SPEED_LIMIT` and
/// `LOW_SPEED_TIME` set one. Past the grace, each window must bring `floor`
/// bytes for every second of it — all of them, as the connection's
/// [`Meter`] counts them — and the next begins once it has; the request
/// fails at the end of a window that did not ([`Pacing::due`]).
///
/// A fixed deadline has to choose between a slow link and a trickling peer:
/// short enough to stop a peer that sends a byte now and then, never falling
/// silent long enough for an idle limit, it failed a large answer on a slow
/// link every time. A pace tells them apart: an answer that keeps coming
/// faster than `floor` makes each window up in time, and completes whatever
/// its size; one that falls behind for a window is cut off at its end.
///
/// Faster, not at: the pace is looked at once a read, so a read's worth of
/// an answer may count for no window — what came between the grace's end
/// and the first look past it, and what the read that made a window up
/// brought past its due. An answer keeps the pace when it beats `floor` by
/// a read's worth a window: at 4096 bytes a second over windows of a
/// second, a steady 410 bytes every 100 ms — 4100 a second — is cut off,
/// the first of the ten reads that end in its first window counting for
/// none of it.
///
/// Nothing is banked: what a window brings past its due counts for none
/// after it. A deadline that grew by a second for every `floor` bytes let a
/// burst buy a trickle — 40 KiB at once, then a byte every 50 ms, held a
/// request with a second's grace at 4 KiB a second for eleven seconds, and
/// all one response may deliver bought hours.
///
/// The window is not the grace. The grace is the time an answer is given to
/// start — a cold model's — and a window how far back the pace looks once
/// it applies: an answer that never keeps the pace is cut off a window past
/// the grace, and one that stops keeping it a window after it last did. A
/// window as long as the profile's idle limit ([`Profile::read_idle`]), as
/// an embeddings batch's is (`crate::embed::BATCH_PACE`), is long enough
/// that none falls within a silence that limit lets pass, and short enough
/// that a trickle is cut an idle limit past the grace — not a second grace
/// past it, as when the window was the grace. What any request may take is
/// bounded all the same, by the meter: the grace, a window, and a second
/// for every `floor` bytes of the most one response may deliver.
///
/// Enforced beneath ureq ([`Exchange`]), at one instant for each read or
/// write, which moves on as each window is made up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Pace {
    /// How long the answer is given before the pace applies.
    pub(crate) grace: Duration,
    /// How long each window the pace is held over is, once it applies.
    pub(crate) window: Duration,
    /// The rate, in bytes a second, the answer must keep.
    pub(crate) floor: u64,
}

impl Pace {
    /// What one window must bring: `floor` bytes for each of its seconds,
    /// to the byte, however short the window — at 4 KiB a second, 100.25 ms
    /// must bring 410.624 bytes, so 411.
    fn due_in_window(self) -> u64 {
        let due = (u128::from(self.floor) * self.window.as_nanos()).div_ceil(1_000_000_000);
        u64::try_from(due).unwrap_or(u64::MAX)
    }
}

/// Where the request on a connection stands with its [`Pace`]: its own,
/// from its own first byte — a kept connection's next request starts afresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pacing {
    /// When the request sent its first byte.
    started: Instant,
    /// The window its answer is in, once the grace is spent: when it began,
    /// and how much of the answer had come in by then.
    window: Option<(Instant, u64)>,
}

impl Pacing {
    fn new(started: Instant) -> Pacing {
        Pacing {
            started,
            window: None,
        }
    }

    /// The instant the request fails unless more of its answer comes in,
    /// `received` of it in by `now`: the end of the window it is in — of
    /// the first, while the grace lasts. A window that has brought its due
    /// ends there, and the next begins `now`: what it brought past its due
    /// earns nothing.
    ///
    /// The first window begins as the grace ends, and counts from what had
    /// come in by the first look past it — what came in between is not
    /// counted, a read's worth at most while the answer flows, as each read
    /// is a look.
    fn due(&mut self, pace: Pace, received: u64, now: Instant) -> Instant {
        let grace_end = self.started + pace.grace;
        if now < grace_end {
            return grace_end + pace.window;
        }
        let (began, from) = *self.window.get_or_insert((grace_end, received));
        if received.saturating_sub(from) >= pace.due_in_window() {
            self.window = Some((now, received));
            return now + pace.window;
        }
        began + pace.window
    }
}

/// The connections a pooled agent keeps, per host and in all: its requests
/// go to one endpoint, one at a time.
const KEPT_CONNECTIONS: usize = 2;

/// The agent for requests that take `route`, sent as `profile` says,
/// trusting `tls`.
///
/// ureq writes each request and parses its response; the connection under it
/// is clew's: the name lookup ([`FirstHop`]) and everything after it
/// ([`Dialer`]). Of ureq's own behaviour, every agent turns off what would
/// decide for the caller:
///
/// - ureq's proxy, which it would read from the environment: [`via`]
///   decides, and the route carries the decision;
/// - redirects, which ureq would follow with every header, and the one route,
///   to wherever a response pointed: each caller follows or refuses them;
/// - a status as an error: an error's body says why, and callers read it.
pub(crate) fn agent(route: Route, tls: Arc<rustls::ClientConfig>, profile: Profile) -> ureq::Agent {
    let kept = if profile.pooled { KEPT_CONNECTIONS } else { 0 };
    let config = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .https_only(profile.https_only)
        .user_agent(USER_AGENT)
        .timeout_resolve(Some(profile.connect))
        .timeout_connect(Some(profile.connect))
        .timeout_global(profile.total)
        .max_idle_connections(kept)
        .max_idle_connections_per_host(kept)
        .build();
    let resolver = FirstHop {
        hop: route.first_hop(),
    };
    let dialer = Dialer {
        route,
        tls,
        profile,
    };
    ureq::Agent::with_parts(config, dialer, resolver)
}

/// Where a request goes, as ureq was handed it ([`request_uri`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    https: bool,
    /// A name, or an address — an IPv6 one without its brackets.
    host: String,
    port: u16,
}

impl Target {
    fn of(uri: &Uri) -> Result<Target, ureq::Error> {
        let bad = |why: &str| ureq::Error::BadUri(format!("{uri}: {why}"));
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err(bad("not an http or https URL")),
        };
        let host = uri.host().ok_or_else(|| bad("no host"))?;
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        Ok(Target {
            https,
            host: host.to_string(),
            port: uri.port_u16().unwrap_or(if https { 443 } else { 80 }),
        })
    }

    /// `host:port`, an IPv6 host in brackets: the target of a `CONNECT`.
    fn authority(&self) -> String {
        host_port(&self.host, self.port)
    }

    /// The authority of an absolute-form request, as a URL writes it: the
    /// port left out when it is the scheme's default.
    fn url_authority(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        match (self.port == default, self.host.contains(':')) {
            (false, _) => self.authority(),
            (true, true) => format!("[{}]", self.host),
            (true, false) => self.host.clone(),
        }
    }
}

/// `url` as ureq is handed it: the `url` crate's serialization of it, without
/// the fragment (which is never sent), as the `http` crate's `Uri` ureq
/// takes — refused unless that parser reads the same scheme, host and port
/// in it as the `url` crate does. Every decision about a URL (is it HTTPS, is
/// its host loopback, which proxy applies, [`url_scheme_host`]) is made on
/// the `url` crate's reading; this is what makes it a decision about where
/// the request goes.
pub(crate) fn request_uri(url: &str) -> Result<Uri, String> {
    let mut parsed = url::Url::parse(url).map_err(|e| format!("{url}: {e}"))?;
    parsed.set_fragment(None);
    let uri: Uri = parsed.as_str().parse().map_err(|e| format!("{url}: {e}"))?;
    let read = url_scheme_host(parsed.as_str())
        .map(|(scheme, host)| (scheme == "https", host, parsed.port_or_known_default()));
    let target = Target::of(&uri).map_err(|e| describe(&e))?;
    let host = target.host.trim_end_matches('.').to_ascii_lowercase();
    if read != Some((target.https, host, Some(target.port))) {
        return Err(format!(
            "{url}: not sent — the HTTP client would read another host in it"
        ));
    }
    Ok(uri)
}

/// The deadline `timeout` sets from now. ureq passes none for a phase with no
/// bound of its own; every agent here bounds the connect ([`agent`]), so
/// that is only a fallback.
fn deadline_of(timeout: NextTimeout) -> Instant {
    let wait = timeout.not_zero().map_or(CONNECT_TIMEOUT, |wait| *wait);
    Instant::now() + wait
}

/// The [`NextTimeout`] of a step of making a connection, which must end by
/// `deadline` — or a timeout, once it has passed ([`Bound::next`]).
fn until(deadline: Instant) -> Result<NextTimeout, ureq::Error> {
    Bound::connect(deadline).next()
}

/// The name lookup of an agent: where its connections go first — the target,
/// or the proxy on its route — resolved by the system's resolver
/// ([`lookup`]), and waited for no longer than the request's line lasts. A
/// target behind a proxy is the proxy's to resolve, but for a `socks5://` or
/// `socks4://` one, which [`Socks`] resolves here too.
#[derive(Debug)]
struct FirstHop {
    /// The proxy on the route, when there is one.
    hop: Option<(String, u16)>,
}

impl ureq::unversioned::resolver::Resolver for FirstHop {
    fn resolve(
        &self,
        uri: &Uri,
        _: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let (host, port) = match &self.hop {
            Some(hop) => hop.clone(),
            None => {
                let target = Target::of(uri).map_err(unreached)?;
                (target.host, target.port)
            }
        };
        let line = Line::current();
        let found = lookup(&host, port, deadline_of(timeout), line.as_deref()).map_err(|e| {
            let proxied = self.hop.is_some() && !line.as_deref().is_some_and(Line::is_closed);
            unreached(if proxied {
                proxy_failed(
                    format!("could not look up the proxy {host}: {e}"),
                    ProxyFault::Unanswered,
                )
            } else {
                e.into()
            })
        })?;
        let mut addrs = self.empty();
        for addr in found {
            // As many as ureq has room for (sixteen).
            if addrs.try_push(addr).is_err() {
                break;
            }
        }
        Ok(addrs)
    }
}

/// A resolver for [`lookup_with`]: the system's, or a test's. It is asked
/// for a name's addresses; the port is its caller's to add.
type Resolve = dyn Fn(&str) -> io::Result<Vec<IpAddr>> + Send + Sync;

/// The system resolver (`getaddrinfo`).
fn system_resolve(host: &str) -> io::Result<Vec<IpAddr>> {
    (host, 0)
        .to_socket_addrs()
        .map(|addrs| addrs.map(|addr| addr.ip()).collect())
}

/// A resolver's answer as every caller waiting on it gets it: the addresses,
/// or the failure's kind and words (one `io::Error` cannot go to many).
type Answer = Result<Vec<IpAddr>, (io::ErrorKind, String)>;

/// A lookup under way: the resolver's answer, once it has given one, for
/// every caller waiting on the lookup of that name.
#[derive(Default)]
struct Pending {
    answer: Mutex<Option<Answer>>,
    answered: std::sync::Condvar,
}

/// The lookups under way, one per name, each on a thread of its own
/// ([`lookup`]).
struct Lookups(Mutex<Vec<(String, Arc<Pending>)>>);

/// This process's lookups.
static LOOKUPS: Lookups = Lookups(Mutex::new(Vec::new()));

/// How many names may be looked up at once: each lookup holds a thread
/// inside the system's resolver until the resolver answers ([`lookup`]).
/// Far more names than clew's requests go to at once — a model provider, an
/// embeddings endpoint, a release host, a proxy — and a bound on the threads
/// a resolver that answers nothing can take.
const MAX_LOOKUPS: usize = 16;

impl Lookups {
    /// The lookup of `name` under way — joined, or begun on a thread of its
    /// own, which `resolve` answers — or a refusal, while [`MAX_LOOKUPS`]
    /// other names are being looked up. Joining or beginning is one step,
    /// under the list's lock: callers side by side cannot take the list past
    /// its bound.
    fn join(&'static self, name: &str, resolve: &'static Resolve) -> io::Result<Arc<Pending>> {
        let mut under_way = self.list();
        if let Some((_, pending)) = under_way.iter().find(|(looked_up, _)| looked_up == name) {
            return Ok(pending.clone());
        }
        if under_way.len() >= MAX_LOOKUPS {
            return Err(io::Error::other(format!(
                "not looking up {name}: the system's resolver has yet to answer the lookups of \
                 {} other names",
                under_way.len()
            )));
        }
        let pending = Arc::new(Pending::default());
        let job = {
            let (pending, name) = (pending.clone(), name.to_string());
            move || {
                // A resolver that fails by panicking answers too: its name
                // must not stay on the list, waited on for good.
                let answer =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| resolve(&name)))
                        .unwrap_or_else(|_| Err(io::Error::other("the resolver failed")))
                        .map_err(|e| (e.kind(), e.to_string()));
                // Off the list before the answer is out: a lookup begun
                // after this asks the resolver afresh.
                self.list()
                    .retain(|(_, under_way)| !Arc::ptr_eq(under_way, &pending));
                *pending.answer.lock().unwrap_or_else(|e| e.into_inner()) = Some(answer);
                pending.answered.notify_all();
            }
        };
        std::thread::Builder::new()
            .name("clew-lookup".into())
            .spawn(job)
            .map_err(|e| io::Error::other(format!("could not look up {name}: {e}")))?;
        under_way.push((name.to_string(), pending.clone()));
        Ok(pending)
    }

    fn list(&self) -> std::sync::MutexGuard<'_, Vec<(String, Arc<Pending>)>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The addresses of `host`, as the system resolver (`getaddrinfo`) finds
/// them — it knows macOS's scoped and VPN split DNS, which a resolver of
/// clew's own would not — waited for until `deadline`, or until `line`
/// closes.
///
/// - An address is its own answer, and `localhost` and every name under it
///   are this machine's loopback addresses (RFC 6761 §6.3), whatever a
///   resolver would say: no resolver is asked, so none that hangs can keep a
///   local model server out of reach.
/// - A lookup cannot be interrupted, so it runs on a thread of its own, which
///   the caller stops waiting for at the deadline or the abandon — and which
///   stays in the resolver until the resolver answers: the one part of a
///   request an abandon cannot end. (ureq's own resolver starts such a
///   thread for every lookup with a timeout, and leaves each one it gives up
///   on behind.)
/// - One lookup per name is under way at a time: a caller that finds one
///   waits on it rather than starting another, so a name the resolver does
///   not answer holds one thread however many requests retry it, and holds
///   up no other name's lookups.
/// - At most [`MAX_LOOKUPS`] names are looked up at once; past that, a new
///   name is refused at once — a resolver that answers nothing costs that
///   many threads, and no more.
///
/// What one lookup per name costs, accepted: a lookup that began before the
/// network changed (a VPN connecting, another network joined) is waited on
/// until the resolver answers it — for as long as the resolver takes to give
/// up on a server it can no longer reach — and not asked again meanwhile.
/// ureq looks a request's host up before it looks for a kept connection to
/// it, so even a request that would reuse one waits. Each request waits no
/// longer than its own deadline, and the wait ends with that lookup, within
/// the resolver's own timeout. A second lookup of the name, begun beside a
/// stale one, would give up the bound on the threads one name can hold (or
/// need a bound of its own), and a request that went on with the addresses
/// found before the change could go to the wrong ones.
fn lookup(
    host: &str,
    port: u16,
    deadline: Instant,
    line: Option<&Line>,
) -> io::Result<Vec<SocketAddr>> {
    lookup_with(&system_resolve, &LOOKUPS, host, port, deadline, line)
}

/// [`lookup`] with the resolver and the list of lookups under way given — a
/// test's own.
fn lookup_with(
    resolve: &'static Resolve,
    lookups: &'static Lookups,
    host: &str,
    port: u16,
    deadline: Instant,
    line: Option<&Line>,
) -> io::Result<Vec<SocketAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    if name == "localhost" || name.ends_with(".localhost") {
        return Ok(vec![
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
            SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
        ]);
    }
    // Asked as it is spelled: a trailing dot, for one, keeps a resolver's
    // search domains off the name.
    let pending = lookups.join(host, resolve)?;
    let mut answer = pending.answer.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        match answer.as_ref() {
            Some(Ok(ips)) if ips.is_empty() => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{host} has no address"),
                ));
            }
            Some(Ok(ips)) => return Ok(ips.iter().map(|ip| SocketAddr::new(*ip, port)).collect()),
            Some(Err((kind, why))) => {
                return Err(io::Error::new(
                    *kind,
                    format!("could not look up {host}: {why}"),
                ));
            }
            None => {}
        }
        if line.is_some_and(Line::is_closed) {
            return Err(abandoned());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out looking up {host}"),
            ));
        }
        answer = pending
            .answered
            .wait_timeout(answer, (deadline - now).min(TICK))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

/// How long an attempt at one address runs alone before the next address is
/// tried beside it: RFC 8305's recommended Connection Attempt Delay.
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// Makes the socket an attempt at `addr` connects on — the system's, or a
/// test's that fails as a host without one family would.
type Open = dyn Fn(SocketAddr) -> io::Result<socket2::Socket>;

/// A new socket for a TCP connection to `addr`.
fn open_socket(addr: SocketAddr) -> io::Result<socket2::Socket> {
    socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
}

/// A TCP connection to one of `addrs` (the first hop's, in the resolver's
/// order) by `deadline` — its socket, and the lease that has it on `line` —
/// made as RFC 8305's Happy Eyeballs makes one:
///
/// - the addresses are taken in turns by family, starting with the family of
///   the first ([`interleaved`]): on a network that drops one family without
///   a word, the first address of the other is tried within
///   [`ATTEMPT_DELAY`], not after every address of the first has timed out;
/// - the next attempt starts every [`ATTEMPT_DELAY`] — at once when every
///   attempt so far has failed — and the attempts run side by side; the first
///   to connect wins, and the others are closed;
/// - any failure of an attempt — refused, unreachable, a family the host has
///   no network for, a firewall's refusal — is its address's alone, and the
///   next address is tried;
/// - when none connects, the failure is the one that says most of the
///   target, and names the other addresses and how each failed
///   ([`unconnected`]).
///
/// Every attempt's socket is on `line` before it connects, so that an
/// abandon ends every attempt at once, and the wait for them looks at the
/// line every [`TICK`] too. std connects only with a timeout nothing can cut
/// short, and one address at a time.
fn connect_to(
    addrs: &[SocketAddr],
    deadline: Instant,
    line: Option<&Arc<Line>>,
) -> io::Result<(TcpStream, Arc<Lease>)> {
    connect_with(addrs, deadline, line, &open_socket)
}

/// One address being connected to: its place in the order tried, its
/// socket, connecting without blocking, and the lease that has it on the
/// request's line.
struct Attempt {
    at: usize,
    addr: SocketAddr,
    socket: socket2::Socket,
    lease: Arc<Lease>,
}

/// An address a connect tried that failed: its place in the order tried,
/// and how it failed.
type Failed = (usize, SocketAddr, io::Error);

/// [`connect_to`], with the socket of each attempt made by `open`.
fn connect_with(
    addrs: &[SocketAddr],
    deadline: Instant,
    line: Option<&Arc<Line>>,
    open: &Open,
) -> io::Result<(TcpStream, Arc<Lease>)> {
    let mut waiting = interleaved(addrs).into_iter().enumerate().peekable();
    let mut attempts: Vec<Attempt> = Vec::new();
    let mut failed: Vec<Failed> = Vec::new();
    // When the next attempt may start.
    let mut next_start = Instant::now();
    loop {
        if line.is_some_and(|line| line.is_closed()) {
            return Err(abandoned());
        }
        let now = Instant::now();
        if now >= deadline {
            // The attempts under way — what the deadline ended — ahead of
            // the failures before them.
            let timed_out = (!attempts.is_empty() || failed.is_empty()).then(|| {
                let trying: Vec<String> = attempts.iter().map(|a| a.addr.to_string()).collect();
                let what = if trying.is_empty() {
                    "timed out connecting".to_string()
                } else {
                    format!("timed out connecting to {}", trying.join(", "))
                };
                io::Error::new(io::ErrorKind::TimedOut, what)
            });
            return Err(unconnected(timed_out, failed));
        }
        if attempts.is_empty() || now >= next_start {
            match waiting.next() {
                Some((at, addr)) => {
                    match start_attempt(at, addr, line, open) {
                        Ok(attempt) => {
                            attempts.push(attempt);
                            next_start = now + ATTEMPT_DELAY;
                        }
                        // That address's failure: the next one, at once.
                        Err(e) => {
                            failed.push((at, addr, e));
                            next_start = now;
                        }
                    }
                    continue;
                }
                None if attempts.is_empty() => return Err(unconnected(None, failed)),
                None => {}
            }
        }
        let wake = if waiting.peek().is_some() {
            next_start.min(deadline)
        } else {
            deadline
        };
        let mut polled: Vec<libc::pollfd> = attempts
            .iter()
            .map(|attempt| libc::pollfd {
                fd: attempt.socket.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            })
            .collect();
        // At least a millisecond: a wait of none returns at once, and spins.
        let millis = wake
            .saturating_duration_since(now)
            .min(TICK)
            .as_millis()
            .max(1) as libc::c_int;
        // SAFETY: `polled` holds one valid `pollfd` per attempt, for a socket
        // `attempts` keeps open throughout, and is as long as it says.
        let ready =
            unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, millis) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
            continue;
        }
        // The attempts that ended, the last first, so the rest keep their
        // places: the first that connected wins, and one that failed makes
        // way for the next address.
        for at in (0..attempts.len()).rev() {
            if polled[at].revents == 0 {
                continue;
            }
            let attempt = attempts.swap_remove(at);
            match connected(&attempt.socket) {
                Ok(()) => {
                    // The others go: dropped, their sockets close.
                    drop(attempts);
                    if line.is_some_and(|line| line.is_closed()) {
                        return Err(abandoned());
                    }
                    attempt.socket.set_nonblocking(false)?;
                    return Ok((attempt.socket.into(), attempt.lease));
                }
                Err(e) => {
                    failed.push((attempt.at, attempt.addr, e));
                    next_start = Instant::now();
                }
            }
        }
    }
}

/// The failure of a connect no address took: `timed_out`, the attempts
/// still under way at the deadline, when that is what ended it — and each
/// address that failed before, with how (`failed`).
///
/// The failure that leads is the one that says most of the target
/// ([`telling`]), the first tried of those that say as much; the others
/// follow it, a few at most, each with its address. With one address and
/// one failure, that failure as it is. Only the last failure was reported:
/// with a server that was down (its IPv4 port refused) on a host with no
/// IPv6 (every IPv6 address unreachable), which came last decided whether
/// the error said so or read as a network fault.
fn unconnected(timed_out: Option<io::Error>, mut failed: Vec<Failed>) -> io::Error {
    /// How many of the other addresses' failures are named.
    const NAMED: usize = 3;
    failed.sort_by_key(|(at, _, _)| *at);
    let (lead, others) = match timed_out {
        Some(timed_out) => (timed_out, failed),
        None => {
            let Some(best) = (0..failed.len()).min_by_key(|&i| telling(&failed[i].2)) else {
                return io::Error::new(io::ErrorKind::NotFound, "no address to connect to");
            };
            let (_, addr, e) = failed.remove(best);
            if failed.is_empty() {
                return e;
            }
            (io::Error::new(e.kind(), format!("{e} at {addr}")), failed)
        }
    };
    if others.is_empty() {
        return lead;
    }
    let named: Vec<String> = others
        .iter()
        .take(NAMED)
        .map(|(_, addr, e)| format!("{addr}: {e}"))
        .collect();
    let mut what = format!("{lead}; also tried {}", named.join(", "));
    if others.len() > NAMED {
        what.push_str(&format!(", and {} more", others.len() - NAMED));
    }
    io::Error::new(lead.kind(), what)
}

/// How much a failure to connect says of the target, most first: a refusal
/// (the host itself answered: its port is closed) or a timeout (nothing
/// answered at all); then any other failure; and last what may be this
/// machine's alone, or one family's: no route to the network or the host, a
/// network that is down, an address or a family it has none for, its own
/// firewall's refusal (`EACCES`, `EPERM`).
fn telling(e: &io::Error) -> u8 {
    match e.kind() {
        io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut => 0,
        io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkDown
        | io::ErrorKind::AddrNotAvailable
        | io::ErrorKind::PermissionDenied => 2,
        _ if matches!(
            e.raw_os_error(),
            Some(libc::EAFNOSUPPORT | libc::EPFNOSUPPORT | libc::EPROTONOSUPPORT)
        ) =>
        {
            2
        }
        _ => 1,
    }
}

/// Start an attempt at `addr`, the `at`th tried: a socket `open` makes, on
/// `line` before its connect begins, connecting without blocking.
fn start_attempt(
    at: usize,
    addr: SocketAddr,
    line: Option<&Arc<Line>>,
    open: &Open,
) -> io::Result<Attempt> {
    let socket = open(addr)?;
    socket.set_nonblocking(true)?;
    let lease = Lease::new(socket.try_clone()?.into());
    lease.take_up(line.cloned());
    match socket.connect(&addr.into()) {
        Ok(()) => {}
        // Under way; an interrupted connect goes on without its caller too.
        Err(e)
            if e.raw_os_error() == Some(libc::EINPROGRESS)
                || e.kind() == io::ErrorKind::Interrupted => {}
        Err(e) => return Err(e),
    }
    Ok(Attempt {
        at,
        addr,
        socket,
        lease,
    })
}

/// Whether the connect in progress on `socket`, which is ready, succeeded:
/// no error pending, and a peer — or the error it failed with.
fn connected(socket: &socket2::Socket) -> io::Result<()> {
    if let Some(e) = socket.take_error()? {
        return Err(e);
    }
    socket.peer_addr().map(|_| ())
}

/// `addrs` in turns by family, starting with the family of the first (RFC
/// 8305 §4), each family in the order it came in.
fn interleaved(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = addrs.first() else {
        return Vec::new();
    };
    let (same, other): (Vec<SocketAddr>, Vec<SocketAddr>) = addrs
        .iter()
        .partition(|addr| addr.is_ipv4() == first.is_ipv4());
    let (mut same, mut other) = (same.into_iter(), other.into_iter());
    let mut turns = Vec::with_capacity(addrs.len());
    loop {
        match (same.next(), other.next()) {
            (None, None) => return turns,
            (one, another) => turns.extend(one.into_iter().chain(another)),
        }
    }
}

/// The agent's connector: every connection of its requests, made the way
/// the route says, bounded and metered as the profile says.
///
/// For each connection: TCP to the first hop ([`connect_to`]), its socket on
/// the line of the request that makes it ([`Lease`]); the SOCKS handshake,
/// when the route has one ([`Socks`]); the byte meter
/// ([`Metered`]); TLS to an `https://` proxy and its `CONNECT` ([`tunnel`]),
/// when the route goes through an http proxy and the target is https; TLS to
/// the target ([`tls_over`]); and on top the request boundary ([`Exchange`]).
#[derive(Debug)]
struct Dialer {
    route: Route,
    tls: Arc<rustls::ClientConfig>,
    profile: Profile,
}

impl ureq::unversioned::transport::Connector for Dialer {
    type Out = Box<dyn Transport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _: Option<()>,
    ) -> Result<Option<Box<dyn Transport>>, ureq::Error> {
        self.dial(details).map(Some).map_err(unreached)
    }
}

impl Dialer {
    fn dial(&self, details: &ConnectionDetails) -> Result<Box<dyn Transport>, ureq::Error> {
        let target = Target::of(details.uri)?;
        let deadline = deadline_of(details.timeout);
        let line = Line::current();
        let abandoned_now = || line.as_deref().is_some_and(Line::is_closed);
        let (stream, lease) = connect_to(&details.addrs, deadline, line.as_ref()).map_err(|e| {
            match self.route.first_hop() {
                Some((host, port)) if !abandoned_now() => proxy_failed(
                    format!(
                        "could not connect to the proxy {}: {e}",
                        host_port(&host, port)
                    ),
                    ProxyFault::Unanswered,
                ),
                _ => ureq::Error::from(e),
            }
        })?;
        let _ = stream.set_nodelay(true);
        if let Route::Socks(socks) = &self.route {
            socks.tunnel(&stream, &target, deadline, line.as_deref())?;
        }
        let buffers = (
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        let meter = Arc::new(Meter::new(self.profile.response_cap));
        let tcp = Tcp {
            stream,
            buffers: LazyBuffers::new(buffers.0, buffers.1),
            read_idle: self.profile.read_idle,
            write_idle: self.profile.write_idle,
            lease: lease.clone(),
            read_timeout: None,
            write_timeout: None,
        };
        let mut conn: Box<dyn Transport> = Box::new(Metered {
            inner: tcp,
            meter: meter.clone(),
        });
        let mut absolute = None;
        if let Route::Http(proxy) = &self.route {
            if proxy.tls {
                conn = tls_over(conn, &proxy.host, &self.tls, deadline, buffers).map_err(|e| {
                    if abandoned_now() {
                        return e;
                    }
                    let name = host_port(&proxy.host, proxy.port);
                    proxy_failed(
                        format!("TLS with the proxy {name} failed: {}", describe(&e)),
                        ProxyFault::Unanswered,
                    )
                })?;
            }
            if target.https {
                conn = tunnel(conn, proxy, &target, deadline, line.as_deref())?;
            } else {
                absolute = Some(Absolute {
                    authority: target.url_authority(),
                    authorization: proxy.authorization(),
                });
            }
        }
        if target.https {
            conn = tls_over(conn, &target.host, &self.tls, deadline, buffers)?;
        }
        Ok(Box::new(Exchange {
            inner: conn,
            meter,
            absolute,
            tls: target.https,
            fresh: true,
            lease,
            answered: Answered::Nothing,
            pace: self.profile.pace,
            pacing: None,
        }))
    }
}

impl Socks {
    /// Open a tunnel to `target` over `stream`, a connection to this proxy,
    /// by `deadline`: the target's address as this version sends it (a name
    /// resolved here for `socks5://` and `socks4://`), then the handshake.
    fn tunnel(
        &self,
        stream: &TcpStream,
        target: &Target,
        deadline: Instant,
        line: Option<&Line>,
    ) -> Result<(), ureq::Error> {
        let resolve = |host: &str, port: u16| lookup(host, port, deadline, line);
        // A name that does not resolve is the target's failure, not the
        // proxy's.
        let address = self.address(&target.host, target.port, &resolve)?;
        let mut io = Timed { stream, deadline };
        self.handshake(&mut io, &address, target.port).map_err(|e| {
            if line.is_some_and(Line::is_closed) {
                return abandoned().into();
            }
            // The proxy's answer, in its own words and with how it failed
            // (`Socks::answered`).
            if e.get_ref().is_some_and(|inner| inner.is::<ProxyFailed>()) {
                return e.into();
            }
            let name = host_port(&self.host, self.port);
            let why = match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                    format!("timed out: the SOCKS proxy {name} did not finish its handshake")
                }
                io::ErrorKind::UnexpectedEof => {
                    format!("the SOCKS proxy {name} closed the connection in its handshake")
                }
                _ => format!("the SOCKS proxy {name} broke off its handshake: {e}"),
            };
            proxy_failed(why, ProxyFault::Unanswered)
        })
    }
}

/// `host:port`, an IPv6 host in brackets.
fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// A connection's TCP socket as ureq's transport: every read and write
/// bounded by ureq's timeout for the phase and by the profile's idle limit,
/// whichever ends first, and a failure after an abandon reported as one.
#[derive(Debug)]
struct Tcp {
    stream: TcpStream,
    buffers: LazyBuffers,
    read_idle: Option<Duration>,
    write_idle: Option<Duration>,
    /// The socket's standing with the request on it: a read that ends (or
    /// fails) once that request was abandoned was cut short by the abandon.
    lease: Arc<Lease>,
    /// The read timeout the socket has now, once one was set — so an
    /// unchanged one costs no system call.
    read_timeout: Option<Option<Duration>>,
    /// The same for writes.
    write_timeout: Option<Option<Duration>>,
}

/// The wait for one read or write: ureq's timeout for the phase, or `idle`
/// when that ends first — and whether it is `idle`.
fn bound(timeout: NextTimeout, idle: Option<Duration>) -> (Option<Duration>, bool) {
    let phase = timeout.not_zero().map(|wait| *wait);
    match (phase, idle) {
        (Some(phase), Some(idle)) if idle < phase => (Some(idle), true),
        (None, Some(idle)) => (Some(idle), true),
        (phase, _) => (phase, false),
    }
}

impl Tcp {
    fn abandoned(&self) -> bool {
        self.lease.abandoned()
    }

    /// `e`, from a read (`receiving`) or a write, as the error ureq gets.
    fn failed(
        &self,
        e: io::Error,
        timeout: NextTimeout,
        idle: bool,
        receiving: bool,
    ) -> ureq::Error {
        if self.abandoned() {
            return abandoned().into();
        }
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut if idle => {
                let limit = if receiving {
                    self.read_idle
                } else {
                    self.write_idle
                };
                let seconds = limit.unwrap_or_default().as_secs_f32();
                let what = if receiving {
                    "sent nothing"
                } else {
                    "took nothing in"
                };
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out: the peer {what} for {seconds} s"),
                )
                .into()
            }
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                ureq::Error::Timeout(timeout.reason)
            }
            _ => ureq::Error::Io(e),
        }
    }
}

impl Transport for Tcp {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    /// Send the first `amount` bytes of the output — at most the output
    /// buffer, ureq's 128 KiB — all of them by one instant ([`Bound`]): each
    /// write is given what is left of it as the socket's send timeout, or
    /// the idle limit when that is shorter. `write_all` under one socket
    /// timeout gave each of its writes the whole of it afresh, so a peer that
    /// took the output a little at a time held a request's sending for twice
    /// its overall deadline, and more.
    ///
    /// What a send timeout bounds is not the same everywhere. Linux counts
    /// it once for the whole of a write. macOS counts it for each wait inside
    /// one — for the peer to make room — and starts it over each time the
    /// peer does: there a write runs past the instant while the peer keeps
    /// taking its bytes in, never pausing as long as the timeout, until the
    /// socket has taken all of them. The most a write holds is what keeps the
    /// bound: what runs over is the peer taking in the rest of one write,
    /// 128 KiB at most, never a whole request of any size — and once that is
    /// in, nothing is left of the instant, and the next write fails before it
    /// starts.
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        let due = Bound::phase(timeout);
        let mut sent = 0;
        while sent < amount {
            let timeout = match due.next() {
                Ok(timeout) => timeout,
                Err(_) if self.abandoned() => return Err(abandoned().into()),
                Err(e) => return Err(e),
            };
            let (wait, idle) = bound(timeout, self.write_idle);
            if self.write_timeout != Some(wait) {
                if let Err(e) = self.stream.set_write_timeout(wait) {
                    return Err(self.failed(e, timeout, idle, false));
                }
                self.write_timeout = Some(wait);
            }
            match self.stream.write(&self.buffers.output()[sent..amount]) {
                Ok(0) => {
                    let e = io::Error::from(io::ErrorKind::WriteZero);
                    return Err(self.failed(e, timeout, idle, false));
                }
                Ok(n) => sent += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(self.failed(e, timeout, idle, false)),
            }
        }
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let (wait, idle) = bound(timeout, self.read_idle);
        if self.read_timeout != Some(wait) {
            if let Err(e) = self.stream.set_read_timeout(wait) {
                return Err(self.failed(e, timeout, idle, true));
            }
            self.read_timeout = Some(wait);
        }
        if self.buffers.input_append_buf().is_empty() {
            return Err(unending_line(&self.buffers));
        }
        let read = loop {
            match self.stream.read(self.buffers.input_append_buf()) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                read => break read,
            }
        };
        match read {
            Ok(0) if self.abandoned() => Err(abandoned().into()),
            Ok(n) => {
                self.buffers.input_appended(n);
                Ok(n > 0)
            }
            Err(e) => Err(self.failed(e, timeout, idle, true)),
        }
    }

    /// Whether the connection can carry another request: open, and with
    /// nothing unasked-for waiting on it — a peek that does not wait.
    fn is_open(&mut self) -> bool {
        if self.abandoned() {
            return false;
        }
        let mut probe = [0u8; 1];
        // SAFETY: a socket this transport owns, and a one-byte buffer.
        let peeked = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
                probe.as_mut_ptr().cast(),
                probe.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        peeked < 0 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock
    }
}

/// The error of a connection whose input buffer is full of bytes ureq's
/// parser cannot use yet: a line — a header, a chunk's size, a trailer —
/// that has not ended within the buffer's size.
fn unending_line(buffers: &dyn Buffers) -> ureq::Error {
    let kib = buffers.input().len() / 1024;
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the response has a line longer than {kib} KiB"),
    )
    .into()
}

/// The error of a connection that delivered more than one response may.
const OVER_LIMIT: &str = "the connection delivered more than the byte limit of one response";

/// What one response may still deliver on a connection: counted at the
/// bottom of the connection ([`Metered`]) and started afresh at its top with
/// every request ([`Exchange`]), so it bounds one response — a connection
/// kept for more than one request is not failed for what they received
/// together.
#[derive(Debug)]
struct Meter {
    cap: u64,
    /// Only the connection's own thread counts; atomic because a transport
    /// is `Sync`.
    left: AtomicU64,
}

impl Meter {
    fn new(cap: u64) -> Meter {
        Meter {
            cap,
            left: AtomicU64::new(cap),
        }
    }

    fn restart(&self) {
        self.left.store(self.cap, Ordering::Relaxed);
    }

    /// What the response has delivered so far.
    fn received(&self) -> u64 {
        self.cap - self.left.load(Ordering::Relaxed)
    }

    /// Count `n` more bytes in, failing once there are more than the cap.
    fn count(&self, n: u64) -> Result<(), ureq::Error> {
        let left = self.left.load(Ordering::Relaxed);
        if n > left {
            self.left.store(0, Ordering::Relaxed);
            return Err(io::Error::other(OVER_LIMIT).into());
        }
        self.left.store(left - n, Ordering::Relaxed);
        Ok(())
    }
}

/// A connection that delivers at most its [`Meter`]'s worth to one
/// response, all it sends counted: the status line and headers, chunked
/// framing, trailers, a proxy's answer, TLS records.
///
/// The caps of the callers bind a response's DECODED body; everything ureq
/// takes in besides it is out of their sight. Its buffer bounds what one
/// line may cost (see [`unending_line`]), but not how many lines come: a
/// peer that followed a body with an endless run of trailer lines — or sent
/// a body of one-byte chunks, each in a frame many times its size — kept a
/// connection, and its thread, busy with no body byte to show for it.
#[derive(Debug)]
struct Metered<T> {
    inner: T,
    meter: Arc<Meter>,
}

impl<T: Transport> Transport for Metered<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        // What the read appends to what was there before is what came in.
        let before = self.inner.buffers().input().len();
        let progress = self.inner.await_input(timeout)?;
        let after = self.inner.buffers().input().len();
        self.meter.count(after.saturating_sub(before) as u64)?;
        Ok(progress)
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

/// A TLS session on a connection — to the target, or to an `https://` proxy.
struct Tls {
    stream: rustls::StreamOwned<rustls::ClientConnection, Under>,
    buffers: LazyBuffers,
}

impl fmt::Debug for Tls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tls")
            .field("under", &self.stream.sock.transport)
            .finish()
    }
}

/// The connection under a TLS session, as the `Read` and `Write` rustls
/// speaks through: every read and write bounded by one instant ([`Bound`]),
/// and every byte handed to rustls followed through TLS's framing
/// ([`Framing`]).
struct Under {
    transport: Box<dyn Transport>,
    bound: Bound,
    framing: Framing,
}

/// What bounds the reads and writes of an [`Under`] — and the writes of one
/// send, a piece at a time ([`Tcp`]'s, [`send_all`]'s): the instant each of
/// them must end by (each wait inside a write, on macOS, which bounds a
/// write no further: see `Tcp`'s `transmit_output`), and what a timeout
/// then is — or no instant, in a phase ureq bounds with none, where every
/// read and write waits as long as the connection's idle limit lets it
/// ([`Tcp`]).
///
/// One instant, not one timeout handed to each: one read of rustls's is as
/// many reads below it as its peer takes to complete a record, and each of
/// them given the whole timeout afresh let a peer that trickles a record a
/// byte at a time hold a read — a handshake, the answer to a `CONNECT`, a
/// request's overall deadline — for many times its bound. (ureq's own
/// `TransportAdapter` does exactly that with a handshake's timeout.) One
/// send is as many writes as its peer takes pieces of it, alike.
#[derive(Debug, Clone, Copy)]
struct Bound {
    due: Option<Instant>,
    reason: Timeout,
}

impl Bound {
    /// A step of making a connection, which must end by `deadline`.
    fn connect(deadline: Instant) -> Bound {
        Bound {
            due: Some(deadline),
            reason: Timeout::Connect,
        }
    }

    /// ureq's `timeout` for one call it makes, fixed from now: every read
    /// and write that call takes below it shares it.
    fn phase(timeout: NextTimeout) -> Bound {
        let due = if timeout.after.is_not_happening() {
            None
        } else {
            Instant::now().checked_add(*timeout.after)
        };
        Bound {
            due,
            reason: timeout.reason,
        }
    }

    /// The timeout of the next read or write: what is left of the bound —
    /// or, once nothing is, the timeout itself (ureq would grant a zero
    /// timeout a second of grace).
    fn next(self) -> Result<NextTimeout, ureq::Error> {
        let Some(due) = self.due else {
            return Ok(NextTimeout {
                after: ureq::unversioned::transport::time::Duration::NotHappening,
                reason: self.reason,
            });
        };
        let left = due.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ureq::Error::Timeout(self.reason));
        }
        Ok(NextTimeout {
            after: left.into(),
            reason: self.reason,
        })
    }
}

/// Where the bytes handed to rustls stand in TLS's framing of them into
/// records — a five-byte header whose last two bytes are the length of the
/// body that follows: whether rustls holds the start of a record whose rest
/// has not come ([`Tls::is_open`]).
#[derive(Debug, Default)]
struct Framing {
    /// The next record's header, as far as it has come.
    header: [u8; 5],
    /// How much of it has.
    seen: usize,
    /// How much of the current record's body is still to come.
    left: usize,
}

impl Framing {
    /// Follow `bytes`, the next ones handed to rustls.
    fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.left > 0 {
                let n = self.left.min(bytes.len());
                self.left -= n;
                bytes = &bytes[n..];
                continue;
            }
            let n = (self.header.len() - self.seen).min(bytes.len());
            self.header[self.seen..self.seen + n].copy_from_slice(&bytes[..n]);
            self.seen += n;
            bytes = &bytes[n..];
            if self.seen == self.header.len() {
                self.left = usize::from(u16::from_be_bytes([self.header[3], self.header[4]]));
                self.seen = 0;
            }
        }
    }

    /// Whether every record begun has ended.
    fn at_boundary(&self) -> bool {
        self.seen == 0 && self.left == 0
    }
}

impl Read for Under {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = self.bound.next().map_err(ureq::Error::into_io)?;
        self.transport
            .maybe_await_input(timeout)
            .map_err(ureq::Error::into_io)?;
        let input = self.transport.buffers().input();
        let n = buf.len().min(input.len());
        buf[..n].copy_from_slice(&input[..n]);
        self.transport.buffers().input_consume(n);
        self.framing.feed(&buf[..n]);
        Ok(n)
    }
}

impl Write for Under {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let timeout = self.bound.next().map_err(ureq::Error::into_io)?;
        let output = self.transport.buffers().output();
        let n = buf.len().min(output.len());
        output[..n].copy_from_slice(&buf[..n]);
        self.transport
            .transmit_output(n, timeout)
            .map_err(ureq::Error::into_io)?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// TLS over `inner` with `host` (a name or an address), verified against
/// `trust`; the handshake must end by `deadline`.
fn tls_over(
    inner: Box<dyn Transport>,
    host: &str,
    trust: &Arc<rustls::ClientConfig>,
    deadline: Instant,
    (input, output): (usize, usize),
) -> Result<Box<dyn Transport>, ureq::Error> {
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{host} is no name TLS can verify: {e}"),
        )
    })?;
    let mut session =
        rustls::ClientConnection::new(trust.clone(), name).map_err(io::Error::other)?;
    let mut under = Under {
        transport: inner,
        bound: Bound::connect(deadline),
        framing: Framing::default(),
    };
    session.complete_io(&mut under)?;
    Ok(Box::new(Tls {
        stream: rustls::StreamOwned::new(session, under),
        buffers: LazyBuffers::new(input, output),
    }))
}

/// How a TLS session whose connection ended under it — with no
/// close_notify from the peer, or with a reset — fails a read
/// ([`Tls::await_input`]): as the session's, and once [`Exchange`] knows
/// how the answer being read stood, as the answer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CutShort {
    /// The session's end, whatever it was carrying.
    Session,
    /// Before anything of the answer came: the peer closed instead of
    /// answering.
    Unanswered,
    /// In the answer, which may have been cut short.
    Answer,
}

impl fmt::Display for CutShort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CutShort::Session => {
                "the connection ended without the peer ending its TLS session (no close_notify)"
            }
            CutShort::Unanswered => "the peer closed the connection before answering",
            CutShort::Answer => {
                "the connection ended without the peer ending its TLS session (no \
                 close_notify): the answer may have been cut short"
            }
        })
    }
}

impl std::error::Error for CutShort {}

/// Whether `e` is a TLS session's connection ending under it ([`CutShort`]).
fn is_cut_short(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<CutShort>())
}

impl Transport for Tls {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.sock.bound = Bound::phase(timeout);
        let output = &self.buffers.output()[..amount];
        self.stream.write_all(output)?;
        // Every record out now: the writes' own sending of them keeps its
        // failures for the next call.
        self.stream.flush()?;
        Ok(())
    }

    /// A read of the session. Its connection ending under it — no
    /// close_notify, or a reset — is an error ([`CutShort`]), never the end
    /// of the response: ureq takes an early end, a reset or an abort for the
    /// peer closing, which ends a body its peer delimits by closing (no
    /// length, not chunked) as though it were whole, and any cut of such a
    /// body would have passed for all of it. Over TLS only the peer's
    /// close_notify ends one (RFC 9112 §9.8), and that reads as the end.
    ///
    /// A body of a declared length, or chunked, is not read past its end,
    /// so a peer that closes without a close_notify after one is no
    /// failure; nor one that closes a chunked body after its last chunk but
    /// before the line that ends the body ([`Exchange`] hands that case to
    /// ureq).
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.sock.bound = Bound::phase(timeout);
        if self.buffers.input_append_buf().is_empty() {
            return Err(unending_line(&self.buffers));
        }
        let n = match self.stream.read(self.buffers.input_append_buf()) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                ) =>
            {
                return Err(io::Error::new(io::ErrorKind::InvalidData, CutShort::Session).into());
            }
            Err(e) => return Err(e.into()),
        };
        self.buffers.input_appended(n);
        Ok(n > 0)
    }

    /// Whether the connection can carry another request: open, and with
    /// nothing unasked-for waiting anywhere between its socket and ureq —
    /// plaintext decrypted and not read, records rustls holds processed or
    /// not (a record begun but not ended among them, [`Framing`]), bytes read
    /// off the connection and not yet handed to rustls, and bytes still in
    /// the socket. Any of those is the start of an answer nothing asked for,
    /// which would pass for the answer to the next request.
    fn is_open(&mut self) -> bool {
        if !self.buffers.input().is_empty() || !self.stream.sock.framing.at_boundary() {
            return false;
        }
        match self.stream.conn.process_new_packets() {
            Ok(state) if state.plaintext_bytes_to_read() == 0 && !state.peer_has_closed() => {}
            _ => return false,
        }
        let under = &mut self.stream.sock.transport;
        under.buffers().input().is_empty() && under.is_open()
    }

    fn is_tls(&self) -> bool {
        true
    }
}

/// The most a proxy's answer to a `CONNECT` may hold before its tunnel: its
/// status line and headers, the blank line that ends them included.
const MAX_PROXY_ANSWER: usize = 64 * 1024;

/// A tunnel to `target` through the http proxy `proxy`, on `conn` (a
/// connection to the proxy): a `CONNECT`, and the proxy's `2xx` to it, by
/// `deadline`. What the proxy sent after its answer stays in the buffer, the
/// tunnel's first bytes. A failure is the proxy's ([`ProxyFailed`]) unless
/// `line` was closed, and how the proxy failed is what its answer says: a
/// status another attempt may get past ([`connect_unavailable`]), a refusal,
/// or a `407` — about credentials, or about logins clew cannot make, as its
/// challenges say ([`refused_login`]).
///
/// A `CONNECT` that carried the setting's user name and password, and whose
/// connection closed or reset before anything of an answer came, counts as
/// a refusal of them ([`ProxyFault::LoginUnanswered`]): a proxy may refuse a
/// login by hanging up, and one that checks logins against a directory
/// counts each refused one against the account, which the resends of a few
/// calls lock. No other lack of an answer does — another attempt may get
/// past it ([`ProxyFault::Unanswered`]): not a stall to the deadline, since
/// an http proxy answers a `CONNECT` only once it has connected to the
/// target, and a slow or unreachable target stalls the answer as surely; nor
/// an answer broken off once it began, unless it began as a `407` — whose
/// challenges are then those of its lines that came whole.
fn tunnel(
    mut conn: Box<dyn Transport>,
    proxy: &HttpProxy,
    target: &Target,
    deadline: Instant,
    line: Option<&Line>,
) -> Result<Box<dyn Transport>, ureq::Error> {
    let name = host_port(&proxy.host, proxy.port);
    let authority = target.authority();
    let authorization = proxy.authorization();
    let abandoned_now = || line.is_some_and(Line::is_closed);
    let unsent = |e: ureq::Error| {
        if abandoned_now() {
            return e;
        }
        proxy_failed(
            format!(
                "the proxy {name} failed the CONNECT to {authority}: {}",
                describe(&e)
            ),
            ProxyFault::Unanswered,
        )
    };
    // No whole answer: `received` of one came, and then the wait for the
    // rest failed with `e`, or (`None`) the connection closed.
    let unanswered = |received: &[u8], e: Option<ureq::Error>| {
        if abandoned_now() {
            return e.unwrap_or_else(|| abandoned().into());
        }
        // The connection closed or reset under the wait — a TLS session
        // with the proxy that just ends is its connection closing — rather
        // than the wait running out, or failing otherwise.
        let hung_up = match &e {
            None => true,
            Some(ureq::Error::Io(e)) => {
                is_cut_short(e)
                    || matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::UnexpectedEof
                    )
            }
            Some(_) => false,
        };
        // How, in words; `None` when it closed.
        let how = match &e {
            Some(ureq::Error::Io(e)) if is_cut_short(e) => None,
            Some(e) => Some(describe(e)),
            None => None,
        };
        if !received.is_empty() {
            let how = how.unwrap_or_else(|| "the connection closed".into());
            // An answer begun as a 407 refused the login, or asked for one,
            // whatever cut it short. Which logins it offers, its lines that
            // came whole say, as far as they go: a line cut short may end in
            // a scheme's name cut short.
            let first = received.split(|&b| b == b'\n').next().unwrap_or_default();
            if connect_status(&String::from_utf8_lossy(first)) == Some(407) {
                let whole = received
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map_or(0, |end| end + 1);
                let head = String::from_utf8_lossy(&received[..whole]);
                let offered = offered_logins(head.lines().skip(1));
                let (what, fault) = refused_login(&offered, proxy.credentials.is_some());
                return proxy_failed(
                    format!(
                        "the proxy {name} began to answer the CONNECT to {authority} with a 407 \
                         and broke off ({how}): {what}"
                    ),
                    fault,
                );
            }
            return proxy_failed(
                format!(
                    "the proxy {name} broke off its answer to the CONNECT to {authority} ({how})"
                ),
                ProxyFault::Unanswered,
            );
        }
        match (&authorization, hung_up, how) {
            (Some(_), true, how) => proxy_failed(
                format!(
                    "the proxy {name} gave no answer to the CONNECT to {authority}, which \
                     carried the user name and password of its setting ({}): it may have \
                     refused them",
                    how.unwrap_or_else(|| "the connection closed".into())
                ),
                ProxyFault::LoginUnanswered,
            ),
            (_, _, Some(how)) => proxy_failed(
                format!("the proxy {name} failed the CONNECT to {authority}: {how}"),
                ProxyFault::Unanswered,
            ),
            (_, _, None) => proxy_failed(
                format!("the proxy {name} closed the connection instead of answering the CONNECT"),
                ProxyFault::Unanswered,
            ),
        }
    };
    let too_long = || {
        proxy_failed(
            format!(
                "the proxy {name} answered the CONNECT with over {} KiB of headers",
                MAX_PROXY_ANSWER / 1024
            ),
            ProxyFault::Refused,
        )
    };
    let mut request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: {USER_AGENT}\r\n"
    );
    if let Some(authorization) = &authorization {
        request.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
    }
    request.push_str("\r\n");
    send_all(conn.as_mut(), request.as_bytes(), Bound::connect(deadline)).map_err(unsent)?;
    // How much of the answer has been searched for the end of its head: a
    // proxy that trickles its answer is not searched over again from the
    // start at every read.
    let mut searched = 0;
    let head = loop {
        let input = conn.buffers().input();
        if let Some(end) = head_end(input, searched) {
            if end > MAX_PROXY_ANSWER {
                return Err(too_long());
            }
            let head = String::from_utf8_lossy(&input[..end]).into_owned();
            conn.buffers().input_consume(end);
            break head;
        }
        // No end within the bound: whatever ends it comes past it.
        if input.len() >= MAX_PROXY_ANSWER {
            return Err(too_long());
        }
        searched = input.len();
        // Nothing of it used yet: the next wait must read more.
        conn.buffers().input_consume(0);
        let more = match until(deadline).and_then(|timeout| conn.await_input(timeout)) {
            Ok(more) => more,
            Err(e) => return Err(unanswered(conn.buffers().input(), Some(e))),
        };
        if !more {
            return Err(unanswered(conn.buffers().input(), None));
        }
    };
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let (what, fault) = match connect_status(status_line) {
        Some(200..=299) => return Ok(conn),
        Some(407) => {
            let (what, fault) = refused_login(&offered_logins(lines), proxy.credentials.is_some());
            (
                format!("answered the CONNECT to {authority} with {status_line}: {what}"),
                fault,
            )
        }
        Some(status) if connect_unavailable(status) => (
            if status >= 500 {
                format!("could not reach {authority}: it answered the CONNECT with {status_line}")
            } else {
                format!("could not take the CONNECT to {authority} now: {status_line}")
            },
            ProxyFault::Unavailable,
        ),
        Some(_) => (
            format!("refused the CONNECT to {authority}: {status_line}"),
            ProxyFault::Refused,
        ),
        None => (
            "answered the CONNECT with something other than HTTP".to_string(),
            ProxyFault::Refused,
        ),
    };
    Err(proxy_failed(format!("the proxy {name} {what}"), fault))
}

/// The status a proxy's answer to a `CONNECT` gives in `status_line` (of a
/// whole head, or of as much as came): `None` when that is not HTTP/1.x's.
fn connect_status(status_line: &str) -> Option<u16> {
    let mut parts = status_line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(version), Some(code)) if version.starts_with("HTTP/1.") => code.parse().ok(),
        _ => None,
    }
}

/// Whether `status`, a proxy's answer to a `CONNECT`, says it could not open
/// the tunnel NOW — what another attempt may get past, as it may a direct
/// connect's failure: the target did not answer it, refused it or could not
/// be reached from it (502, 504 and the like), it was overloaded (503) or
/// held clew to a rate (429), or it gave up waiting for the request (408).
/// Every 5xx but those that say it never will: 501 (it has no `CONNECT`),
/// 505 (nor that version of HTTP), 511 (the network itself wants a login —
/// a captive portal's — which no attempt of clew's makes).
fn connect_unavailable(status: u16) -> bool {
    matches!(status, 408 | 429)
        || ((500..=599).contains(&status) && !matches!(status, 501 | 505 | 511))
}

/// The login schemes the `Proxy-Authenticate` fields of a proxy's answer to
/// a `CONNECT` offer ([`login_schemes`]), `lines` the lines of its head past
/// its status line — those up to the blank line that ends its headers.
fn offered_logins<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    login_schemes(
        lines
            .take_while(|line| !line.is_empty())
            .filter_map(|line| {
                let (field, value) = line.split_once(':')?;
                field
                    .eq_ignore_ascii_case("proxy-authenticate")
                    .then_some(value)
            }),
    )
}

/// What a `407` says, and how the proxy failed with it: its challenges
/// offer only logins clew cannot make (`schemes`, [`unusable_login`]) — or
/// it asks for a user name and password, or refused the ones its setting
/// names (`named`).
fn refused_login(schemes: &[String], named: bool) -> (String, ProxyFault) {
    match unusable_login(schemes) {
        Some(schemes) => (
            format!("it takes only {schemes} logins, which clew cannot make"),
            ProxyFault::LoginScheme,
        ),
        None if named => (
            "it refused the user name and password".into(),
            ProxyFault::Credentials,
        ),
        None => (
            "it asks for a user name and password".into(),
            ProxyFault::Credentials,
        ),
    }
}

/// Where the head of a proxy's answer to a `CONNECT` in `input` ends — past
/// the blank line that ends it, a CRLF after the last line's — when it does:
/// the one answer clew reads itself ([`tunnel`]); what it follows of the
/// answers ureq reads, it reads as ureq does ([`answer_head`]). The first
/// `searched` bytes were searched before, so only what came after them is,
/// with the three before it that the blank line may have begun in.
fn head_end(input: &[u8], searched: usize) -> Option<usize> {
    let from = searched.saturating_sub(3).min(input.len());
    input[from..]
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| from + at + 4)
}

/// Send all of `bytes` on `conn`, as many at a time as its output buffer
/// holds, each by what is left of `bound`: one instant for all of them, not
/// the whole of one timeout for each.
fn send_all(conn: &mut dyn Transport, bytes: &[u8], bound: Bound) -> Result<(), ureq::Error> {
    for chunk in bytes.chunks(conn.buffers().output().len().max(1)) {
        conn.buffers().output()[..chunk.len()].copy_from_slice(chunk);
        conn.transmit_output(chunk.len(), bound.next()?)?;
    }
    Ok(())
}

/// How a plain-http request is addressed to the http proxy it goes through:
/// the target in absolute form, and the proxy's credentials.
#[derive(Debug)]
struct Absolute {
    /// The target's authority, as its URL writes it.
    authority: String,
    /// The `Proxy-Authorization`, when the proxy setting names a user.
    authorization: Option<String>,
}

/// The top of a connection, where ureq's requests go in.
///
/// Each request starts a new count on the connection's [`Meter`]: what one
/// response may deliver is bounded, not what a kept connection delivers in
/// all. A request starts where a send follows anything received (and with
/// the first send): clew sends no `Expect: 100-continue`, so between two
/// sends on a connection there is always a whole response.
///
/// Each request takes the connection up on its own line, too, and it is let
/// go when ureq asks whether the connection can carry another request — the
/// request is over then ([`Lease`]).
///
/// A plain-http request through an http proxy is addressed to it in
/// absolute form (`GET http://host:port/path HTTP/1.1`), with the proxy's
/// credentials, as a proxy is asked for a plain-http resource: ureq writes
/// the request line in origin form only, and tunnels such a request through
/// a `CONNECT`, which proxies commonly allow to port 443 alone.
///
/// And each answer is followed as far as a TLS session that ends under it
/// needs ([`Answered`]): the end is then said as what it was to the answer —
/// the peer closing before answering, or an answer that may be cut short —
/// and a chunked body whose last chunk had come is taken, as ureq takes it.
///
/// Its reads and writes keep the request's [`Pace`], when it has one.
#[derive(Debug)]
struct Exchange {
    inner: Box<dyn Transport>,
    meter: Arc<Meter>,
    /// How a plain-http request is addressed through an http proxy.
    absolute: Option<Absolute>,
    /// TLS to the target itself — what ureq requires of an https request (a
    /// TLS session with an `https://` proxy is not one).
    tls: bool,
    /// Whether the next send starts a request.
    fresh: bool,
    /// The connection's standing with the request on it.
    lease: Arc<Lease>,
    /// What has come of the answer to the request on the connection.
    answered: Answered,
    /// The pace every request on the connection keeps, if any.
    pace: Option<Pace>,
    /// Where the request on the connection stands with it, from its first
    /// byte out.
    pacing: Option<Pacing>,
}

/// What has come of the answer to the request on a connection, as far as a
/// TLS session that ends under it needs to know ([`Answered::cut_short`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answered {
    /// Nothing yet.
    Nothing,
    /// Part of its head: the input, `searched` bytes long at the last look,
    /// held no whole one.
    Head { searched: usize },
    /// Its head, whole, and whether its body is chunked ([`answer_head`]).
    Body { chunked: bool },
}

impl Answered {
    /// What has come, now that the input — the answer's bytes ureq has not
    /// taken yet — is `input`, `before` bytes of it there before the read
    /// that brought the rest.
    ///
    /// ureq takes a head off the input only once the whole of it is in, and
    /// an interim answer's (1xx) as soon as it is — reading each as
    /// [`answer_head`] does. So until the final head is whole, the input
    /// starts where the answer does, or where the interim answers before it
    /// end, and holds all it held at the last look. When it does not, ureq
    /// took a head off it that was not whole here: the two readings parted,
    /// and the answer is taken as not chunked — a cut of it an error, never
    /// the end of a body ureq reads to the connection's end.
    ///
    /// The input is read again whole at each look that follows a line's
    /// end, as no head is whole before one: a head that comes a line a read
    /// is read in time quadratic in its length — at worst a byte a read, of
    /// the empty lines before the status line, which httparse passes over
    /// however many there are. No worse than ureq, which reads the input
    /// whole again at every read until a head is in (its `recv_response`),
    /// and bounded as that reading is: past 64 KiB of head (ureq's
    /// `max_response_header_size`), ureq fails the answer.
    fn after(self, before: usize, mut input: &[u8]) -> Answered {
        let mut searched = match self {
            Answered::Body { .. } => return self,
            Answered::Nothing if input.is_empty() => return self,
            Answered::Nothing => 0,
            Answered::Head { searched } if searched != before => {
                return Answered::Body { chunked: false };
            }
            Answered::Head { searched } => searched,
        };
        loop {
            // A head ends at the end of a line: none has come since the last
            // look, so none is whole — and it is not read all over again.
            if !input
                .get(searched..)
                .is_some_and(|new| new.contains(&b'\n'))
            {
                return Answered::Head {
                    searched: input.len(),
                };
            }
            match answer_head(input) {
                AnswerHead::Partial => {
                    return Answered::Head {
                        searched: input.len(),
                    };
                }
                AnswerHead::Interim(len) => {
                    input = &input[len..];
                    searched = 0;
                }
                AnswerHead::Final { chunked } => return Answered::Body { chunked },
            }
        }
    }

    /// The failure of a read of this answer's whose TLS session ended under
    /// it ([`CutShort`]), said as what it was to the answer.
    ///
    /// A chunked body is left to ureq, which takes such an end for the peer
    /// closing: a body whose last chunk (`0\r\n`) had come is whole — every
    /// byte of it came, framed; only the line that ends the body is missing,
    /// as from a server that closes without sending it, whose body ureq takes
    /// on purpose — and one cut before its last chunk fails. Any other body
    /// ends with the peer's close_notify only (see [`Tls::await_input`]).
    fn cut_short(self) -> io::Error {
        match self {
            Answered::Nothing => io::Error::new(io::ErrorKind::InvalidData, CutShort::Unanswered),
            Answered::Body { chunked: true } => {
                io::Error::new(io::ErrorKind::UnexpectedEof, CutShort::Answer)
            }
            _ => io::Error::new(io::ErrorKind::InvalidData, CutShort::Answer),
        }
    }
}

/// The most headers ureq reads in one answer's head (`ureq-proto`'s
/// `MAX_RESPONSE_HEADERS`): a head with more fails the answer.
const MAX_ANSWER_HEADERS: usize = 128;

/// The head at the start of an answer's input, as [`answer_head`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnswerHead {
    /// Not whole yet.
    Partial,
    /// An interim answer's (1xx), this many bytes of the input: a final
    /// answer follows it.
    Interim(usize),
    /// The final answer's, and whether its body is chunked.
    Final { chunked: bool },
}

/// The head at the start of `input`, read as ureq reads an answer's head:
/// by httparse, its parser, as ureq calls it — the default configuration,
/// room for [`MAX_ANSWER_HEADERS`] headers (`ureq-proto`'s
/// `try_parse_response`) — so the two readings cannot part on where the
/// head ends or what its headers are. httparse ends a line, and the head,
/// at a bare LF as much as at a CRLF, and passes over empty lines before
/// the status line: a head split on CRLF alone read on past the `\n\n` that
/// ended ureq's, into the body, and read other headers than ureq's where a
/// line ended with a bare LF.
///
/// The body is chunked only where ureq reads it as chunked too, and more
/// narrowly (it finds `chunked` anywhere in the first `Transfer-Encoding`
/// of an answer that is not HTTP/1.0: `ureq-proto`'s
/// `BodyReader::for_response`): an HTTP/1.1 answer with one
/// `Transfer-Encoding`, in visible ASCII, whose last coding is `chunked`.
/// Read wider, a body ureq reads to the connection's end would pass for a
/// chunked one, whose end it takes without the peer's close_notify
/// ([`Answered::cut_short`]). A head httparse refuses, which fails the
/// answer, is not chunked either.
fn answer_head(input: &[u8]) -> AnswerHead {
    let mut headers = [httparse::EMPTY_HEADER; MAX_ANSWER_HEADERS];
    let mut head = httparse::Response::new(&mut headers);
    let len = match head.parse(input) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) => return AnswerHead::Partial,
        Err(_) => return AnswerHead::Final { chunked: false },
    };
    let code = head.code.unwrap_or_default();
    if (100..200).contains(&code) && code != 101 {
        return AnswerHead::Interim(len);
    }
    let mut encodings = head
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("transfer-encoding"));
    let chunked = match (encodings.next(), encodings.next()) {
        (Some(encoding), None) => {
            let value = encoding.value;
            value
                .iter()
                .all(|&b| b == b'\t' || (b' '..=b'~').contains(&b))
                && value
                    .rsplit(|&b| b == b',')
                    .next()
                    .is_some_and(|last| last.trim_ascii().eq_ignore_ascii_case(b"chunked"))
        }
        _ => false,
    };
    AnswerHead::Final {
        chunked: head.version == Some(1) && chunked,
    }
}

impl Exchange {
    /// `timeout`, for a read or write of the request on the connection, cut
    /// to what is left of the window of its pace it is in ([`Pacing::due`])
    /// — or the request's failure, once nothing is. A timeout of the pace's
    /// is one of the whole request (`Timeout::Global`): what ureq's own
    /// deadline would be, which a paced profile goes without
    /// ([`Profile::pace`]).
    fn paced(&mut self, timeout: NextTimeout) -> Result<NextTimeout, ureq::Error> {
        let (Some(pace), Some(pacing)) = (self.pace, self.pacing.as_mut()) else {
            return Ok(timeout);
        };
        let now = Instant::now();
        let due = pacing.due(pace, self.meter.received(), now);
        let left = due.saturating_duration_since(now);
        if left.is_zero() {
            return Err(self.too_slow(pace));
        }
        if !timeout.after.is_not_happening() && *timeout.after <= left {
            return Ok(timeout);
        }
        Ok(NextTimeout {
            after: left.into(),
            reason: Timeout::Global,
        })
    }

    /// `e`, from a read or write of a paced request: a timeout of the whole
    /// request is its pace running out, and says so.
    fn failed(&self, e: ureq::Error) -> ureq::Error {
        match (e, self.pace) {
            (ureq::Error::Timeout(Timeout::Global), Some(pace)) => self.too_slow(pace),
            (e, _) => e,
        }
    }

    /// The failure of a request whose answer came in too slowly for `pace`.
    fn too_slow(&self, pace: Pace) -> ureq::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out: the answer came in slower than {} KiB/s over {} s, once the first {} s \
                 were up",
                pace.floor.div_ceil(1024),
                pace.window.as_secs_f32(),
                pace.grace.as_secs_f32()
            ),
        )
        .into()
    }

    /// Send ureq's first `amount` output bytes — the start of a request, its
    /// request line first — with the request line in absolute form and the
    /// proxy's credentials after it.
    fn transmit_absolute(
        &mut self,
        amount: usize,
        timeout: NextTimeout,
    ) -> Result<(), ureq::Error> {
        let Some(absolute) = &self.absolute else {
            return self.inner.transmit_output(amount, timeout);
        };
        let output = &self.inner.buffers().output()[..amount];
        let parsed = output
            .windows(2)
            .position(|w| w == b"\r\n")
            .and_then(|end| {
                let line = std::str::from_utf8(&output[..end]).ok()?;
                let (method, rest) = line.split_once(' ')?;
                let (target, version) = rest.split_once(' ')?;
                target
                    .starts_with('/')
                    .then_some((end, method, target, version))
            });
        let Some((end, method, target, version)) = parsed else {
            return Err(io::Error::other(
                "could not address the request to the proxy: its request line was not \
                 where a request starts",
            )
            .into());
        };
        let mut head = format!(
            "{method} http://{}{target} {version}\r\n",
            absolute.authority
        );
        if let Some(authorization) = &absolute.authorization {
            head.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
        }
        let rest = output[end + 2..].to_vec();
        let bound = Bound::phase(timeout);
        send_all(self.inner.as_mut(), head.as_bytes(), bound)?;
        send_all(self.inner.as_mut(), &rest, bound)
    }
}

impl Transport for Exchange {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        if !std::mem::replace(&mut self.fresh, false) {
            let timeout = self.paced(timeout)?;
            return self
                .inner
                .transmit_output(amount, timeout)
                .map_err(|e| self.failed(e));
        }
        self.lease.take_up(Line::current());
        self.meter.restart();
        self.answered = Answered::Nothing;
        self.pacing = Some(Pacing::new(Instant::now()));
        let timeout = self.paced(timeout)?;
        self.transmit_absolute(amount, timeout)
            .map_err(|e| self.failed(e))
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.fresh = true;
        let timeout = self.paced(timeout)?;
        let before = self.inner.buffers().input().len();
        match self.inner.await_input(timeout) {
            Ok(progress) => {
                self.answered = self.answered.after(before, self.inner.buffers().input());
                Ok(progress)
            }
            Err(ureq::Error::Io(e)) if is_cut_short(&e) => Err(self.answered.cut_short().into()),
            Err(e) => Err(self.failed(e)),
        }
    }

    /// Asked between requests only — a connection handed back, or taken up
    /// again — so whichever request was on the connection is over: it lets
    /// the connection go, and one it abandoned is not kept.
    fn is_open(&mut self) -> bool {
        let abandoned = self.lease.abandoned();
        self.lease.let_go();
        !abandoned && self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.tls
    }
}

/// A failure of the proxy on a request's route — the lookup of its name, the
/// connection to it, its TLS, its handshake, its answer to a `CONNECT` —
/// before anything of the request reached the target. A kind of
/// [`Unreached`], so a request is safe to send again after one
/// ([`is_unreached`]) — and worth sending again only when the proxy did not
/// refuse it ([`proxy_refused`]). What the user can do about it goes with
/// its message ([`proxy_failure`], [`Via::advice`]).
#[derive(Debug)]
struct ProxyFailed {
    why: String,
    fault: ProxyFault,
}

/// How the proxy on a request's route failed it ([`ProxyFailed`]): whether
/// another attempt may get past it, and what the user can do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProxyFault {
    /// It could not be reached, or gave no answer: the lookup of its name,
    /// the connection to it, TLS with it, a handshake it broke off or let
    /// stall — as passing as the same failure of a direct connection.
    Unanswered,
    /// It answered that it could not open the tunnel now: the target did
    /// not answer it, refused it or could not be reached from it, or it was
    /// too busy — a `CONNECT` answered 408, 429 or a 5xx that is not final
    /// ([`connect_unavailable`]), a SOCKS5 reply of a general failure, an
    /// unreachable network or host, a refused connection or an expired TTL,
    /// SOCKS4's "rejected or failed". As passing as a direct connect's
    /// failure to reach the target, and as safe to try again: no tunnel was
    /// opened, and no login was refused.
    Unavailable,
    /// It answered, and the answer was no: a `CONNECT` refused (with any
    /// other status but a 2xx or a 407), a SOCKS request refused, an answer
    /// clew cannot use. Asked again, it answers the same.
    Refused,
    /// It asks for a user name and password clew does not have, or refused
    /// the ones its setting names. A refusal too, and one a proxy that
    /// checks them against a directory counts against the account, locking
    /// it after a few.
    Credentials,
    /// It gave no answer to the user name and password its setting names:
    /// it closed or reset the connection before its answer said anything of
    /// them — an http proxy with nothing of its answer to the `CONNECT` in,
    /// a SOCKS proxy with at most the first of the two bytes of its RFC 1929
    /// reply in, the version before the status — or, a SOCKS proxy, let that
    /// reply stall. A proxy may refuse a login so, as surely as by saying
    /// no, and nothing tells the two apart: it counts as a refusal, and they
    /// are not sent again, as after [`ProxyFault::Credentials`] — though
    /// they may be right, and the proxy have failed for a reason of its own.
    LoginUnanswered,
    /// It takes only logins of a kind clew cannot make — NTLM, Negotiate
    /// (Kerberos), Digest; not Basic, the one clew makes — so no user name
    /// and password in any setting lets clew in.
    LoginScheme,
}

impl fmt::Display for ProxyFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.why)
    }
}

impl std::error::Error for ProxyFailed {}

fn proxy_failed(why: String, fault: ProxyFault) -> ureq::Error {
    io::Error::other(ProxyFailed { why, fault }).into()
}

/// How `e` is a failure of the proxy on the request's route
/// ([`ProxyFailed`]), if it is one.
pub(crate) fn proxy_failure(e: &ureq::Error) -> Option<ProxyFault> {
    let ureq::Error::Io(e) = e else {
        return None;
    };
    let inner = e.get_ref()?;
    if let Some(failed) = inner.downcast_ref::<ProxyFailed>() {
        return Some(failed.fault);
    }
    proxy_failure(&inner.downcast_ref::<Unreached>()?.0)
}

/// Whether `e` is the refusal of the proxy on the request's route: the same
/// request would be refused again — a refused login counted against the
/// account once more — so it is not sent again. Of the proxy's answers,
/// only one saying it could not open the tunnel now
/// ([`ProxyFault::Unavailable`]) is not one; of its failures to answer, only
/// the ones [`ProxyFault::LoginUnanswered`] names are.
pub(crate) fn proxy_refused(e: &ureq::Error) -> bool {
    matches!(
        proxy_failure(e),
        Some(
            ProxyFault::Refused
                | ProxyFault::Credentials
                | ProxyFault::LoginUnanswered
                | ProxyFault::LoginScheme
        )
    )
}

/// A failure before any byte of the request was sent — its name lookup, its
/// connection, a proxy's handshake, TLS — as [`Dialer`] and [`FirstHop`]
/// return it (see [`is_unreached`]).
#[derive(Debug)]
struct Unreached(ureq::Error);

impl fmt::Display for Unreached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&describe(&self.0))
    }
}

impl std::error::Error for Unreached {}

/// `e`, marked as a failure before the request was sent.
fn unreached(e: ureq::Error) -> ureq::Error {
    if is_unreached(&e) {
        return e;
    }
    let kind = match &e {
        ureq::Error::Io(e) => e.kind(),
        ureq::Error::Timeout(_) => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    ureq::Error::Io(io::Error::new(kind, Unreached(e)))
}

/// Whether `e` came before any byte of the request was sent: the peer
/// cannot have seen the request, so sending it again is safe.
pub(crate) fn is_unreached(e: &ureq::Error) -> bool {
    matches!(e, ureq::Error::Io(e) if e.get_ref().is_some_and(|inner| inner.is::<Unreached>()))
}

/// What went wrong with a request, for a message: ureq's error in words,
/// without its category's prefix where the cause speaks for itself.
pub(crate) fn describe(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Io(e) => describe_io(e),
        ureq::Error::Timeout(timeout) => match timeout {
            Timeout::Global | Timeout::PerCall => {
                "timed out: the request took longer than it may".into()
            }
            Timeout::Resolve => "timed out looking up the host".into(),
            Timeout::Connect => "timed out connecting".into(),
            Timeout::SendRequest | Timeout::SendBody => "timed out sending the request".into(),
            Timeout::RecvResponse => "timed out waiting for the response".into(),
            Timeout::RecvBody => "timed out reading the response".into(),
            other => format!("timed out ({other})"),
        },
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::LargeResponseHeader(_, limit) => {
            format!("the response headers exceed {} KiB", limit / 1024)
        }
        ureq::Error::Protocol(e) => format!("the response is not valid HTTP: {e}"),
        ureq::Error::ConnectProxyFailed(why) => why.clone(),
        other => other.to_string(),
    }
}

/// `e` in words — and, when the proxy the request went through (`via`) is
/// what failed, what the user can do about it ([`Via::advice`]).
pub(crate) fn failure(e: &ureq::Error, via: Option<&Via>) -> String {
    let text = describe(e);
    match (via, proxy_failure(e)) {
        (Some(via), Some(fault)) => format!("{text}{}", via.advice(fault)),
        _ => text,
    }
}

/// [`describe`], for an I/O error — which may carry a ureq error (a body
/// reader hands its failures out as I/O errors).
pub(crate) fn describe_io(e: &io::Error) -> String {
    let inner = e.get_ref();
    if let Some(unreached) = inner.and_then(|inner| inner.downcast_ref::<Unreached>()) {
        return describe(&unreached.0);
    }
    if let Some(e) = inner.and_then(|inner| inner.downcast_ref::<ureq::Error>()) {
        return describe(e);
    }
    if e.kind() == io::ErrorKind::UnexpectedEof {
        return "the connection closed before the whole response arrived".into();
    }
    e.to_string()
}

/// Send one request — `send` is a ureq `call` or `send` — with a panic
/// inside ureq turned into an error. Every request clew-core makes goes
/// through here: model calls ([`crate::llm`]), embeddings ([`crate::embed`]),
/// downloads (this module).
///
/// ureq 2.12 panicked on responses a peer could shape: it handed a finished
/// connection back by resetting the socket's timeouts under an `expect`, and
/// macOS refuses that on a connection the peer has reset — a server that
/// answered and dropped the connection at the wrong moment took down the
/// thread that asked (a GUI task, a server request). ureq 3 has no such path
/// (the socket is [`Tcp`]'s, which fails instead), and the catch stays so no
/// defect inside the client can end a caller's thread: the panic becomes a
/// transport error, which nothing retries (it is not [`is_unreached`]). The
/// default panic hook still prints the message; the error carries it too.
pub(crate) fn guarded(
    send: impl FnOnce() -> Result<Response<Body>, ureq::Error>,
) -> Result<Response<Body>, ureq::Error> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(send)).unwrap_or_else(|panic| {
        let what = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("no message");
        Err(io::Error::other(format!("{CLIENT_FAILED} ({what})")).into())
    })
}

/// How [`guarded`] reports a panic inside ureq.
pub(crate) const CLIENT_FAILED: &str = "the HTTP client failed while handling the response";

// --------------------------------------------------------------- downloads

/// What a legitimate response delivers beyond its body: a status line and
/// headers (ureq refuses more than 64 KiB of them), and chunked framing,
/// which is a few bytes per chunk. See [`Wire::raw_cap`].
const FRAMING_ALLOWANCE: u64 = 1 << 20;

/// What the connections of one transfer share, between the worker thread
/// that makes them and the caller waiting on it (see [`transfer_on`]): the
/// trust, the bound on what each response may deliver, and the line an
/// abandon closes.
struct Wire {
    /// Trust for every HTTPS hop: [`tls_config`] (a test's own root in tests).
    tls: Arc<rustls::ClientConfig>,
    /// The most one response may deliver, all it sends counted — twice
    /// the body cap, plus [`FRAMING_ALLOWANCE`]. Loose on purpose: the body
    /// cap itself is enforced exactly, on the decoded bytes ([`copy_capped`]);
    /// this only has to keep what ureq takes in out of that loop's sight —
    /// headers, chunked framing, trailers — finite.
    raw_cap: u64,
    /// The line the worker's requests are made on ([`Line::tether`]).
    line: Arc<Line>,
}

impl Wire {
    fn new(max_bytes: u64) -> Wire {
        Wire::trusting(tls_config(), max_bytes)
    }

    fn trusting(tls: Arc<rustls::ClientConfig>, max_bytes: u64) -> Wire {
        Wire {
            tls,
            raw_cap: max_bytes
                .saturating_mul(2)
                .saturating_add(FRAMING_ALLOWANCE),
            line: Arc::default(),
        }
    }
}

/// The agent for ONE request to `url` — HTTPS only (or loopback http), no
/// redirects of its own, the proxy `url`'s host calls for, its connections
/// metered per `wire` — and that proxy, as the agent was built with it.
fn hop_agent(
    url: &str,
    proxies: &ProxySources<'_>,
    wire: &Wire,
) -> Result<(ureq::Agent, Option<Via>), String> {
    let https = url_scheme_host(url).is_some_and(|(scheme, _)| scheme == "https");
    if !https && !is_loopback_http(url) {
        return Err(format!("refusing a non-HTTPS URL: {url}"));
    }
    let via = via(url, proxies);
    let route = route(via.as_ref().map(|via| via.spec.as_str()))?;
    // Redirects are followed by `transfer_with`, one agent per hop — never
    // inside ureq (`agent`), whose one route would cover every hop, and which
    // would copy the caller's headers to wherever it was sent.
    let profile = Profile {
        connect: CONNECT_TIMEOUT,
        read_idle: Some(READ_TIMEOUT),
        write_idle: Some(READ_TIMEOUT),
        total: None,
        pace: None,
        response_cap: wire.raw_cap,
        pooled: false,
        https_only: https,
    };
    Ok((agent(route, wire.tls.clone(), profile), via))
}

/// Where a redirect from `current` to `location` (absolute, or relative to
/// `current`) goes, refused unless it is HTTPS: a hop in the clear is a hop an
/// observer can rewrite.
fn redirect_target(current: &str, location: &str) -> Result<String, String> {
    let next = url::Url::parse(current)
        .and_then(|base| base.join(location))
        .map_err(|e| format!("{current}: an unreadable redirect to {location:?} ({e})"))?;
    if next.scheme() != "https" {
        return Err(format!(
            "{current}: refusing a redirect to a non-HTTPS URL: {next}"
        ));
    }
    Ok(next.to_string())
}

/// Whether `status` is a redirect a download follows (a GET stays a GET
/// under each of them).
fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// The caller's headers that follow a chain once it has left the caller's
/// origin: the ones that carry nothing of the caller's. An allowlist, not a
/// list of credential headers to strip: `x-api-key`, `PRIVATE-TOKEN`,
/// `api-key` — a credential header can be called anything.
const FOLLOWS_EVERY_HOP: &[&str] = &["accept"];

fn follows_every_hop(name: &str) -> bool {
    FOLLOWS_EVERY_HOP
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
}

/// Whether `a` and `b` are the same origin — scheme, host AND port: a
/// credential for `https://host/` is not one for `https://host:8443/` (curl
/// leaked them that way, CVE-2022-27774).
fn same_origin(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

/// One hop of a transfer as it is sent: where to, through which proxy (the
/// one its agent was built with), with which headers.
struct Hop<'a> {
    /// The hop's URL as ureq is handed it ([`request_uri`]).
    uri: &'a Uri,
    proxy: Option<&'a Via>,
    headers: &'a [(&'a str, &'a str)],
}

/// Sends one hop through the agent built for it (see [`send_hop`]).
type SendHop<'a> = dyn FnMut(&ureq::Agent, &Hop<'_>) -> Result<Response<Body>, ureq::Error> + 'a;

/// The real [`SendHop`]: GET the hop's URL with its headers, a panic inside
/// ureq turned into an error ([`guarded`]).
fn send_hop(agent: &ureq::Agent, hop: &Hop<'_>) -> Result<Response<Body>, ureq::Error> {
    let mut request = agent.get(hop.uri.clone());
    for (name, value) in hop.headers {
        request = request.header(*name, *value);
    }
    guarded(move || request.call())
}

/// `status` as a message: its code and, when it has one, its reason.
fn status_text(status: u16) -> String {
    let reason = ureq::http::StatusCode::from_u16(status)
        .ok()
        .and_then(|code| code.canonical_reason());
    match reason {
        Some(reason) => format!("HTTP {status} {reason}"),
        None => format!("HTTP {status}"),
    }
}

/// GET `url` into memory, within `limits`.
pub fn get(url: &str, headers: &[(&str, &str)], limits: Limits) -> Result<Vec<u8>, String> {
    get_cancellable(url, headers, limits, &AtomicBool::new(false))
}

/// [`get`], abandoned the moment `cancel` is set.
pub fn get_cancellable(
    url: &str,
    headers: &[(&str, &str)],
    limits: Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    transfer(url, headers, limits, cancel, &mut body, &mut |_, _| {})?;
    Ok(body)
}

/// GET `url`, streaming the body into `sink` within `limits` and reporting
/// `(bytes_so_far, content_length)` as it goes, abandoned the moment `cancel`
/// is set. Returns the byte count.
pub fn download(
    url: &str,
    headers: &[(&str, &str)],
    limits: Limits,
    cancel: &AtomicBool,
    sink: &mut impl Write,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    transfer(url, headers, limits, cancel, sink, &mut on_progress)
}

fn transfer(
    url: &str,
    headers: &[(&str, &str)],
    limits: Limits,
    cancel: &AtomicBool,
    sink: &mut dyn Write,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    let wire = Arc::new(Wire::new(limits.max_bytes));
    transfer_on(wire, url, headers, limits, cancel, sink, on_progress)
}

/// What a transfer's worker hands the caller waiting on it, in order.
enum Piece {
    Data(Vec<u8>),
    Progress(u64, Option<u64>),
    Done(Result<u64, String>),
}

/// How many pieces a worker may be ahead of its caller: a bounded channel,
/// so a caller that writes slowly slows the download instead of buffering it.
const PIECES_IN_FLIGHT: usize = 4;

/// The worker's end of the sink: every write becomes a [`Piece::Data`].
struct PieceSink<'a>(&'a SyncSender<Piece>);

impl Write for PieceSink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(Piece::Data(buf.to_vec()))
            .map_err(|_| std::io::Error::other("nobody is waiting for the transfer"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Stops a worker nobody waits for any more: set when the caller drops it,
/// however it returns. The flag ends the worker between steps; closing the
/// wire's [`Line`] ends whatever it is blocked in — the connect, a handshake,
/// a read or a write — by shutting its connection down. A name lookup in the
/// system resolver is the one exception: the worker stops waiting for it,
/// and the lookup itself runs on to its own end ([`lookup`]).
struct Abandon {
    stop: Arc<AtomicBool>,
    wire: Arc<Wire>,
}

impl Drop for Abandon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wire.line.close();
    }
}

/// [`transfer`] over `wire` (a test's trust in tests).
///
/// The transfer runs on a worker thread ([`transfer_with`]), and this side
/// only waits for its pieces, testing `cancel` and the deadline every
/// [`POLL`]. Checking them between the worker's own steps was not enough:
/// a hop in flight — a name lookup, a proxy's CONNECT, a handshake, the wait
/// for headers — is one call into ureq that nothing interrupts, and a peer
/// sending a byte a minute restarts the stall timeout forever. A cancelled
/// language-server install sat out its hop, and a trickling peer outlived any
/// deadline. The moment either fires, the caller returns and the worker is
/// abandoned ([`Abandon`]), which ends it too.
#[allow(clippy::too_many_arguments)]
fn transfer_on(
    wire: Arc<Wire>,
    url: &str,
    headers: &[(&str, &str)],
    limits: Limits,
    cancel: &AtomicBool,
    sink: &mut dyn Write,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    let deadline = Instant::now() + limits.deadline;
    let (tx, rx) = std::sync::mpsc::sync_channel(PIECES_IN_FLIGHT);
    let stop = Arc::new(AtomicBool::new(false));
    let job = {
        let (url, stop, wire) = (url.to_string(), stop.clone(), wire.clone());
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        move || {
            // Every connection this worker makes goes on the wire's line.
            let _tether = Line::tether(wire.line.clone());
            let headers: Vec<(&str, &str)> = headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            let mut sink = PieceSink(&tx);
            let mut progress = |done, total| {
                let _ = tx.send(Piece::Progress(done, total));
            };
            // A panic inside ureq outside `guarded` (its body reader) ends
            // this transfer, reported, not the thread silently.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                transfer_with(
                    &url,
                    &headers,
                    limits,
                    &stop,
                    &mut sink,
                    &mut progress,
                    &ProxySources::LIVE,
                    &wire,
                    &mut send_hop,
                )
            }))
            .unwrap_or_else(|panic| {
                let what = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("no message");
                Err(format!("{url}: {CLIENT_FAILED} ({what})"))
            });
            let _ = tx.send(Piece::Done(result));
        }
    };
    std::thread::Builder::new()
        .name("clew-download".into())
        .spawn(job)
        .map_err(|e| format!("{url}: could not start the transfer: {e}"))?;
    let _abandon = Abandon { stop, wire };
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(format!("{url}: download cancelled"));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(format!("{url}: the transfer took too long — giving up"));
        }
        match rx.recv_timeout(POLL.min(deadline - now)) {
            Ok(Piece::Data(bytes)) => sink.write_all(&bytes).map_err(|e| e.to_string())?,
            Ok(Piece::Progress(done, total)) => on_progress(done, total),
            Ok(Piece::Done(result)) => return result,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(format!("{url}: the transfer stopped unexpectedly"));
            }
        }
    }
}

/// One transfer, on the calling thread, with what decides its proxies, the
/// wire its connections go on, and the sending of each hop injected.
/// Redirects are followed HERE, one agent per hop (see the module docs):
/// each hop is HTTPS — a redirect to anything else is refused, and plain http
/// to loopback follows none — goes through the proxy its own host calls for,
/// and carries the caller's headers only while the chain has not left the
/// origin the caller named (a hop reached through another origin was steered
/// by it, back to this one included); after that, only
/// [`FOLLOWS_EVERY_HOP`]. `cancel` and the deadline are tested between steps
/// here — before every hop and between chunks — and [`transfer_on`] enforces
/// them during the steps too; the byte cap binds the body of the last hop.
#[allow(clippy::too_many_arguments)]
fn transfer_with(
    url: &str,
    headers: &[(&str, &str)],
    limits: Limits,
    cancel: &AtomicBool,
    sink: &mut dyn Write,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
    proxies: &ProxySources<'_>,
    wire: &Wire,
    send: &mut SendHop<'_>,
) -> Result<u64, String> {
    let started = Instant::now();
    let deadline = started + limits.deadline;
    let mut current = url.to_string();
    let mut redirects = 0u32;
    let mut left_origin = false;
    let response = loop {
        let (agent, via) = hop_agent(&current, proxies, wire)?;
        let uri = request_uri(&current)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(format!("{current}: download cancelled"));
        }
        if Instant::now() > deadline {
            return Err(format!("{current}: the transfer took too long — giving up"));
        }
        left_origin |= !same_origin(&current, url);
        let hop_headers: Vec<(&str, &str)> = headers
            .iter()
            .filter(|(name, _)| !left_origin || follows_every_hop(name))
            .copied()
            .collect();
        let hop = Hop {
            uri: &uri,
            proxy: via.as_ref(),
            headers: &hop_headers,
        };
        // A failure names the proxy it went through (credentials redacted):
        // behind one, "connection refused" alone does not say by whom.
        let response = send(&agent, &hop).map_err(|e| match hop.proxy {
            Some(via) => format!(
                "{current} (through the proxy {}): {}",
                redact_userinfo(&via.spec),
                failure(&e, Some(via))
            ),
            None => format!("{current}: {}", describe(&e)),
        })?;
        let status = response.status().as_u16();
        if status == 407
            && let Some(via) = hop.proxy
        {
            return Err(format!(
                "{current}: {}",
                via.answered_407(response.headers())
            ));
        }
        if is_redirect(status) && !is_loopback_http(&current) {
            redirects += 1;
            if redirects > MAX_REDIRECTS {
                return Err(format!("{url}: more than {MAX_REDIRECTS} redirects"));
            }
            let location = match response.headers().get("location") {
                Some(value) => match std::str::from_utf8(value.as_bytes()) {
                    Ok(location) => location.to_string(),
                    Err(_) => {
                        return Err(format!(
                            "{current}: HTTP {status} with a Location that is not UTF-8"
                        ));
                    }
                },
                None => return Err(format!("{current}: HTTP {status} without a Location")),
            };
            current = redirect_target(&current, &location)?;
            continue;
        }
        // Every status comes back as a response (`agent`): what is left to
        // refuse is an error, a redirect loopback http may not follow, or
        // another 1xx/3xx.
        if !(200..300).contains(&status) {
            return Err(format!("{current}: {}", status_text(status)));
        }
        break response;
    };
    let url = current.as_str();
    // The length the body is sent with: none for a chunked body, and none
    // for one ureq decompresses (its declared length is the compressed one).
    let total = response.body().content_length();
    // A declared length is not trusted to be honest — the read below binds
    // the bytes actually received — but an honest one over the cap can be
    // refused before any of it is transferred.
    if let Some(total) = total
        && total > limits.max_bytes
    {
        return Err(format!(
            "{url}: the response is {total} bytes, over the {} byte limit",
            limits.max_bytes
        ));
    }
    copy_capped(
        url,
        &mut response.into_body().into_reader(),
        sink,
        Bounds {
            max_bytes: limits.max_bytes,
            deadline,
            cancel,
        },
        total,
        on_progress,
    )
}

/// What stops a copy between chunks.
struct Bounds<'a> {
    max_bytes: u64,
    deadline: Instant,
    cancel: &'a AtomicBool,
}

/// Copy `reader` into `sink`, refusing more than `bounds.max_bytes`, and
/// stopping between chunks at the deadline or once `bounds.cancel` is set.
fn copy_capped(
    url: &str,
    reader: &mut dyn Read,
    sink: &mut dyn Write,
    bounds: Bounds<'_>,
    total: Option<u64>,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    on_progress(0, total);
    loop {
        if bounds.cancel.load(Ordering::Relaxed) {
            return Err(format!("{url}: download cancelled"));
        }
        if Instant::now() > bounds.deadline {
            return Err(format!("{url}: the transfer took too long — giving up"));
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // ureq's word for a body that ended before its declared length
            // (or its closing chunk) — and TLS's, for one whose connection
            // ended without the peer ending TLS.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof || is_cut_short(&e) => {
                let of = total.map(|total| format!(" ({done} of {total})"));
                return Err(format!(
                    "{url}: the connection closed before all bytes were read{}",
                    of.unwrap_or_default()
                ));
            }
            Err(e) => return Err(format!("{url}: {}", describe_io(&e))),
        };
        done += n as u64;
        if done > bounds.max_bytes {
            return Err(format!(
                "{url}: the response exceeds the {} byte limit",
                bounds.max_bytes
            ));
        }
        sink.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        on_progress(done, total);
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The agent a download's hop to `url` gets, with no proxy configured.
    fn agent_for(url: &str) -> Result<ureq::Agent, String> {
        agent_with_env(url, &|_| None)
    }

    /// [`agent_for`] with the environment it reads proxies from.
    fn agent_with_env(
        url: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<ureq::Agent, String> {
        hop_agent(url, &env_only(env), &Wire::new(1024)).map(|(agent, _)| agent)
    }

    /// What a test's proxy decision reads: `env`, and no system settings
    /// (whatever this machine's are).
    fn env_only<'a>(env: &'a dyn Fn(&str) -> Option<String>) -> ProxySources<'a> {
        ProxySources {
            env,
            system: &no_system,
        }
    }

    fn no_system() -> Option<ProxyConfig> {
        None
    }

    /// The setting of the proxy [`via`] decides on for `url`.
    fn proxy_for(url: &str, sources: &ProxySources<'_>) -> Option<String> {
        via(url, sources).map(|via| via.spec)
    }

    #[test]
    fn only_https_or_loopback_http_is_accepted() {
        assert!(agent_for("https://github.com/x").is_ok());
        assert!(agent_for("http://127.0.0.1:8080/x").is_ok());
        assert!(agent_for("http://localhost/x").is_ok());
        assert!(agent_for("http://[::1]:9/x").is_ok());
        assert!(agent_for("http://127.1.2.3/x").is_ok());
        for bad in [
            "http://github.com/x",
            "http://127.0.0.1.evil.com/x",
            "http://evil.com@127.0.0.1.nip.io/x",
            "http://localhost.evil.com/",
            "http://app.localhost/",
            "ftp://127.0.0.1/x",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(agent_for(bad).is_err(), "{bad} must be refused");
        }
        // Userinfo is not the host.
        assert!(is_loopback_http("http://evil.com@127.0.0.1/x"));
        assert!(!is_loopback_http("http://127.0.0.1@evil.com/x"));
    }

    /// The URL is judged the way it is sent. The WHATWG parser ends a special
    /// URL's authority at a backslash, so this one CONNECTS to `evil.com`
    /// (ureq is handed that parser's serialization of it, `request_uri`);
    /// the split that stopped only at `/?#` read its host as `127.0.0.1`
    /// and let it through as loopback cleartext.
    #[test]
    fn a_url_is_judged_as_it_is_sent() {
        let tricky = "http://evil.com\\@127.0.0.1/x";
        let sent_to = request_uri(tricky).unwrap();
        assert_eq!(
            sent_to.host(),
            Some("evil.com"),
            "precondition: where ureq connects"
        );
        assert!(!is_loopback_http(tricky));
        assert!(agent_for(tricky).is_err());
        // Whatever the spelling, the host is the one ureq connects to.
        for (url, host) in [
            ("HTTP://LOCALHOST:8080/x", "localhost"),
            ("http://[::1]:9/x", "::1"),
            ("https://API.example.com./v1", "api.example.com"),
            ("http://user:pw@127.0.0.1:1/", "127.0.0.1"),
        ] {
            let (_, parsed) = url_scheme_host(url).unwrap();
            assert_eq!(parsed, host, "{url}");
        }
        // The proxy decision follows the same reading: this is not loopback,
        // so it is not exempt from the proxy either.
        let env = |name: &str| (name == "http_proxy").then(|| "http://p.corp:1".to_string());
        assert_eq!(
            proxy_for(tricky, &env_only(&env)).as_deref(),
            Some("http://p.corp:1")
        );
    }

    /// The proxy rules, driven through an injected environment so the test
    /// never touches the process's own.
    #[test]
    fn proxies_follow_the_environment_with_no_proxy_and_loopback_bypass() {
        fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
            move |name| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        }
        let corp = env(&[
            ("HTTPS_PROXY", "http://proxy.corp:3128"),
            ("http_proxy", "http://plain.corp:8080"),
            (
                "NO_PROXY",
                "internal.example, .svc.local, 10.1.2.3, gateway:8443",
            ),
        ]);
        assert_eq!(
            proxy_for("https://api.anthropic.com/v1/messages", &env_only(&corp)).as_deref(),
            Some("http://proxy.corp:3128")
        );
        assert_eq!(
            proxy_for("http://models.example.org/v1", &env_only(&corp)).as_deref(),
            Some("http://plain.corp:8080"),
            "plain http uses the http proxy"
        );
        for direct in [
            // Loopback never goes through a proxy: a local model server is
            // unreachable from the proxy's side of the network.
            "http://localhost:11434/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:8080/v1",
            "http://model.localhost/v1",
            // NO_PROXY: exact host, domain suffix, leading-dot suffix, an IP,
            // and an entry with a port (ports are ignored).
            "https://internal.example/v1",
            "https://api.internal.example/v1",
            "https://llm.svc.local/v1",
            "http://10.1.2.3:9000/v1",
            "https://gateway/v1",
        ] {
            assert_eq!(proxy_for(direct, &env_only(&corp)), None, "{direct}");
        }
        // A suffix match is on a label boundary, not a substring.
        assert!(proxy_for("https://notinternal.example/v1", &env_only(&corp)).is_some());

        // `*` bypasses everything; lowercase wins over uppercase; ALL_PROXY
        // is the fallback; empty values count as unset.
        let star = env(&[("HTTPS_PROXY", "http://p:1"), ("no_proxy", "*")]);
        assert_eq!(
            proxy_for("https://api.openai.com/v1", &env_only(&star)),
            None
        );
        let both = env(&[
            ("https_proxy", "http://lower:1"),
            ("HTTPS_PROXY", "http://upper:1"),
        ]);
        assert_eq!(
            proxy_for("https://x.example", &env_only(&both)).as_deref(),
            Some("http://lower:1")
        );
        let all = env(&[("https_proxy", "  "), ("ALL_PROXY", "socks5://s:1080")]);
        assert_eq!(
            proxy_for("https://x.example", &env_only(&all)).as_deref(),
            Some("socks5://s:1080")
        );
        assert_eq!(proxy_for("https://x.example", &env_only(&env(&[]))), None);
    }

    /// A system proxy dictionary as a table of values, for
    /// [`ProxyConfig::from_system`].
    fn dictionary(pairs: Vec<(&'static str, SystemValue)>) -> impl Fn(&str) -> Option<SystemValue> {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        }
    }

    fn text(value: &str) -> SystemValue {
        SystemValue::Text(value.to_string())
    }

    fn list(values: &[&str]) -> SystemValue {
        SystemValue::List(values.iter().map(|v| v.to_string()).collect())
    }

    /// The system's proxy settings apply when — and only when — the
    /// environment names no proxy: a proxy variable keeps priority over
    /// them whole, and `NO_PROXY` still exempts hosts from them, beside the
    /// system's own exceptions. clew used to read the environment alone, so
    /// behind a proxy set in System Settings (how a Mac is usually
    /// configured, and the only way for an app launched from the Dock)
    /// every request connected directly.
    #[test]
    fn the_system_proxy_applies_only_when_the_environment_names_none() {
        let system = || {
            Some(ProxyConfig {
                http: Some("http://sys.corp:8080".into()),
                https: Some("http://sys.corp:8443".into()),
                bypass: vec!["*.local".into(), "169.254/16".into(), "10.0.0.0/8".into()],
                bypass_simple: true,
            })
        };
        let none = |_: &str| None;
        fn with<'a>(
            env: &'a dyn Fn(&str) -> Option<String>,
            system: &'a dyn Fn() -> Option<ProxyConfig>,
        ) -> ProxySources<'a> {
            ProxySources { env, system }
        }
        // No proxy variable: the system's proxy, per scheme.
        assert_eq!(
            proxy_for("https://api.openai.com/v1", &with(&none, &system)).as_deref(),
            Some("http://sys.corp:8443")
        );
        assert_eq!(
            proxy_for("http://models.example.org/v1", &with(&none, &system)).as_deref(),
            Some("http://sys.corp:8080")
        );
        // Its exceptions: a suffix, macOS's abbreviated range, a range, a
        // simple host name — and loopback, always.
        for direct in [
            "https://printer.local/",
            "http://169.254.10.1/",
            "https://10.20.30.40:8443/v1",
            "https://intranet/",
            "http://localhost:11434/v1",
        ] {
            assert_eq!(proxy_for(direct, &with(&none, &system)), None, "{direct}");
        }
        // NO_PROXY exempts hosts from the system's proxy as well.
        let no_proxy = |name: &str| (name == "NO_PROXY").then(|| "internal.example".to_string());
        assert_eq!(
            proxy_for("https://api.internal.example/", &with(&no_proxy, &system)),
            None
        );
        assert!(proxy_for("https://api.openai.com/", &with(&no_proxy, &system)).is_some());
        let star = |name: &str| (name == "no_proxy").then(|| "*".to_string());
        assert_eq!(
            proxy_for("https://api.openai.com/", &with(&star, &system)),
            None
        );
        // A proxy variable decides alone: its proxy, its exceptions, and no
        // fallback to the system's for the scheme it leaves out.
        let env_proxy =
            |name: &str| (name == "https_proxy").then(|| "http://env.corp:3128".to_string());
        assert_eq!(
            proxy_for("https://api.openai.com/v1", &with(&env_proxy, &system)).as_deref(),
            Some("http://env.corp:3128")
        );
        assert_eq!(
            proxy_for("http://models.example.org/v1", &with(&env_proxy, &system)),
            None,
            "the environment named no http proxy"
        );
        assert!(
            proxy_for("https://intranet/", &with(&env_proxy, &system)).is_some(),
            "the system's exceptions belong to the system's proxy"
        );
        // Not even read then: the environment keeps priority whatever the
        // system holds.
        let unread = || -> Option<ProxyConfig> { panic!("the system settings were read") };
        let sources = ProxySources {
            env: &env_proxy,
            system: &unread,
        };
        assert!(proxy_for("https://api.openai.com/v1", &sources).is_some());
        // An empty variable counts as unset.
        let empty = |name: &str| (name == "https_proxy").then(String::new);
        assert_eq!(
            proxy_for("https://api.openai.com/v1", &with(&empty, &system)).as_deref(),
            Some("http://sys.corp:8443")
        );
        // No system proxy either: direct.
        let nothing = ProxySources {
            env: &none,
            system: &no_system,
        };
        assert_eq!(proxy_for("https://api.openai.com/v1", &nothing), None);
    }

    /// The system's proxy dictionary is read as macOS writes it: the proxy
    /// for each scheme when its switch is on, SOCKS for a scheme whose own
    /// is off, the port when there is one, and the exception list.
    #[test]
    fn a_system_proxy_dictionary_is_read_as_macos_writes_it() {
        use SystemValue::Number;
        // What System Settings leaves with no proxy on: nothing to apply.
        let default = dictionary(vec![
            ("ExceptionsList", list(&["*.local", "169.254/16"])),
            ("FTPPassive", Number(1)),
            ("HTTPEnable", Number(0)),
            ("HTTPSEnable", Number(0)),
        ]);
        assert_eq!(ProxyConfig::from_system(&default), None);
        // A PAC file alone is not evaluated.
        let pac = dictionary(vec![
            ("ProxyAutoConfigEnable", Number(1)),
            ("ProxyAutoConfigURLString", text("http://wpad/proxy.pac")),
        ]);
        assert_eq!(ProxyConfig::from_system(&pac), None);

        let corp = dictionary(vec![
            ("ExceptionsList", list(&["*.local", " 169.254/16 ", ""])),
            ("ExcludeSimpleHostnames", Number(1)),
            ("HTTPEnable", Number(1)),
            ("HTTPProxy", text("web.corp")),
            ("HTTPPort", Number(8080)),
            ("HTTPSEnable", Number(1)),
            ("HTTPSProxy", text(" secure.corp ")),
            ("HTTPSPort", Number(8443)),
            ("SOCKSEnable", Number(1)),
            ("SOCKSProxy", text("socks.corp")),
            ("SOCKSPort", Number(1080)),
        ]);
        assert_eq!(
            ProxyConfig::from_system(&corp),
            Some(ProxyConfig {
                http: Some("http://web.corp:8080".into()),
                https: Some("http://secure.corp:8443".into()),
                bypass: vec!["*.local".into(), "169.254/16".into()],
                bypass_simple: true,
            })
        );
        // SOCKS alone covers both schemes; a switched-off proxy is not used
        // even when its host is filled in; no port leaves the default.
        let socks = dictionary(vec![
            ("HTTPEnable", Number(0)),
            ("HTTPProxy", text("stale.corp")),
            ("HTTPSEnable", Number(1)),
            ("HTTPSProxy", text("")),
            ("SOCKSEnable", Number(1)),
            ("SOCKSProxy", text("socks.corp")),
        ]);
        // As `socks5h`: the system's own clients hand a SOCKS proxy the
        // host's name, for it to resolve.
        assert_eq!(
            ProxyConfig::from_system(&socks),
            Some(ProxyConfig {
                http: Some("socks5h://socks.corp".into()),
                https: Some("socks5h://socks.corp".into()),
                bypass: Vec::new(),
                bypass_simple: false,
            })
        );
        // An IPv6 proxy address is bracketed, as a URL spells it — a proxy
        // clew connects to like any other.
        let v6 = dictionary(vec![
            ("HTTPSEnable", Number(1)),
            ("HTTPSProxy", text("fd00::1")),
            ("HTTPSPort", Number(3128)),
        ]);
        let config = ProxyConfig::from_system(&v6).unwrap();
        assert_eq!(config.https.as_deref(), Some("http://[fd00::1]:3128"));
        assert!(matches!(
            proxy("http://[fd00::1]:3128"),
            Ok(ProxySetting::Http(HttpProxy { ref host, port: 3128, .. })) if host == "fd00::1"
        ));
    }

    /// fixR5 #11: a switch that is a CFBoolean switches the proxy on, as a
    /// number does; and a port no connection can use is the setting's
    /// failure, never a reason to use the default port instead (it used to
    /// fall back to 80 or 1080 in silence).
    #[test]
    fn a_system_proxy_reads_boolean_switches_and_refuses_unusable_ports() {
        use SystemValue::{Bool, Number};
        let on = dictionary(vec![
            ("HTTPSEnable", Bool(true)),
            ("HTTPSProxy", text("secure.corp")),
            ("HTTPSPort", Number(8443)),
            ("HTTPEnable", Bool(false)),
            ("HTTPProxy", text("web.corp")),
            ("ExcludeSimpleHostnames", Bool(true)),
        ]);
        assert_eq!(
            ProxyConfig::from_system(&on),
            Some(ProxyConfig {
                http: None,
                https: Some("http://secure.corp:8443".into()),
                bypass: Vec::new(),
                bypass_simple: true,
            })
        );
        for port in [0, 65536, 70000, -1] {
            let bad = dictionary(vec![
                ("HTTPSEnable", Number(1)),
                ("HTTPSProxy", text("secure.corp")),
                ("HTTPSPort", Number(port)),
            ]);
            let config = ProxyConfig::from_system(&bad).expect("the proxy is on");
            let spec = config.https.expect("its setting");
            let err = proxy(&spec).expect_err(&format!("port {port} was used"));
            assert!(err.contains("unusable proxy setting"), "{port}: {err}");
            // And so is every request that would take it: never direct.
            let sources = ProxySources {
                env: &|_| None,
                system: &|| ProxyConfig::from_system(&bad),
            };
            let via = proxy_for("https://api.example.com/v1", &sources);
            assert_eq!(via.as_deref(), Some(spec.as_str()));
            assert!(route(via.as_deref()).is_err(), "{port}");
        }
    }

    /// A bypass entry — of `NO_PROXY`, or of the system's exceptions — may
    /// be an address range, which covers the IP hosts inside it: `10.0.0.0/8`
    /// and IPv6 prefixes, and macOS's abbreviated `169.254/16`. Such entries
    /// used to be compared as names, so they never matched anything.
    #[test]
    fn bypass_entries_take_address_ranges_and_wildcards() {
        let covers = |entry: &str, host: &str| bypass_entry_matches(host, entry);
        for (entry, inside, outside) in [
            ("10.0.0.0/8", "10.255.1.2", "11.0.0.1"),
            ("192.168.1.0/24", "192.168.1.200", "192.168.2.1"),
            ("192.168.1.77/24", "192.168.1.1", "192.168.0.1"),
            ("169.254/16", "169.254.3.4", "169.253.3.4"),
            ("10/8", "10.1.1.1", "100.1.1.1"),
            ("172.16.0.0/12", "172.31.255.255", "172.32.0.0"),
            ("1.2.3.4/32", "1.2.3.4", "1.2.3.5"),
            ("0.0.0.0/0", "203.0.113.9", "::1"),
            ("fd00::/8", "fd12:3456::1", "fe80::1"),
            ("[fd00::]/8", "fdff::2", "fc00::1"),
            ("2001:db8::/32", "2001:db8:ffff::1", "2001:db9::1"),
            ("::/0", "2001:db8::1", "10.0.0.1"),
        ] {
            assert!(covers(entry, inside), "{entry} covers {inside}");
            assert!(!covers(entry, outside), "{entry} does not cover {outside}");
        }
        // An IPv4 address spelled as IPv6 is still inside an IPv4 range.
        assert!(covers("10.0.0.0/8", "::ffff:10.1.2.3"));
        // A name is not resolved to find out.
        assert!(!covers("10.0.0.0/8", "ten.example"));
        // Not a range: a prefix too long, or not a number.
        assert!(!covers("10.0.0.0/33", "10.0.0.1"));
        assert!(!covers("fd00::/129", "fd00::1"));
        assert!(!covers("10.0.0.0/x", "10.0.0.1"));
        assert!(!covers("1.2.3.4.5/8", "1.2.3.4"));
        // An address covers itself however it is spelled.
        assert!(covers("fd00::1", "fd00:0:0::1"));
        assert!(covers("[::1]:8080", "::1"));
        assert!(covers("10.1.2.3", "::ffff:10.1.2.3"));
        // Wildcards anywhere, as the macOS list allows; still on the name.
        assert!(covers("192.168.*", "192.168.7.9"));
        assert!(!covers("192.168.*", "192.169.7.9"));
        assert!(covers("*.corp.*", "git.corp.net"));
        assert!(!covers("*.corp.*", "corp.net"));
        assert!(covers("*.local", "printer.local"));
        assert!(covers("*.LOCAL", "printer.local"));
        // fixR5 #11: a range with a port is still a range (the port is
        // ignored, as on any entry), and an IPv4 range written as
        // IPv4-mapped IPv6 covers the IPv4 addresses it maps.
        assert!(covers("10.0.0.0/8:443", "10.9.8.7"));
        assert!(!covers("10.0.0.0/8:443", "11.0.0.1"));
        assert!(covers("[fd00::]/8:8443", "fd00::7"));
        assert!(!covers("10.0.0.0/8:https", "10.9.8.7"));
        assert!(covers("::ffff:10.0.0.0/104", "10.1.2.3"));
        assert!(covers("::ffff:10.0.0.0/104", "::ffff:10.1.2.3"));
        assert!(!covers("::ffff:10.0.0.0/104", "11.1.2.3"));
        assert!(covers("[::ffff:192.168.0.0]/112", "192.168.4.5"));
        assert!(glob_matches("a*b*c", "aXXbYYc"));
        assert!(!glob_matches("a*b*c", "aXXbYY"));
        assert!(glob_matches("**", ""));
    }

    /// The macOS reader takes the dictionary CoreFoundation hands out — its
    /// numbers, strings and arrays — to the same settings. Built here, so
    /// the machine's own settings are neither read nor touched.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_system_dictionary_is_read_through_core_foundation() {
        use system_configuration::core_foundation::array::CFArray;
        use system_configuration::core_foundation::base::{CFType, TCFType};
        use system_configuration::core_foundation::boolean::CFBoolean;
        use system_configuration::core_foundation::dictionary::CFDictionary;
        use system_configuration::core_foundation::number::CFNumber;
        use system_configuration::core_foundation::string::CFString;
        let key = |k: &str| CFString::new(k);
        let number = |n: i32| CFNumber::from(n).as_CFType();
        let string = |s: &str| CFString::new(s).as_CFType();
        let exceptions =
            CFArray::from_CFTypes(&[CFString::new("*.local"), CFString::new("169.254/16")]);
        let dict: CFDictionary<CFString, CFType> = CFDictionary::from_CFType_pairs(&[
            // A switch as a CFBoolean, as a number.
            (key("HTTPSEnable"), CFBoolean::true_value().as_CFType()),
            (key("HTTPSProxy"), string("secure.corp")),
            (key("HTTPSPort"), number(8443)),
            (key("HTTPEnable"), number(0)),
            (key("ExcludeSimpleHostnames"), number(1)),
            (key("ExceptionsList"), exceptions.as_CFType()),
        ]);
        assert_eq!(
            macos_proxies::from_dictionary(&dict),
            Some(ProxyConfig {
                http: None,
                https: Some("http://secure.corp:8443".into()),
                bypass: vec!["*.local".into(), "169.254/16".into()],
                bypass_simple: true,
            })
        );
    }

    #[test]
    fn proxy_credentials_are_redacted_from_messages() {
        assert_eq!(
            redact_userinfo("http://user:pw@proxy:3128"),
            "http://…@proxy:3128"
        );
        assert_eq!(
            redact_userinfo("http://user:p@ss@proxy:3128"),
            "http://…@proxy:3128",
            "a password holding an `@` is redacted whole"
        );
        assert_eq!(redact_userinfo("proxy:3128"), "proxy:3128");
        // An unusable setting is refused without the password.
        let err = proxy("ftp://user:secret@proxy:21").unwrap_err();
        assert!(
            err.contains("unusable proxy setting ftp://…@proxy:21"),
            "{err}"
        );
        assert!(!err.contains("secret"), "{err}");
        // fixR5 #11: nor does a proxy printed for debugging show it.
        for spec in [
            "socks5://user:secret@proxy:1080",
            "https://user:secret@proxy:3128",
        ] {
            let printed = format!(
                "{:?} {:?}",
                proxy(spec).unwrap(),
                route(Some(spec)).unwrap()
            );
            assert!(!printed.contains("secret"), "{printed}");
            assert!(printed.contains("user"), "{printed}");
        }
        let via = Via {
            spec: "http://user:secret@proxy:3128".into(),
            system: None,
            for_https: true,
        };
        assert!(!format!("{via:?}").contains("secret"));
    }

    /// Whether a root of `store` has a subject naming `name`.
    fn has_root(store: &rustls::RootCertStore, name: &[u8]) -> bool {
        store
            .roots
            .iter()
            .any(|root| root.subject.as_ref().windows(name.len()).any(|w| w == name))
    }

    /// The trust store holds every bundled root, whatever the host store
    /// holds — ISRG Root X1 (Let's Encrypt's) among them — so it never holds
    /// less than the bundled set alone, on a host with no CA bundle as on any.
    #[test]
    fn the_trust_store_keeps_the_bundled_roots() {
        let roots = trust_roots();
        for bundled in webpki_roots::TLS_SERVER_ROOTS {
            assert!(roots.roots.contains(bundled), "a bundled root is missing");
        }
        assert!(has_root(&roots, b"ISRG Root X1"));
        let config = tls_config();
        assert!(Arc::ptr_eq(&config, &tls_config()), "built once");
    }

    /// fixR5 #8: the other half, offline — the OS store is in the trust as
    /// well, beside the bundled roots. The test certificate is the OS store
    /// here: `SSL_CERT_FILE`, which `rustls-native-certs` loads in place of
    /// the platform's store, in a child process (the store is read once per
    /// process, and the variable would reach every test beside this one).
    #[test]
    fn the_trust_store_takes_in_the_systems_roots() {
        let dir = crate::testutil::TempDir::new("net-os-trust");
        let roots = dir.join("roots.pem");
        std::fs::write(&roots, crate::testutil::TEST_CERT).unwrap();
        crate::testutil::run_in_child(
            "net::tests::child_the_trust_store_takes_in_the_systems_roots",
            &[
                ("SSL_CERT_FILE", Some(roots.as_os_str())),
                ("SSL_CERT_DIR", None),
            ],
        );
    }

    #[test]
    #[ignore = "runs in a child process, from the_trust_store_takes_in_the_systems_roots"]
    fn child_the_trust_store_takes_in_the_systems_roots() {
        if !crate::testutil::in_child() {
            return;
        }
        let roots = trust_roots();
        assert!(
            has_root(&roots, b"clew-test-provider"),
            "the OS store's root is not trusted"
        );
        assert!(
            has_root(&roots, b"ISRG Root X1"),
            "the bundled roots are gone once the OS store has one"
        );
    }

    /// Downloads take the proxy the environment names for their host, as the
    /// model calls always did: behind a proxy, a download that went direct
    /// failed while chat worked. The request reaches the proxy as a CONNECT
    /// for the download's host.
    #[test]
    fn a_download_goes_through_the_configured_proxy() {
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let (seen_tx, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = proxy.accept() {
                // Bounded: a client that stops short fails the test, not hangs it.
                let _ = seen_tx.send(crate::testutil::read_http(&mut conn).head);
                let _ = conn.write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
            }
        });
        // A host that resolves nowhere: only the proxy can take this request.
        let url = "https://releases.invalid/clew/asset.tar.gz";
        let env = |name: &str| (name == "https_proxy").then(|| proxy_url.clone());
        let agent = agent_with_env(url, &env).unwrap();
        let _ = guarded(|| agent.get(url).call());
        let line = seen
            .recv_timeout(Duration::from_secs(5))
            .expect("the download must go to the proxy");
        assert!(line.starts_with("CONNECT releases.invalid:443 "), "{line}");

        // An unusable proxy is an error, not a direct connection.
        let broken = |name: &str| (name == "https_proxy").then(|| "ftp://p:1".to_string());
        let err = agent_with_env(url, &broken).unwrap_err();
        assert!(err.contains("unusable proxy setting"), "{err}");
    }

    /// An HTTPS hop's agent refuses plain http itself, before connecting —
    /// the guard under `redirect_target`'s own refusal (redirects are
    /// followed by `transfer_with`, never inside the agent). Nothing may
    /// reach this listener.
    #[test]
    fn the_https_agent_refuses_plain_http_on_every_hop() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/artifact", listener.local_addr().unwrap());
        let agent = agent_with_env("https://example.invalid/", &|_| None).unwrap();
        let err = agent.get(url.as_str()).call().unwrap_err();
        assert!(matches!(err, ureq::Error::RequireHttpsOnly(_)), "{err}");
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "the agent connected to a plain-HTTP URL"
        );
    }

    /// A one-shot HTTP server on loopback: answers the first connection with
    /// `response`, once the whole request is in (answering a partial read
    /// resets the connection under the client, see `testutil`).
    fn serve_once(response: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                crate::testutil::read_http_request(&mut stream);
                let _ = stream.write_all(&response);
            }
        });
        format!("http://{addr}/asset")
    }

    /// A panic inside the send is an error of the request, and a result
    /// passes through untouched.
    #[test]
    fn a_panicking_send_is_an_error_not_a_panic() {
        let err = guarded(|| panic!("returning stream to pool: Os {{ code: 22 }}")).unwrap_err();
        assert!(matches!(err, ureq::Error::Io(_)), "{err}");
        assert!(!is_unreached(&err), "never a retryable failure");
        let text = describe(&err);
        assert!(text.contains(CLIENT_FAILED), "{text}");
        assert!(text.contains("returning stream to pool"), "{text}");
        let err = guarded(|| std::panic::panic_any(7u8)).unwrap_err();
        assert!(describe(&err).contains("no message"), "{err}");

        let ok = guarded(|| fake_response("HTTP/1.1 204 No Content\r\n\r\n")).unwrap();
        assert_eq!(ok.status().as_u16(), 204);
    }

    /// `raw` — a status line, headers and a body — as the response ureq
    /// hands over: of the length its `Content-Length` declares, or of none.
    fn fake_response(raw: &str) -> Result<Response<Body>, ureq::Error> {
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
        let mut lines = head.lines();
        let status: u16 = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("a status line");
        let mut response = Response::builder().status(status);
        let mut declared = false;
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                declared |= name.trim().eq_ignore_ascii_case("content-length");
                response = response.header(name.trim(), value.trim());
            }
        }
        let bytes = body.as_bytes().to_vec();
        let body = if declared {
            Body::builder().data(bytes)
        } else {
            Body::builder().reader(std::io::Cursor::new(bytes))
        };
        Ok(response.body(body)?)
    }

    #[test]
    fn the_byte_cap_holds_with_and_without_a_content_length() {
        let limits = Limits {
            max_bytes: 16,
            deadline: Duration::from_secs(20),
        };
        let body = "x".repeat(64);
        let declared =
            serve_once(format!("HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\n{body}").into_bytes());
        let err = get(&declared, &[], limits).unwrap_err();
        assert!(err.contains("over the 16 byte limit"), "{err}");

        let undeclared =
            serve_once(format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{body}").into_bytes());
        let err = get(&undeclared, &[], limits).unwrap_err();
        assert!(err.contains("exceeds the 16 byte limit"), "{err}");

        let fine = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec());
        assert_eq!(get(&fine, &[], limits).unwrap(), b"ok");
    }

    /// The copy binds the bytes received (the declared length is advisory)
    /// and stops between chunks once cancelled or past its deadline.
    #[test]
    fn a_copy_is_capped_cancellable_and_bounded_in_time() {
        let never = AtomicBool::new(false);
        let later = Instant::now() + Duration::from_secs(60);
        let cap = 256 * 1024;
        let copy = |len: u64, cancel: &AtomicBool, deadline: Instant| {
            let mut sink = Vec::new();
            copy_capped(
                "u",
                &mut std::io::repeat(7).take(len),
                &mut sink,
                Bounds {
                    max_bytes: cap,
                    deadline,
                    cancel,
                },
                None,
                &mut |_, _| {},
            )
            .map(|n| (n, sink.len() as u64))
        };
        assert_eq!(copy(cap, &never, later), Ok((cap, cap)));
        let err = copy(cap + 1, &never, later).unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
        let err = copy(10, &AtomicBool::new(true), later).unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        let past = Instant::now() - Duration::from_secs(1);
        let err = copy(10, &never, past).unwrap_err();
        assert!(err.contains("too long"), "{err}");
    }

    /// One hop as the fake network below saw it.
    #[derive(Debug, PartialEq)]
    struct SentHop {
        url: String,
        proxy: Option<String>,
        headers: Vec<String>,
    }

    /// Run [`transfer_with`] against a fake network: `answer` gives each
    /// hop's raw HTTP response, by URL. Returns every hop sent and the body
    /// (or why the transfer failed).
    fn fake_transfer(
        url: &str,
        headers: &[(&str, &str)],
        limits: Limits,
        cancel: &AtomicBool,
        env: &dyn Fn(&str) -> Option<String>,
        answer: &mut dyn FnMut(&str) -> String,
    ) -> (Vec<SentHop>, Result<Vec<u8>, String>) {
        let mut hops = Vec::new();
        let mut body = Vec::new();
        let mut send = |_: &ureq::Agent, hop: &Hop<'_>| {
            let url = hop.uri.to_string();
            hops.push(SentHop {
                url: url.clone(),
                proxy: hop.proxy.map(|via| via.spec.clone()),
                headers: hop.headers.iter().map(|(n, _)| n.to_string()).collect(),
            });
            fake_response(&answer(&url))
        };
        let result = transfer_with(
            url,
            headers,
            limits,
            cancel,
            &mut body,
            &mut |_, _| {},
            &env_only(env),
            &Wire::new(limits.max_bytes),
            &mut send,
        );
        (hops, result.map(|_| body))
    }

    fn redirect(status: u16, to: &str) -> String {
        format!("HTTP/1.1 {status} Moved\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n")
    }

    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    const ROOMY: Limits = Limits {
        max_bytes: 1024,
        deadline: Duration::from_secs(60),
    };

    /// The way a transfer took: each hop's URL and proxy.
    fn way(hops: &[SentHop]) -> Vec<(&str, Option<&str>)> {
        hops.iter()
            .map(|h| (h.url.as_str(), h.proxy.as_deref()))
            .collect()
    }

    /// fixR1 #3 residual: redirects are followed one hop at a time, and each
    /// hop goes through the proxy ITS host calls for. A release asset that
    /// redirects from a proxied host to one `NO_PROXY` exempts reaches that
    /// one directly (ureq applied the first hop's proxy to the whole chain),
    /// and the other way round.
    #[test]
    fn each_hop_takes_the_proxy_its_own_host_calls_for() {
        let env = |name: &str| match name {
            "https_proxy" => Some("http://proxy.corp:3128".to_string()),
            "no_proxy" => Some("internal.example".to_string()),
            _ => None,
        };
        let never = AtomicBool::new(false);
        let (hops, body) = fake_transfer(
            "https://releases.example/clew/v1/asset",
            &[],
            ROOMY,
            &never,
            &env,
            &mut |url| match url {
                "https://releases.example/clew/v1/asset" => {
                    redirect(302, "https://objects.internal.example/blob?sig=1")
                }
                _ => ok("payload"),
            },
        );
        assert_eq!(body.unwrap(), b"payload");
        assert_eq!(
            way(&hops),
            [
                (
                    "https://releases.example/clew/v1/asset",
                    Some("http://proxy.corp:3128")
                ),
                ("https://objects.internal.example/blob?sig=1", None),
            ]
        );

        // From an exempt host to a proxied one, through a relative Location
        // first (resolved against the hop it came from).
        let (hops, body) = fake_transfer(
            "https://mirror.internal.example/a/asset",
            &[],
            ROOMY,
            &never,
            &env,
            &mut |url| match url {
                "https://mirror.internal.example/a/asset" => redirect(301, "../b/asset"),
                "https://mirror.internal.example/b/asset" => {
                    redirect(307, "https://cdn.example/asset")
                }
                _ => ok("x"),
            },
        );
        assert_eq!(body.unwrap(), b"x");
        assert_eq!(
            way(&hops),
            [
                ("https://mirror.internal.example/a/asset", None),
                ("https://mirror.internal.example/b/asset", None),
                ("https://cdn.example/asset", Some("http://proxy.corp:3128")),
            ]
        );
    }

    /// The caller's headers go to the origin it named — scheme, host and
    /// port — and nowhere else: not to another port of the same host, not to
    /// the host a redirect hands the download to, and not back to the origin
    /// through a chain another origin steered. Only `Accept` follows every
    /// hop: a credential header can be called anything (`X-Api-Key`,
    /// `PRIVATE-TOKEN`), so the ones that go on are listed, not the ones that
    /// stay.
    #[test]
    fn credentials_stay_with_the_origin_the_caller_named() {
        let never = AtomicBool::new(false);
        let (hops, body) = fake_transfer(
            "https://api.example/r1",
            &[
                ("Authorization", "token t"),
                ("Cookie", "c=1"),
                // Credentials under names no list of credential headers has.
                ("X-Api-Key", "k"),
                ("PRIVATE-TOKEN", "p"),
                ("Accept", "application/octet-stream"),
            ],
            ROOMY,
            &never,
            &|_| None,
            &mut |url| match url {
                "https://api.example/r1" => redirect(302, "/r2"),
                "https://api.example/r2" => redirect(302, "https://api.example:8443/r3"),
                "https://api.example:8443/r3" => redirect(302, "https://cdn.example/r4"),
                "https://cdn.example/r4" => redirect(302, "https://api.example/r5"),
                _ => ok("done"),
            },
        );
        assert_eq!(body.unwrap(), b"done");
        let seen: Vec<(&str, Vec<&str>)> = hops
            .iter()
            .map(|h| {
                (
                    h.url.as_str(),
                    h.headers.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        let all = vec![
            "Authorization",
            "Cookie",
            "X-Api-Key",
            "PRIVATE-TOKEN",
            "Accept",
        ];
        assert_eq!(
            seen,
            [
                ("https://api.example/r1", all.clone()),
                ("https://api.example/r2", all),
                ("https://api.example:8443/r3", vec!["Accept"]),
                ("https://cdn.example/r4", vec!["Accept"]),
                ("https://api.example/r5", vec!["Accept"]),
            ]
        );
    }

    /// Every hop stays HTTPS — a redirect to plain http is refused before
    /// anything is sent to it, loopback included — the chain is bounded, a
    /// redirect must say where to, and the byte cap binds the final body.
    #[test]
    fn a_redirect_chain_is_https_only_bounded_and_capped() {
        let never = AtomicBool::new(false);
        let none = |_: &str| None;
        let start = "https://releases.example/asset";
        for to in ["http://cdn.example/asset", "http://127.0.0.1:9/asset"] {
            let (hops, result) =
                fake_transfer(start, &[], ROOMY, &never, &none, &mut |_| redirect(302, to));
            let err = result.unwrap_err();
            assert!(err.contains("non-HTTPS"), "{to}: {err}");
            assert_eq!(hops.len(), 1, "{to}: a hop went out in the clear");
        }

        // Unbounded, this chain would run to the deadline, a minute away: the
        // answer fails the test the moment the chain outgrows the bound.
        let mut answered = 0;
        let (hops, result) = fake_transfer(start, &[], ROOMY, &never, &none, &mut |url| {
            answered += 1;
            assert!(
                answered <= 4 * MAX_REDIRECTS,
                "the redirect chain is unbounded"
            );
            redirect(302, &format!("{url}/next"))
        });
        let err = result.unwrap_err();
        assert!(err.contains("more than 8 redirects"), "{err}");
        assert_eq!(hops.len(), MAX_REDIRECTS as usize + 1);

        let (_, result) = fake_transfer(start, &[], ROOMY, &never, &none, &mut |_| {
            "HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n".to_string()
        });
        let err = result.unwrap_err();
        assert!(err.contains("without a Location"), "{err}");

        let small = Limits {
            max_bytes: 16,
            ..ROOMY
        };
        let big = "x".repeat(64);
        let (_, result) = fake_transfer(start, &[], small, &never, &none, &mut |url| {
            if url == start {
                redirect(302, "https://cdn.example/asset")
            } else {
                ok(&big)
            }
        });
        let err = result.unwrap_err();
        assert!(err.contains("over the 16 byte limit"), "{err}");
        let (_, result) = fake_transfer(start, &[], small, &never, &none, &mut |url| {
            if url == start {
                redirect(302, "https://cdn.example/asset")
            } else {
                format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{big}")
            }
        });
        let err = result.unwrap_err();
        assert!(err.contains("exceeds the 16 byte limit"), "{err}");
    }

    /// The cancel flag and the deadline are checked before every hop, not
    /// only while a body streams: a download stopped (or out of time) while
    /// its first hop was answered sends no second one.
    #[test]
    fn cancel_and_deadline_cover_every_hop() {
        let _ = tls_config(); // built once, before anything is timed
        let none = |_: &str| None;
        let start = "https://releases.example/asset";
        let cancel = AtomicBool::new(false);
        let (hops, result) = fake_transfer(start, &[], ROOMY, &cancel, &none, &mut |_| {
            cancel.store(true, Ordering::Relaxed);
            redirect(302, "https://cdn.example/asset")
        });
        assert!(result.unwrap_err().contains("cancelled"));
        assert_eq!(hops.len(), 1);

        let never = AtomicBool::new(false);
        let brief = Limits {
            deadline: Duration::from_millis(300),
            ..ROOMY
        };
        let (hops, result) = fake_transfer(start, &[], brief, &never, &none, &mut |_| {
            std::thread::sleep(Duration::from_millis(600));
            redirect(302, "https://cdn.example/asset")
        });
        assert!(result.unwrap_err().contains("too long"));
        assert_eq!(hops.len(), 1);
    }

    /// Plain http to loopback may not redirect anywhere — least of all to
    /// another host in the clear.
    #[test]
    fn loopback_http_follows_no_redirects() {
        let url = serve_once(
            b"HTTP/1.1 302 Found\r\nLocation: http://example.com/x\r\nContent-Length: 0\r\n\r\n"
                .to_vec(),
        );
        let limits = Limits {
            max_bytes: 1024,
            deadline: Duration::from_secs(20),
        };
        let err = get(&url, &[], limits).unwrap_err();
        assert!(err.contains("302"), "{err}");
    }

    // ------------------------------------------------------------- SOCKS

    /// What a test's agent is made with: generous bounds, a meter far past
    /// any answer here, no pooling.
    fn test_profile() -> Profile {
        Profile {
            connect: Duration::from_secs(10),
            read_idle: Some(Duration::from_secs(10)),
            write_idle: Some(Duration::from_secs(10)),
            total: None,
            pace: None,
            response_cap: 1 << 20,
            pooled: false,
            https_only: false,
        }
    }

    /// GET `url` through `agent`: the body, or the failure in words.
    fn call(agent: &ureq::Agent, url: &str) -> Result<String, String> {
        let response = guarded(|| agent.get(url).call()).map_err(|e| describe(&e))?;
        let mut body = String::new();
        response
            .into_body()
            .into_reader()
            .read_to_string(&mut body)
            .map_err(|e| describe_io(&e))?;
        Ok(body)
    }

    /// SOCKS proxies work: `ALL_PROXY=socks5://…` alone (a common local
    /// proxy setup) used to be accepted and then fail every connection —
    /// ureq was built without SOCKS support. The request reaches the proxy as
    /// a CONNECT: by NAME under `socks5h://`, for the proxy to resolve, and by
    /// address under `socks5://`, whose names are resolved here.
    #[test]
    fn a_download_goes_through_a_socks_proxy() {
        for (scheme, host, sent) in [
            ("socks5h", "releases.invalid", (3, "releases.invalid")),
            ("socks5", "192.0.2.10", (1, "192.0.2.10")),
        ] {
            // "Host unreachable": the request ends there.
            let (addr, seen) =
                crate::testutil::socks5_proxy(None, crate::testutil::SocksAnswer::Refuse(4));
            let spec = format!("{scheme}://{addr}");
            let url = format!("https://{host}/clew/asset.tar.gz");
            let env = move |name: &str| (name == "ALL_PROXY").then(|| spec.clone());
            let agent = agent_with_env(&url, &env).unwrap();
            let err = call(&agent, &url).unwrap_err();
            assert!(err.contains("host unreachable"), "{scheme}: {err}");
            let seen = seen
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{scheme}: the download never reached the proxy"));
            assert_eq!(seen.command, 1, "{scheme}: a CONNECT");
            assert_eq!(
                seen.target,
                Some((sent.0, sent.1.to_string(), 443)),
                "{scheme}: the download's host, as the scheme sends it"
            );
        }
    }

    /// A proxy setting is read as a URL, as curl reads it: percent-encoded
    /// credentials are decoded before they are sent (ureq 2 sent them
    /// encoded), an `https://` proxy and an IPv6 address are proxies like
    /// any other, and a path, a port of 0 or a scheme clew does not speak are
    /// refused with the reason.
    #[test]
    fn proxy_settings_are_read_as_urls() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let _ = seen_tx.send(crate::testutil::read_http(&mut conn).head);
                let _ = conn.write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
            }
        });
        let spec = format!("http://us%20er:p%40ss%3Aw@{addr}/");
        let url = "https://releases.invalid/asset";
        let env = move |name: &str| (name == "https_proxy").then(|| spec.clone());
        let agent = agent_with_env(url, &env).unwrap();
        let _ = guarded(|| agent.get(url).call());
        let head = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        // base64("us er:p@ss:w")
        assert!(
            head.contains("Proxy-Authorization: Basic dXMgZXI6cEBzczp3"),
            "{head}"
        );

        for (spec, why) in [
            ("http://proxy.corp:3128/proxy.pac", "no path"),
            ("http://proxy.corp:0", "port 0"),
            ("ftp://proxy.corp:21", "ftp://"),
            ("http://us%3Aer:pw@proxy.corp:3128", "`:`"),
        ] {
            let err = proxy(spec).unwrap_err();
            assert!(err.contains(why), "{spec}: {err}");
        }
        for fine in [
            "proxy.corp:3128",
            "https://proxy.corp:3128",
            "http://[::1]:3128",
            "https://[fd00::1]",
            "socks5h://127.0.0.1:1080",
            "socks5://[::1]:1080",
            "socks4://p:1080",
        ] {
            assert!(proxy(fine).is_ok(), "{fine}");
        }
        assert!(matches!(
            proxy("https://proxy.corp"),
            Ok(ProxySetting::Http(HttpProxy {
                tls: true,
                port: 443,
                ..
            }))
        ));
    }

    /// A SOCKS proxy that accepts the connection and never answers is let go
    /// with the request: the handshake runs on the connection the request's
    /// line holds, so the abandon (`Line::close`, which a deadline or a
    /// cancel runs) shuts it down and the request's thread returns. ureq's own
    /// SOCKS runs the handshake on a helper thread that nothing can reach.
    #[test]
    fn a_silent_socks_proxy_is_let_go_with_the_request() {
        let (spec, greeted, closed) = crate::testutil::silent_socks_proxy();
        let url = "https://releases.invalid/asset";
        let env = move |name: &str| (name == "ALL_PROXY").then(|| spec.clone());
        let wire = test_wire(1024);
        let (agent, _) = hop_agent(url, &env_only(&env), &wire).unwrap();
        let (done_tx, done) = std::sync::mpsc::channel();
        let line = wire.line.clone();
        std::thread::spawn(move || {
            let _tether = Line::tether(line);
            let failed = guarded(|| agent.get(url).call()).is_err();
            let _ = done_tx.send(failed);
        });
        greeted
            .recv_timeout(Duration::from_secs(10))
            .expect("the request reached the proxy");
        let abandoned = Instant::now();
        wire.line.close();
        let (gone, at) = closed.recv_timeout(Duration::from_secs(60)).unwrap();
        assert!(
            gone && at.duration_since(abandoned) < Duration::from_secs(2),
            "the connection to the proxy outlived the abandon"
        );
        assert_eq!(
            done.recv_timeout(Duration::from_secs(2)),
            Ok(true),
            "the request's thread was not let go"
        );
    }

    /// fixR5 #4: a SOCKS proxy that stalls mid-handshake is given up at the
    /// connect timeout — the handshake's every read and write is bounded by
    /// it — and nothing is left behind: the request returns, and its
    /// connection to the proxy is closed. ureq's own SOCKS waits for a helper
    /// thread (inside `thread::scope`) that no timeout can cut short.
    #[test]
    fn a_socks_proxy_that_stalls_is_given_up_at_the_connect_timeout() {
        let (spec, greeted, closed) = crate::testutil::silent_socks_proxy();
        let profile = Profile {
            connect: Duration::from_millis(500),
            ..test_profile()
        };
        let agent = agent(route(Some(&spec)).unwrap(), test_trust(), profile);
        let began = Instant::now();
        let err = call(&agent, "https://releases.invalid/asset").unwrap_err();
        let took = began.elapsed();
        greeted
            .recv_timeout(Duration::from_secs(10))
            .expect("the proxy was greeted");
        assert!(err.contains("did not finish its handshake"), "{err}");
        assert!(took < Duration::from_secs(5), "given up after {took:?}");
        let (gone, _) = closed.recv_timeout(Duration::from_secs(60)).unwrap();
        assert!(gone, "the connection to the proxy was left open");
    }

    /// The SOCKS handshakes, byte for byte: SOCKS5 with a user name and
    /// password (RFC 1928, RFC 1929), to a name and to an IPv6 address, and
    /// to an address resolved here; SOCKS4 to an address, with the user as
    /// its id; SOCKS4a to a name; and the ways a proxy says no — a refusal
    /// to let clew in marked as one.
    #[test]
    fn socks_handshakes_are_spoken_as_the_rfcs_write_them() {
        /// A connection whose far side is scripted: it answers `input`, and
        /// keeps what it was sent.
        struct Scripted {
            input: std::io::Cursor<Vec<u8>>,
            sent: Vec<u8>,
        }
        impl Read for Scripted {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.input.read(buf)
            }
        }
        impl Write for Scripted {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.sent.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // A resolver that knows one name.
        let resolve = |host: &str, port: u16| -> io::Result<Vec<SocketAddr>> {
            match host {
                "api.example.com" => Ok(vec![
                    SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 7], port)),
                    SocketAddr::from(([203, 0, 113, 7], port)),
                ]),
                _ => Err(io::Error::new(io::ErrorKind::NotFound, "no such host")),
            }
        };
        let run = |spec: &str, host: &str, port: u16, answers: &[u8]| {
            let ProxySetting::Socks(socks) = proxy(spec).unwrap() else {
                panic!("{spec} is not a SOCKS proxy");
            };
            let mut conn = Scripted {
                input: std::io::Cursor::new(answers.to_vec()),
                sent: Vec::new(),
            };
            let result = socks
                .address(host, port, &resolve)
                .and_then(|address| socks.handshake(&mut conn, &address, port));
            (result, conn.sent)
        };

        // SOCKS5h, logging in: both ways offered, the password chosen, the
        // credentials sent decoded, then a CONNECT by name; the answer's bound
        // address (IPv4) is read past.
        let (result, sent) = run(
            "socks5h://us%20er:p%40ss@proxy:1080",
            "api.example.com",
            443,
            &[5, 2, 1, 0, 5, 0, 0, 1, 10, 0, 0, 1, 0x1f, 0x90],
        );
        assert!(result.is_ok(), "{result:?}");
        let want = [
            &[5u8, 2, 0, 2, 1, 5][..],
            b"us er",
            &[4],
            b"p@ss",
            &[5, 1, 0, 3, 15],
            b"api.example.com",
            &[1, 187],
        ]
        .concat();
        assert_eq!(sent, want);

        // SOCKS5 resolves the name here, and sends the first address.
        let (result, sent) = run(
            "socks5://proxy",
            "api.example.com",
            443,
            &[5, 0, 5, 0, 0, 1, 0, 0, 0, 0, 0, 0],
        );
        assert!(result.is_ok(), "{result:?}");
        let v6 = "2001:db8::7"
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .octets();
        let want = [&[5u8, 1, 0, 5, 1, 0, 4][..], &v6, &443u16.to_be_bytes()].concat();
        assert_eq!(sent, want);
        // A name that does not resolve fails before the proxy is spoken to —
        // saying which scheme hands the name to the proxy instead (fixR7 #5):
        // behind a proxy, it may be a name only the proxy's side knows.
        let (result, sent) = run("socks5://proxy", "nowhere.example", 443, &[]);
        let err = result.unwrap_err();
        assert!(sent.is_empty(), "{sent:?}");
        assert!(err.to_string().contains("no such host"), "{err}");
        assert!(err.to_string().contains("set it as socks5h://"), "{err}");
        let (result, _) = run("socks4://proxy", "nowhere.example", 80, &[]);
        let err = result.unwrap_err();
        assert!(err.to_string().contains("set it as socks4a://"), "{err}");
        // An abandon is only that.
        let ProxySetting::Socks(socks) = proxy("socks5://proxy").unwrap() else {
            panic!("a SOCKS proxy");
        };
        let err = socks
            .address("x.example", 443, &|_, _| Err(abandoned()))
            .unwrap_err();
        assert_eq!(err.to_string(), ABANDONED);

        // SOCKS5 to an IPv6 address, as one; a bound name is read past.
        let (result, sent) = run(
            "socks5h://proxy",
            "2001:db8::1",
            8443,
            &[5, 0, 5, 0, 0, 3, 4, b'h', b'o', b's', b't', 0, 80],
        );
        assert!(result.is_ok(), "{result:?}");
        let v6 = "2001:db8::1"
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .octets();
        let want = [&[5u8, 1, 0, 5, 1, 0, 4][..], &v6, &8443u16.to_be_bytes()].concat();
        assert_eq!(sent, want);

        // The ways to say no, each saying why — and how the proxy failed:
        // what another attempt may get past, what it would only answer
        // again, and a refusal to let clew in (a `PermissionDenied`).
        let fault = |e: &io::Error| {
            e.get_ref()
                .and_then(|inner| inner.downcast_ref::<ProxyFailed>())
                .map(|failed| failed.fault)
        };
        use ProxyFault::{Credentials, LoginUnanswered, Refused, Unavailable};
        // fixR9 #1: a CONNECT's reply (RFC 1928 §6). A target the proxy
        // could not reach, or not now, is worth another attempt, as a direct
        // connect's failure to reach it is.
        let replies = [
            (
                1,
                "could not reach x.example:443: a general failure",
                Unavailable,
            ),
            (
                2,
                "refused the CONNECT to x.example:443: not allowed by its rules",
                Refused,
            ),
            (
                3,
                "could not reach x.example:443: network unreachable",
                Unavailable,
            ),
            (
                4,
                "could not reach x.example:443: host unreachable",
                Unavailable,
            ),
            (
                5,
                "could not reach x.example:443: connection refused",
                Unavailable,
            ),
            (6, "could not reach x.example:443: TTL expired", Unavailable),
            (
                7,
                "refused the CONNECT to x.example:443: command not supported",
                Refused,
            ),
            (
                8,
                "refused the CONNECT to x.example:443: address type not supported",
                Refused,
            ),
            (
                9,
                "refused the CONNECT to x.example:443: an unknown error",
                Refused,
            ),
        ]
        .map(|(code, why, fault)| {
            let answers = vec![5u8, 0, 5, code, 0, 1, 0, 0, 0, 0, 0, 0];
            ("socks5h://proxy", answers, why, fault)
        });
        let others = [
            (
                "socks5h://proxy",
                vec![5u8, 0xFF],
                "asks for a user name and password",
                Credentials,
            ),
            // It takes no user name and password at all: not a matter of
            // credentials.
            (
                "socks5h://u:p@proxy",
                vec![5, 0xFF],
                "accepts none of the ways",
                Refused,
            ),
            ("socks5h://proxy", vec![5, 2], "not offered", Refused),
            (
                "socks5h://u:bad@proxy",
                vec![5, 2, 1, 1],
                "refused the user name",
                Credentials,
            ),
            // fixR9 #5: no answer to the user name and password — the
            // connection closed before the answer, or halfway through it —
            // may be a refusal as surely, and counts as one; fixR10 #1:
            // one said to be a refusal only perhaps.
            (
                "socks5h://u:p@proxy",
                vec![5, 2],
                "gave no answer to the user name and password (the connection closed): it \
                 may have refused them",
                LoginUnanswered,
            ),
            (
                "socks5h://u:p@proxy",
                vec![5, 2, 1],
                "gave no answer to the user name and password",
                LoginUnanswered,
            ),
            // fixR5 #11: RFC 1929's reply starts with its own version (1).
            (
                "socks5h://u:p@proxy",
                vec![5, 2, 5, 0],
                "other than RFC 1929",
                Refused,
            ),
            (
                "socks5h://proxy",
                vec![4, 0],
                "does not speak SOCKS5",
                Refused,
            ),
            // SOCKS4's "rejected or failed", which most proxies answer any
            // failure with; and its answers about the user id.
            (
                "socks4a://proxy",
                vec![0, 91, 0, 0, 0, 0, 0, 0],
                "could not reach x.example:443 (SOCKS4 answer 91",
                Unavailable,
            ),
            (
                "socks4a://proxy",
                vec![0, 93, 0, 0, 0, 0, 0, 0],
                "refused the CONNECT to x.example:443 (SOCKS4 answer 93)",
                Refused,
            ),
            // fixR5 #11: and SOCKS4's with its version (0).
            (
                "socks4a://proxy",
                vec![4, 90, 0, 0, 0, 0, 0, 0],
                "other than SOCKS4",
                Refused,
            ),
        ];
        for (spec, answers, why, expected) in replies.into_iter().chain(others) {
            let (result, _) = run(spec, "x.example", 443, &answers);
            let err = result.unwrap_err();
            assert!(err.to_string().contains(why), "{spec} {answers:?}: {err}");
            assert_eq!(fault(&err), Some(expected), "{spec} {answers:?}: {err}");
            assert_eq!(
                err.kind() == io::ErrorKind::PermissionDenied,
                matches!(expected, Credentials | LoginUnanswered),
                "{spec} {answers:?}: {err}"
            );
        }
        // A proxy that stops mid-answer — with no login to make, or once it
        // took the login — is an error, not a tunnel: the connection's, not
        // an answer about credentials.
        let (result, _) = run("socks5h://proxy", "x.example", 443, &[5, 0, 5, 0, 0]);
        let err = result.unwrap_err();
        assert_eq!(fault(&err), None, "{err}");
        let (result, _) = run("socks5h://u:p@proxy", "x.example", 443, &[5, 2, 1, 0, 5, 0]);
        let err = result.unwrap_err();
        assert_eq!(fault(&err), None, "{err}");

        // SOCKS4 to an address, with the user as its id — a name resolved
        // here, to IPv4; SOCKS4a by name.
        let granted = [0u8, 90, 0, 0, 0, 0, 0, 0];
        let (result, sent) = run("socks4://me@proxy", "10.1.2.3", 80, &granted);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            sent,
            [&[4u8, 1, 0, 80, 10, 1, 2, 3][..], b"me", &[0]].concat()
        );
        let (result, sent) = run("socks4://proxy", "api.example.com", 80, &granted);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(sent, [&[4u8, 1, 0, 80, 203, 0, 113, 7][..], &[0]].concat());
        let (result, sent) = run("socks4a://proxy", "api.example.com", 443, &granted);
        assert!(result.is_ok(), "{result:?}");
        let want = [
            &[4u8, 1, 1, 187, 0, 0, 0, 1, 0][..],
            b"api.example.com",
            &[0],
        ]
        .concat();
        assert_eq!(sent, want);
    }

    /// A request really goes through a SOCKS tunnel: the proxy logs the
    /// client in, connects where the CONNECT says and relays, and the TLS
    /// session with the target runs inside the tunnel, verified against the
    /// target's certificate as any other.
    #[test]
    fn a_download_goes_through_a_socks_tunnel() {
        let target = tls_serve(vec![answer(
            b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\ndone",
        )]);
        let (addr, seen) = crate::testutil::socks5_proxy(
            Some(("me", "secret")),
            crate::testutil::SocksAnswer::Relay,
        );
        let spec = format!("socks5://me:secret@{addr}");
        // A hop's agent as `hop_agent` builds it, but routed through the
        // proxy by hand: the loopback bypass would keep it off the proxy.
        let agent = agent(route(Some(&spec)).unwrap(), test_trust(), test_profile());
        assert_eq!(
            call(&agent, &format!("{target}/asset")).as_deref(),
            Ok("done")
        );
        let seen = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(seen.login, Some(("me".into(), "secret".into())));
        assert_eq!(
            seen.target.map(|(kind, _, _)| kind),
            Some(1),
            "an IPv4 target"
        );
    }

    /// fixR5 #4, #9: plain http goes through a SOCKS proxy as https does —
    /// by name under `socks5h://`, and after logging in. R4 refused it (the
    /// SOCKS tunnel was opened where ureq 2 handed out an https connection
    /// only), so a LAN model server behind a system SOCKS proxy — which covers
    /// plain http too — could not be reached at all.
    #[test]
    fn plain_http_goes_through_a_socks_proxy() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (head_tx, head) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let _ = head_tx.send(crate::testutil::read_http(&mut conn).head);
                let _ = conn.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                );
            }
        });
        let (addr, seen) =
            crate::testutil::socks5_proxy(Some(("me", "pw")), crate::testutil::SocksAnswer::Relay);
        let spec = format!("socks5h://me:pw@{addr}");
        let agent = agent(route(Some(&spec)).unwrap(), test_trust(), test_profile());
        let url = format!("http://localhost:{port}/v1/models");
        assert_eq!(call(&agent, &url).as_deref(), Ok("ok"));
        let seen = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(seen.target, Some((3, "localhost".into(), port)));
        // In the tunnel, the request as it goes to the server itself.
        let head = head.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.starts_with("GET /v1/models HTTP/1.1\r\n"), "{head}");
    }

    /// fixR5 #9, #10: a failure of the proxy is the proxy's — nothing of the
    /// request reached the target, so sending it again is safe
    /// (`is_unreached`) — and says what can be done: how to reach the host
    /// without the proxy, where the proxy's setting came from (the system's
    /// bypass list first: a Dock-launched app has no environment to set),
    /// and what to do when the proxy wants a password clew does not have.
    #[test]
    fn a_proxy_failure_says_what_to_do() {
        let never = AtomicBool::new(false);
        let url = "https://models.lan.invalid/v1/asset";
        let fetch = |sources: &ProxySources<'_>| {
            let mut body = Vec::new();
            transfer_with(
                url,
                &[],
                ROOMY,
                &never,
                &mut body,
                &mut |_, _| {},
                sources,
                &Wire::new(1024),
                &mut send_hop,
            )
            .unwrap_err()
        };
        let system = |spec: String| {
            move || {
                Some(ProxyConfig {
                    http: Some(spec.clone()),
                    https: Some(spec.clone()),
                    bypass: Vec::new(),
                    bypass_simple: false,
                })
            }
        };
        const BYPASS: &str = "Bypass proxy settings for these hosts";

        // A system SOCKS proxy that cannot reach the host.
        let (addr, seen) =
            crate::testutil::socks5_proxy(None, crate::testutil::SocksAnswer::Refuse(4));
        let socks = system(format!("socks5h://{addr}"));
        let err = fetch(&ProxySources {
            env: &|_| None,
            system: &socks,
        });
        assert!(err.contains("host unreachable"), "{err}");
        assert!(
            err.contains(BYPASS) && err.contains("or in no_proxy"),
            "{err}"
        );
        let seen = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(seen.target, Some((3, "models.lan.invalid".into(), 443)));

        // The same proxy named by the environment: no_proxy is the way.
        let (addr, _) =
            crate::testutil::socks5_proxy(None, crate::testutil::SocksAnswer::Refuse(4));
        let spec = format!("socks5h://{addr}");
        let env = move |name: &str| (name == "https_proxy").then(|| spec.clone());
        let err = fetch(&env_only(&env));
        assert!(
            err.contains("list the host in no_proxy") && !err.contains(BYPASS),
            "{err}"
        );

        // A system proxy that asks for the password it keeps in the keychain.
        let (proxy_url, heads) = crate::testutil::http_proxy(
            "127.0.0.1:0",
            false,
            1,
            crate::testutil::ProxyAnswer::Status(407),
        )
        .unwrap();
        let keychain = system(proxy_url);
        let err = fetch(&ProxySources {
            env: &|_| None,
            system: &keychain,
        });
        assert!(err.contains("407"), "{err}");
        assert!(err.contains("asks for a user name and password"), "{err}");
        assert!(err.contains("keychain") && err.contains(BYPASS), "{err}");
        let head = heads.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            head.starts_with("CONNECT models.lan.invalid:443 "),
            "{head}"
        );

        // The failures themselves: the proxy's, and before anything was sent
        // — and (fixR9 #1) refusals only when the proxy's rules refused: a
        // target it could not reach may be reached by another attempt.
        for (code, fault) in [(5, ProxyFault::Unavailable), (2, ProxyFault::Refused)] {
            let (addr, _) =
                crate::testutil::socks5_proxy(None, crate::testutil::SocksAnswer::Refuse(code));
            let agent = agent(
                route(Some(&format!("socks5h://{addr}"))).unwrap(),
                test_trust(),
                test_profile(),
            );
            let err = guarded(|| agent.get(url).call()).unwrap_err();
            assert!(is_unreached(&err), "{err}");
            assert_eq!(proxy_failure(&err), Some(fault), "{err}");
            assert_eq!(proxy_refused(&err), fault == ProxyFault::Refused, "{err}");
        }

        // The advice itself, by where the setting came from.
        let via = |system: bool| Via {
            spec: "http://proxy.corp:3128".into(),
            system: system.then(ProxyConfig::default),
            for_https: true,
        };
        for fault in [
            ProxyFault::Unanswered,
            ProxyFault::Unavailable,
            ProxyFault::Refused,
        ] {
            assert!(via(true).advice(fault).contains(BYPASS));
            assert!(!via(false).advice(fault).contains(BYPASS));
        }
        assert!(
            via(true)
                .advice(ProxyFault::Credentials)
                .contains("keychain")
        );
        assert!(
            !via(false)
                .advice(ProxyFault::Credentials)
                .contains("keychain")
        );
        // fixR10 #1: no answer to the user name and password the setting
        // names may be a refusal of them — or the proxy's own failure, and
        // they right: both ways out, not a password to correct alone.
        let unanswered = Via {
            spec: "http://me:pw@proxy.corp:3128".into(),
            ..via(false)
        };
        assert_eq!(
            unanswered.advice(ProxyFault::LoginUnanswered),
            format!(
                " — if they are wrong, correct them in its setting \
                 (http://user:password@proxy.corp:3128, percent-encoding any character in them \
                 that is not a letter or a digit); if they are right, try again later, or \
                 {ENV_BYPASS}"
            )
        );
        let named = Via {
            spec: "http://user:secret@proxy.corp:3128".into(),
            ..via(true)
        };
        assert!(
            !named
                .answered_407(&ureq::http::HeaderMap::new())
                .contains("secret")
        );
    }

    /// fixR5 #3: `https://` proxies — spoken to over TLS, verified with the
    /// same trust as any peer — and proxies at an IPv6 address both work, for
    /// an https target (a `CONNECT`, and TLS with the target inside the
    /// tunnel) and a plain-http one (the request itself, in absolute form, as
    /// a proxy is asked for a plain-http resource), credentials and all. ureq
    /// 2 refused both kinds of proxy, and ureq 3 alone would `CONNECT` for a
    /// plain-http target too, which proxies commonly allow to port 443 only.
    #[test]
    fn https_and_ipv6_proxies_carry_both_kinds_of_target() {
        use crate::testutil::{ProxyAnswer, http_proxy};
        for (bind, tls) in [("127.0.0.1:0", true), ("[::1]:0", false)] {
            let Some((proxy_url, heads)) = http_proxy(bind, tls, 2, ProxyAnswer::Serve) else {
                eprintln!("skipped {bind}: no such address to listen on here");
                continue;
            };
            let spec = proxy_url.replacen("://", "://me:secret@", 1);
            let agent = agent(route(Some(&spec)).unwrap(), test_trust(), test_profile());
            let next = || heads.recv_timeout(Duration::from_secs(10)).unwrap();
            // base64("me:secret")
            let credentials = |head: &str| {
                head.lines()
                    .any(|line| line == "Proxy-Authorization: Basic bWU6c2VjcmV0")
            };

            let body = call(&agent, "https://localhost:4443/asset");
            assert_eq!(body.as_deref(), Ok("ok"), "{spec}");
            let head = next();
            assert!(
                head.starts_with("CONNECT localhost:4443 HTTP/1.1\r\n"),
                "{spec}: {head}"
            );
            assert!(credentials(&head), "{spec}: {head}");
            let head = next();
            assert!(
                head.starts_with("GET /asset HTTP/1.1\r\n"),
                "{spec}: {head}"
            );

            let body = call(&agent, "http://localhost:8080/page?q=1");
            assert_eq!(body.as_deref(), Ok("ok"), "{spec}");
            let head = next();
            assert!(
                head.starts_with("GET http://localhost:8080/page?q=1 HTTP/1.1\r\n"),
                "{spec}: {head}"
            );
            assert!(credentials(&head), "{spec}: {head}");
        }
    }

    /// A failure names its URL once: ureq's own message starts with it too.
    #[test]
    fn a_failure_names_its_url_once() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/asset", listener.local_addr().unwrap());
        drop(listener);
        let err = get(&url, &[], ROOMY).unwrap_err();
        assert_eq!(err.matches(url.as_str()).count(), 1, "{err}");
    }

    /// The URL ureq is handed is the one every decision was made on: the
    /// `url` crate's serialization — its host lowercase, its default port
    /// left out, its fragment dropped (never sent) — which ureq's parser reads
    /// the same; and what cannot be sent is refused before anything is.
    #[test]
    fn a_request_goes_where_its_url_reads() {
        let uri = request_uri("https://API.Example.com:443/v1/x?q=1#part").unwrap();
        assert_eq!(uri.to_string(), "https://api.example.com/v1/x?q=1");
        for (url, host, port) in [
            ("http://[::1]:8080/v1", "::1", 8080),
            (
                "https://user:pw@models.example:8443/",
                "models.example",
                8443,
            ),
            ("HTTP://LOCALHOST/x", "localhost", 80),
        ] {
            let target = Target::of(&request_uri(url).unwrap()).unwrap();
            assert_eq!((target.host.as_str(), target.port), (host, port), "{url}");
        }
        for bad in ["ftp://x.example/y", "file:///etc/passwd", "not a url"] {
            assert!(request_uri(bad).is_err(), "{bad}");
        }
    }

    // ---------------------------------------------------- over real TLS

    use crate::testutil::{TlsConn, TlsScript as Script, tls_answer as answer, tls_serve};

    /// Client trust for the test certificate, and nothing else.
    fn test_trust() -> Arc<rustls::ClientConfig> {
        crate::testutil::trusting_the_test_cert()
    }

    /// A wire whose connections trust the test certificate.
    fn test_wire(max_bytes: u64) -> Arc<Wire> {
        Arc::new(Wire::trusting(test_trust(), max_bytes))
    }

    /// [`transfer_on`] into memory over `wire`: the production path, worker
    /// and all, with the test trust.
    fn fetch_on(
        wire: Arc<Wire>,
        url: &str,
        limits: Limits,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>, String> {
        let mut body = Vec::new();
        transfer_on(wire, url, &[], limits, cancel, &mut body, &mut |_, _| {})?;
        Ok(body)
    }

    /// Every redirect of an HTTPS download is a hop of `transfer_with`'s —
    /// the real agent follows none itself. ureq following one inside the
    /// agent would take the first hop's proxy, and the caller's headers, to
    /// wherever it pointed, past every check here.
    #[test]
    fn an_https_agent_follows_no_redirect_of_its_own() {
        let base = tls_serve(vec![
            answer(b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\n\r\n"),
            answer(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndone"),
        ]);
        let url = format!("{base}/asset");
        let mut sent = Vec::new();
        let mut send = |agent: &ureq::Agent, hop: &Hop<'_>| {
            sent.push(hop.uri.to_string());
            send_hop(agent, hop)
        };
        let mut body = Vec::new();
        let result = transfer_with(
            &url,
            &[],
            ROOMY,
            &AtomicBool::new(false),
            &mut body,
            &mut |_, _| {},
            &env_only(&|_| None),
            &test_wire(ROOMY.max_bytes),
            &mut send,
        );
        assert_eq!(result, Ok(4));
        assert_eq!(body, b"done");
        assert_eq!(sent, [url, format!("{base}/next")]);
    }

    /// A Location header that is there but not UTF-8 is reported as such, not
    /// as a missing one.
    #[test]
    fn a_location_that_is_not_utf8_is_named() {
        let base = tls_serve(vec![answer(
            b"HTTP/1.1 302 Found\r\nLocation: /\xff\xfe\r\nContent-Length: 0\r\n\r\n",
        )]);
        let err = fetch_on(
            test_wire(1024),
            &format!("{base}/asset"),
            ROOMY,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.contains("not UTF-8"), "{err}");
    }

    /// fixR5 #1: a response that never ends fails fast, over plain http as
    /// over TLS, with the reason — and costs a bounded amount: the peer is
    /// hung up on long before its bound. An endless chunk-size line stops at
    /// the input buffer (a line cannot outgrow it), an endless header at the
    /// 64 KiB the headers may take, endless headers at the number ureq's
    /// parser takes, and an endless run of trailer lines — each one short,
    /// none of them body — at the meter every connection carries
    /// (`Metered`). Before, a plain-http connection had no meter at all: the
    /// trailers ran on until the deadline, a thread and a connection busy
    /// the whole time.
    #[test]
    fn a_response_that_never_ends_fails_fast_over_plain_http() {
        const BOUND: u64 = 64 << 20;
        let limits = Limits {
            max_bytes: 1024,
            deadline: Duration::from_secs(20),
        };
        let chunked: &'static [u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        for (head, unit, why) in [
            (chunked, &b"ffffffffffffffff"[..], "a line longer than"),
            (
                &b"HTTP/1.1 200 OK\r\nX-Pad: "[..],
                &b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"[..],
                "response headers exceed 64 KiB",
            ),
            (
                &b"HTTP/1.1 200 OK\r\n"[..],
                &b"X-Pad: aaaaaaaaaaaaaaaaaaaaaaaaaa\r\n"[..],
                "too many headers",
            ),
            (
                &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n"[..],
                &b"X-Trailer: aaaaaaaaaaaaaaaaaaa\r\n"[..],
                "byte limit",
            ),
        ] {
            let (base, sent) = crate::testutil::endless_http(head, unit, BOUND);
            let began = Instant::now();
            let err = get(&format!("{base}/asset"), &[], limits).unwrap_err();
            assert!(err.contains(why), "{why}: {err}");
            assert!(
                began.elapsed() < Duration::from_secs(8),
                "{why}: ran on until {:?}",
                began.elapsed()
            );
            let sent = sent.recv_timeout(Duration::from_secs(30)).unwrap();
            assert!(
                sent < 8 << 20,
                "{why}: the client took {sent} bytes without hanging up"
            );
        }
    }

    /// The byte cap binds what a connection delivers, not only the body the
    /// copy loop sees, over TLS as over plain http: a response that follows
    /// its last chunk with trailer lines that never end delivers no body byte
    /// at all, and fails at the meter instead of running on to the deadline.
    #[test]
    fn endless_trailers_cannot_outgrow_the_byte_cap_over_tls() {
        let base = tls_serve(vec![Box::new(|tls: &mut TlsConn| {
            let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n");
            // 64 MiB of trailers, then silence (bounded, so a client without
            // the meter costs 64 MiB, not all).
            let trailers = b"X-Trailer: aaaaaaaaaaaaaaaaaaa\r\n".repeat(2048);
            for _ in 0..1024 {
                if tls.write_all(&trailers).and_then(|()| tls.flush()).is_err() {
                    return;
                }
            }
            let mut probe = [0u8; 1];
            let _ = tls.sock.set_read_timeout(Some(Duration::from_secs(30)));
            let _ = std::io::Read::read(tls, &mut probe);
        })]);
        let limits = Limits {
            max_bytes: 1024,
            deadline: Duration::from_secs(10),
        };
        let started = Instant::now();
        let err = fetch_on(
            test_wire(limits.max_bytes),
            &format!("{base}/asset"),
            limits,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.contains("byte limit"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "the trailers ran on until {:?}",
            started.elapsed()
        );
    }

    /// fixR5 #6: a kept connection's meter counts one response at a time:
    /// two answers each well under the cap, and over it together, both come
    /// through — the second on the connection the first came on. Counted
    /// over the connection's life, the meter failed a healthy kept
    /// connection in the end.
    #[test]
    fn a_kept_connection_is_metered_per_response() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (served_tx, served) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            let body = "x".repeat(600);
            let mut answered = 0;
            for _ in 0..2 {
                if crate::testutil::read_http(&mut conn).head.is_empty() {
                    break;
                }
                let _ = write!(
                    conn,
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                answered += 1;
            }
            let _ = served_tx.send(answered);
        });
        let profile = Profile {
            response_cap: 1000,
            pooled: true,
            ..test_profile()
        };
        let agent = agent(Route::Direct, test_trust(), profile);
        for at in 0..2 {
            let body = call(&agent, &format!("{base}/x")).unwrap_or_else(|e| panic!("{at}: {e}"));
            assert_eq!(body.len(), 600);
        }
        assert_eq!(
            served.recv_timeout(Duration::from_secs(10)),
            Ok(2),
            "both answers came on one connection"
        );
    }

    /// The deadline and a cancel end a transfer even while a hop is in flight
    /// — here a peer that takes the request and never answers, which ureq
    /// would wait out for its whole 60 s stall timeout (a trickling one, for
    /// good) — and the abandoned connection is shut down, not left open.
    #[test]
    fn a_hop_in_flight_ends_at_the_deadline_or_a_cancel() {
        let (closed_tx, closed) = std::sync::mpsc::channel();
        let silent = |closed_tx: std::sync::mpsc::Sender<()>| -> Script {
            Box::new(move |tls: &mut TlsConn| {
                let _ = tls.sock.set_read_timeout(Some(Duration::from_secs(30)));
                let mut probe = [0u8; 1];
                // Ends when the client's side of the connection goes away.
                let _ = std::io::Read::read(tls, &mut probe);
                let _ = closed_tx.send(());
            })
        };
        let base = tls_serve(vec![silent(closed_tx.clone()), silent(closed_tx)]);
        let url = format!("{base}/asset");

        let brief = Limits {
            max_bytes: 1024,
            deadline: Duration::from_millis(500),
        };
        let started = Instant::now();
        let err = fetch_on(test_wire(1024), &url, brief, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("too long"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5), "{err}");
        closed
            .recv_timeout(Duration::from_secs(5))
            .expect("the timed-out connection was left open");

        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            setter.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let err = fetch_on(test_wire(1024), &url, ROOMY, &cancel).unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5), "{err}");
        closed
            .recv_timeout(Duration::from_secs(5))
            .expect("the cancelled connection was left open");
    }

    /// Whether whoever holds `line` besides `weak` lets go within `bound`: a
    /// request's worker, whose thread holds its line while it lives.
    fn let_go_within(line: &std::sync::Weak<Line>, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        while line.strong_count() > 0 {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// fixR5 #2: an abandon ends the transfer's WORKER, not just its caller's
    /// wait: the worker's connection is shut down under whatever it is blocked
    /// in — a plain-http body gone quiet, a TLS handshake never answered —
    /// and the worker thread ends at once. A plain-http connection used to be
    /// out of the abandon's reach (ureq 2 handed out only a TLS connection's
    /// socket): its worker sat out the read's 60 s stall timeout.
    #[test]
    fn an_abandoned_transfer_lets_its_worker_go() {
        // A plain-http peer that sends part of a body, then nothing.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let quiet_body = format!("http://{}/asset", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                crate::testutil::read_http_request(&mut conn);
                let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nabc");
                let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
                let _ = std::io::Read::read(&mut conn, &mut [0u8; 1]);
            }
        });
        let roomier = Limits {
            max_bytes: 1 << 20,
            ..ROOMY
        };
        let wire = test_wire(roomier.max_bytes);
        let line = Arc::downgrade(&wire.line);
        let cancel = AtomicBool::new(false);
        let err = transfer_on(
            wire,
            &quiet_body,
            &[],
            roomier,
            &cancel,
            &mut Vec::new(),
            &mut |done, _| {
                if done > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
        )
        .unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert!(
            let_go_within(&line, Duration::from_secs(2)),
            "the worker of a plain-http transfer outlived its cancel"
        );

        // A peer that takes the ClientHello and never answers it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unanswered = format!("https://{}/asset", listener.local_addr().unwrap());
        let (hello_tx, hello) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
                if std::io::Read::read(&mut conn, &mut [0u8; 512]).is_ok() {
                    let _ = hello_tx.send(());
                }
                let _ = std::io::Read::read(&mut conn, &mut [0u8; 1]);
            }
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        std::thread::spawn(move || {
            if hello.recv_timeout(Duration::from_secs(10)).is_ok() {
                setter.store(true, Ordering::Relaxed);
            }
        });
        let wire = test_wire(1024);
        let line = Arc::downgrade(&wire.line);
        let err = fetch_on(wire, &unanswered, ROOMY, &cancel).unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert!(
            let_go_within(&line, Duration::from_secs(2)),
            "the worker of an unanswered handshake outlived its cancel"
        );
    }

    /// An address no connect to gets an answer from: a listener whose queue
    /// is full. `None` on a system that answers — or refuses — a connect to
    /// a full queue all the same. The listener and what fills its queue come
    /// with it, to hold.
    fn black_hole() -> Option<(SocketAddr, Vec<socket2::Socket>)> {
        use socket2::{Domain, Socket, Type};
        let listener = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        listener
            .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
            .unwrap();
        listener.listen(1).unwrap();
        let addr = listener.local_addr().unwrap().as_socket().unwrap();
        let mut held = vec![listener];
        loop {
            if held.len() > 64 {
                eprintln!("no black hole: this system answers a connect to a full queue");
                return None;
            }
            let probe = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
            probe.set_nonblocking(true).unwrap();
            let _ = probe.connect(&addr.into());
            let mut polled = libc::pollfd {
                fd: probe.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: one valid `pollfd`, for a socket held open meanwhile.
            let unanswered = unsafe { libc::poll(&mut polled, 1, 300) } == 0;
            if !unanswered && probe.take_error().ok().flatten().is_some() {
                eprintln!("no black hole: this system refuses a connect to a full queue");
                return None;
            }
            held.push(probe);
            if unanswered {
                return Some((addr, held));
            }
        }
    }

    /// fixR5 #2: an abandon ends a connect in progress too. A SYN nobody
    /// answers holds a connect for its whole timeout, and std connects only
    /// with a timeout nothing can cut short; the connect here looks at the
    /// request's line as it waits. fixR7 #11: and with several addresses
    /// tried at once, every attempt's socket is on the line.
    #[test]
    fn an_abandon_ends_a_connect_in_progress() {
        let (Some((first, _first_held)), Some((second, _second_held))) =
            (black_hole(), black_hole())
        else {
            eprintln!("skipped");
            return;
        };
        let line = Arc::new(Line::default());
        let worker = {
            let line = line.clone();
            std::thread::spawn(move || {
                connect_to(
                    &[first, second],
                    Instant::now() + Duration::from_secs(30),
                    Some(&line),
                )
            })
        };
        // Past the attempt delay: both attempts are under way.
        std::thread::sleep(ATTEMPT_DELAY + Duration::from_millis(250));
        assert!(!worker.is_finished(), "precondition: the connect hangs");
        let held = line
            .state()
            .held
            .iter()
            .filter(|(lease, _)| lease.upgrade().is_some())
            .count();
        assert_eq!(held, 2, "not every attempt is on the line");
        line.close();
        let closed = Instant::now();
        while !worker.is_finished() && closed.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(worker.is_finished(), "the connect outlived the abandon");
        let err = worker.join().unwrap().unwrap_err();
        assert_eq!(err.to_string(), ABANDONED);
    }

    /// fixR7 #11: several addresses are tried as RFC 8305's Happy Eyeballs
    /// tries them. One that never answers gives way to the next within the
    /// attempt delay — it used to hold the connect for half the time left —
    /// and the next one's connection is the one made.
    #[test]
    fn a_silent_address_gives_way_within_the_attempt_delay() {
        let Some((silent, _held)) = black_hole() else {
            eprintln!("skipped");
            return;
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = listener.local_addr().unwrap();
        let began = Instant::now();
        let (stream, _) =
            connect_to(&[silent, live], began + Duration::from_secs(10), None).unwrap();
        let took = began.elapsed();
        assert_eq!(stream.peer_addr().unwrap(), live);
        assert!(took < Duration::from_secs(2), "connected after {took:?}");
    }

    /// fixR7 #11: any failure of an attempt is its address's alone — a
    /// family this host has no network for (an IPv6 socket on a Linux host
    /// with IPv6 switched off), a firewall's refusal — and the next address
    /// is tried. Such a failure used to end the whole connect.
    #[test]
    fn a_failing_address_or_family_is_passed_over() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = listener.local_addr().unwrap();
        let v6 = SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1], live.port()));
        let failing = |errno: i32| {
            move |addr: SocketAddr| {
                if addr.is_ipv6() {
                    Err(io::Error::from_raw_os_error(errno))
                } else {
                    open_socket(addr)
                }
            }
        };
        for errno in [
            libc::EAFNOSUPPORT,
            libc::EACCES,
            libc::EPERM,
            libc::ENETDOWN,
        ] {
            let deadline = Instant::now() + Duration::from_secs(10);
            let (stream, _) = connect_with(&[v6, live], deadline, None, &failing(errno))
                .unwrap_or_else(|e| panic!("{errno}: {e}"));
            assert_eq!(stream.peer_addr().unwrap(), live, "{errno}");
        }
        // One address, failing: its failure, as it is.
        let deadline = Instant::now() + Duration::from_secs(10);
        let err = connect_with(&[v6], deadline, None, &failing(libc::EAFNOSUPPORT)).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EAFNOSUPPORT), "{err}");
    }

    /// fixR9 #7: when no address takes the connection, the failure is the
    /// one that says most of the target, and names each address with how it
    /// failed — whichever failed last. Only the last failure was reported:
    /// a server that was down (its IPv4 port refused) on a host with no IPv6
    /// (every IPv6 address unreachable) read as a network fault or not, by
    /// the order the resolver gave the two families in.
    #[test]
    fn a_connect_no_address_takes_says_what_says_most() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let v6 = SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1], closed.port()));
        let unreachable = |addr: SocketAddr| {
            if addr.is_ipv6() {
                Err(io::Error::from_raw_os_error(libc::ENETUNREACH))
            } else {
                open_socket(addr)
            }
        };
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused).to_string();
        let lost = io::Error::from_raw_os_error(libc::ENETUNREACH).to_string();
        for addrs in [[closed, v6], [v6, closed]] {
            let deadline = Instant::now() + Duration::from_secs(10);
            let err = connect_with(&addrs, deadline, None, &unreachable).unwrap_err();
            assert_eq!(
                err.kind(),
                io::ErrorKind::ConnectionRefused,
                "{addrs:?}: {err}"
            );
            let text = err.to_string();
            assert!(
                text.contains(&format!(" at {closed}; also tried {v6}: {lost}")),
                "{addrs:?}: {text}"
            );
            assert!(
                text.to_lowercase().starts_with(&refused.to_lowercase()),
                "{addrs:?}: {text}"
            );
        }
        // Past a few, the rest are counted.
        let many: Vec<SocketAddr> = (1..=5)
            .map(|last| SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, last], 443)))
            .chain([closed])
            .collect();
        let deadline = Instant::now() + Duration::from_secs(10);
        let err = connect_with(&many, deadline, None, &unreachable).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err}");
        assert!(err.to_string().ends_with(", and 2 more"), "{err}");
    }

    /// fixR7 #11: addresses are taken in turns by family, starting with the
    /// family the resolver put first, each family in the resolver's order.
    #[test]
    fn addresses_are_tried_in_turns_by_family() {
        let v6 = |last: u16| SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, last], 443));
        let v4 = |last: u8| SocketAddr::from(([192, 0, 2, last], 443));
        assert_eq!(
            interleaved(&[v6(1), v6(2), v6(3), v4(1), v4(2)]),
            [v6(1), v4(1), v6(2), v4(2), v6(3)]
        );
        assert_eq!(interleaved(&[v4(1), v6(1), v4(2)]), [v4(1), v6(1), v4(2)]);
        assert_eq!(interleaved(&[v4(1), v4(2)]), [v4(1), v4(2)]);
        assert!(interleaved(&[]).is_empty());
    }

    /// fixR7 #10: name lookups, which cannot be interrupted, are bounded
    /// without holding up the ones that can be answered:
    ///
    /// - `localhost` and every name under it are loopback, with no resolver
    ///   asked — not one known to hang, not even once the bound is reached;
    /// - one lookup per name is under way at a time: callers side by side
    ///   wait on it, and a name the resolver does not answer holds one thread
    ///   however many requests retry it — and holds up no other name;
    /// - at most `MAX_LOOKUPS` names are looked up at once, however many
    ///   callers race for the last places: past that, a new name is refused
    ///   at once, saying why;
    /// - once the resolver answers, the places are free again;
    /// - an abandon ends the wait for a lookup at once.
    ///
    /// fixR5's bound counted the lookups given up on, across every name, and
    /// was checked before one began: four stuck lookups of one name refused
    /// every other lookup, `localhost` included, and callers side by side
    /// could all pass the check.
    #[test]
    fn lookups_are_bounded_per_name_and_in_all() {
        use std::sync::{Barrier, Condvar};
        static LOOKUPS_HERE: Lookups = Lookups(Mutex::new(Vec::new()));
        static RACED: Lookups = Lookups(Mutex::new(Vec::new()));
        static OPEN: Mutex<bool> = Mutex::new(false);
        static OPENED: Condvar = Condvar::new();
        static ASKED: Mutex<Vec<String>> = Mutex::new(Vec::new());
        // A resolver that holds every lookup of a `hung…` name until the gate
        // opens, answers any other at once, and notes every name it is asked.
        fn gated(name: &str) -> io::Result<Vec<IpAddr>> {
            ASKED.lock().unwrap().push(name.to_string());
            if !name.starts_with("hung") {
                return Ok(vec![IpAddr::from([192, 0, 2, 1])]);
            }
            let mut open = OPEN.lock().unwrap();
            while !*open {
                open = OPENED.wait(open).unwrap();
            }
            Ok(vec![IpAddr::from([192, 0, 2, 7])])
        }
        let asked = |name: &str| ASKED.lock().unwrap().iter().filter(|n| *n == name).count();
        let within = |millis| Instant::now() + Duration::from_millis(millis);
        let look = |name: &str, deadline: Instant| {
            lookup_with(&gated, &LOOKUPS_HERE, name, 80, deadline, None)
        };
        let loopback = |port| {
            vec![
                SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
                SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
            ]
        };
        for name in ["localhost", "LocalHost.", "models.localhost"] {
            assert_eq!(look(name, within(300)).unwrap(), loopback(80), "{name}");
        }
        assert!(
            ASKED.lock().unwrap().is_empty(),
            "a resolver was asked for loopback"
        );

        // Callers racing for more names than there are places: the places
        // are all taken, and not one more.
        let racers = 2 * MAX_LOOKUPS;
        let start = Arc::new(Barrier::new(racers));
        let raced: Vec<_> = (0..racers)
            .map(|at| {
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    let name = format!("hung-raced-{at}.example");
                    lookup_with(&gated, &RACED, &name, 80, within(300), None)
                })
            })
            .collect();
        let (mut waited, mut refused) = (0, 0);
        for racer in raced {
            let err = racer.join().unwrap().unwrap_err();
            if err.kind() == io::ErrorKind::TimedOut {
                waited += 1;
            } else {
                assert!(err.to_string().contains("not looking up"), "{err}");
                refused += 1;
            }
        }
        assert_eq!((waited, refused), (MAX_LOOKUPS, MAX_LOOKUPS));
        let raced_asked = ASKED
            .lock()
            .unwrap()
            .iter()
            .filter(|n| n.starts_with("hung-raced"))
            .count();
        assert_eq!(
            raced_asked, MAX_LOOKUPS,
            "more lookups ran than the bound allows"
        );

        // Eight requests for one name the resolver does not answer: one
        // lookup, which each of them gives up on at its own deadline.
        let callers: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(move || look("hung.example", within(300))))
            .collect();
        for caller in callers {
            let err = caller.join().unwrap().unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        }
        assert_eq!(
            asked("hung.example"),
            1,
            "the resolver was asked more than once"
        );
        // Another name is answered meanwhile.
        assert_eq!(
            look("fine.example", within(300)).unwrap(),
            [SocketAddr::from(([192, 0, 2, 1], 80))]
        );

        // Full: a name under way is still waited on, loopback still needs no
        // lookup, and a new name is refused at once.
        for at in 1..MAX_LOOKUPS {
            let err = look(&format!("hung-{at}.example"), within(20)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        }
        assert_eq!(LOOKUPS_HERE.list().len(), MAX_LOOKUPS);
        let err = look("hung.example", within(300)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert_eq!(look("localhost", within(300)).unwrap(), loopback(80));
        let began = Instant::now();
        let err = look("new.example", within(10_000)).unwrap_err();
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        assert!(
            err.to_string().contains("not looking up new.example"),
            "{err}"
        );

        // The resolver answers: the places are free, and lookups go on.
        *OPEN.lock().unwrap() = true;
        OPENED.notify_all();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(LOOKUPS_HERE.list().is_empty() && RACED.list().is_empty())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(LOOKUPS_HERE.list().is_empty(), "the lookups did not end");
        assert_eq!(
            look("new.example", within(300)).unwrap(),
            [SocketAddr::from(([192, 0, 2, 1], 80))]
        );

        // A resolver that panics is a failed lookup, and leaves no name
        // behind on the list.
        static PANICKED: Lookups = Lookups(Mutex::new(Vec::new()));
        fn panics(_: &str) -> io::Result<Vec<IpAddr>> {
            panic!("a resolver failing badly (this panic is the test's)")
        }
        let err =
            lookup_with(&panics, &PANICKED, "any.example", 80, within(5_000), None).unwrap_err();
        assert!(err.to_string().contains("the resolver failed"), "{err}");
        assert!(PANICKED.list().is_empty(), "the name stayed on the list");

        // An abandon ends the wait for a lookup at once.
        static PARKED_LOOKUPS: Lookups = Lookups(Mutex::new(Vec::new()));
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let entered_tx = Mutex::new(entered_tx);
        let parked: &'static Resolve =
            Box::leak(Box::new(move |_: &str| -> io::Result<Vec<IpAddr>> {
                let _ = entered_tx.lock().unwrap().send(());
                std::thread::sleep(Duration::from_secs(5));
                Err(io::Error::other("gave up"))
            }));
        let line = Arc::new(Line::default());
        let closer = line.clone();
        std::thread::spawn(move || {
            if entered.recv_timeout(Duration::from_secs(10)).is_ok() {
                closer.close();
            }
        });
        let began = Instant::now();
        let later = Instant::now() + Duration::from_secs(10);
        let err = lookup_with(
            parked,
            &PARKED_LOOKUPS,
            "slow.example",
            80,
            later,
            Some(&line),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), ABANDONED);
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "{:?}",
            began.elapsed()
        );
    }

    /// ureq 2 panicked handing back a connection its peer had reset: it
    /// cleared the socket's timeouts under an `expect`, which macOS refuses
    /// on such a socket. The socket is [`Tcp`]'s now, and a reset is only
    /// ever an error: the connection is not kept, and a read or a write on
    /// it fails as one — whatever the system makes of the timeouts.
    #[test]
    fn a_reset_connection_is_an_error_and_not_kept() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server_side, _) = listener.accept().unwrap();
        crate::testutil::abort(server_side);
        // The reset has landed once the socket has no peer any more.
        let deadline = Instant::now() + Duration::from_secs(10);
        while client.peer_addr().is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut tcp = Tcp {
            lease: Lease::new(client.try_clone().unwrap()),
            stream: client,
            buffers: LazyBuffers::new(1024, 1024),
            read_idle: Some(Duration::from_secs(1)),
            write_idle: Some(Duration::from_secs(1)),
            read_timeout: None,
            write_timeout: None,
        };
        assert!(!tcp.is_open(), "a reset connection was kept");
        let timeout = NextTimeout {
            after: Duration::from_secs(1).into(),
            reason: Timeout::RecvBody,
        };
        assert!(
            !matches!(tcp.await_input(timeout), Ok(true)),
            "a reset connection read data"
        );
        tcp.buffers.output()[..4].copy_from_slice(b"GET ");
        let _ = tcp.transmit_output(4, timeout);
    }

    // ------------------------------------------------------------- fixR7

    /// The bypass advice for a proxy named in the environment, as
    /// [`Via::advice`] words it.
    const ENV_BYPASS: &str = "list the host in no_proxy, which clew reads in place of NO_PROXY";

    /// The bypass advice for a system proxy, as [`Via::advice`] words it.
    const SYSTEM_BYPASS: &str = "list the host under \u{201c}Bypass proxy settings for these \
                                 hosts\u{201d} in System Settings \u{203a} Network \u{203a} \
                                 Details \u{203a} Proxies, or in no_proxy, which clew reads in \
                                 place of NO_PROXY";

    /// How [`Via::advice`] says to give a system proxy the password the
    /// keychain holds: in the environment, which then stands in for all of
    /// the system's settings — `set`, as the environment names them.
    fn keychain(set: &str) -> String {
        format!(
            " — clew does not read proxy passwords from the keychain: name them in the \
             environment (percent-encoding any character in them that is not a letter or a \
             digit), which clew then reads instead of all of the system's proxy settings — so \
             set {set}"
        )
    }

    /// fixR7 #2: a proxy that asks for a user name and password, or refuses
    /// the ones its setting names, says so once — and the way to give them
    /// is a setting of the proxy's own kind, with its own host and port: a
    /// SOCKS one through `ALL_PROXY` as `socks5h://`, an http one through
    /// the variable of the request's scheme. The advice used to say again
    /// what the failure had said (`asks for a user name and password — the
    /// proxy asks for a user name and password`), to point an `http://`
    /// setting at a SOCKS port, and to ask for credentials the setting
    /// already named.
    #[test]
    fn a_proxy_that_wants_credentials_says_how_to_give_them() {
        use crate::testutil::{ProxyAnswer, SocksAnswer, http_proxy, socks5_proxy};
        let never = AtomicBool::new(false);
        let url = "https://models.lan.invalid/v1/asset";
        let fetch = |sources: &ProxySources<'_>| {
            let mut body = Vec::new();
            transfer_with(
                url,
                &[],
                ROOMY,
                &never,
                &mut body,
                &mut |_, _| {},
                sources,
                &Wire::new(1024),
                &mut send_hop,
            )
            .unwrap_err()
        };
        let system = |spec: String| {
            move || {
                Some(ProxyConfig {
                    http: Some(spec.clone()),
                    https: Some(spec.clone()),
                    bypass: Vec::new(),
                    bypass_simple: false,
                })
            }
        };
        let env = |spec: String| move |name: &str| (name == "https_proxy").then(|| spec.clone());
        let no_env = |_: &str| None;

        // A system SOCKS proxy that wants a login clew has none for: the
        // keychain's password goes into the environment, with the rest of
        // the system's settings (fixR9 #4).
        let (addr, _) = socks5_proxy(Some(("me", "pw")), SocksAnswer::Relay);
        let settings = system(format!("socks5h://{addr}"));
        let err = fetch(&ProxySources {
            env: &no_env,
            system: &settings,
        });
        assert_eq!(
            err,
            format!(
                "{url} (through the proxy socks5h://{addr}): the SOCKS proxy {addr} asks for a \
                 user name and password{}, or {SYSTEM_BYPASS}",
                keychain(&format!("ALL_PROXY=socks5h://user:password@{addr}"))
            )
        );

        // The same proxy named by the environment.
        let (addr, _) = socks5_proxy(Some(("me", "pw")), SocksAnswer::Relay);
        let setting = env(format!("socks5h://{addr}"));
        let err = fetch(&env_only(&setting));
        assert_eq!(
            err,
            format!(
                "{url} (through the proxy socks5h://{addr}): the SOCKS proxy {addr} asks for a \
                 user name and password — name them in its setting, as \
                 socks5h://user:password@{addr}, or {ENV_BYPASS}"
            )
        );

        // One whose setting names a login it refuses.
        let (addr, seen) = socks5_proxy(Some(("me", "pw")), SocksAnswer::Relay);
        let setting = env(format!("socks5h://me:wrong@{addr}"));
        let err = fetch(&env_only(&setting));
        assert_eq!(
            err,
            format!(
                "{url} (through the proxy socks5h://…@{addr}): the SOCKS proxy {addr} refused \
                 the user name and password — correct them in its setting \
                 (socks5h://user:password@{addr}, percent-encoding any character in them that \
                 is not a letter or a digit), or {ENV_BYPASS}"
            )
        );
        let seen = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(seen.login, Some(("me".into(), "wrong".into())));

        // A system http proxy answering the CONNECT with a 407.
        let (proxy_url, _) = http_proxy("127.0.0.1:0", false, 1, ProxyAnswer::Status(407)).unwrap();
        let addr = proxy_url.trim_start_matches("http://").to_string();
        let settings = system(proxy_url.clone());
        let err = fetch(&ProxySources {
            env: &no_env,
            system: &settings,
        });
        assert_eq!(
            err,
            format!(
                "{url} (through the proxy {proxy_url}): the proxy {addr} answered the CONNECT to \
                 models.lan.invalid:443 with HTTP/1.1 407 Refused: it asks for a user name and \
                 password{}, or {SYSTEM_BYPASS}",
                keychain(&format!(
                    "https_proxy=http://user:password@{addr} http_proxy=http://user:password@{addr}"
                ))
            )
        );

        // An http proxy named by the environment, which refuses the login
        // its setting names.
        let (proxy_url, _) = http_proxy("127.0.0.1:0", false, 1, ProxyAnswer::Status(407)).unwrap();
        let addr = proxy_url.trim_start_matches("http://").to_string();
        let setting = env(format!("http://me:wrong@{addr}"));
        let err = fetch(&env_only(&setting));
        assert_eq!(
            err,
            format!(
                "{url} (through the proxy http://…@{addr}): the proxy {addr} answered the \
                 CONNECT to models.lan.invalid:443 with HTTP/1.1 407 Refused: it refused the user \
                 name and password — correct them in its setting (http://user:password@{addr}, \
                 percent-encoding any character in them that is not a letter or a digit), or \
                 {ENV_BYPASS}"
            )
        );

        // fixR9 #3: a proxy that takes only logins clew cannot make says so,
        // and what can make them — however right the password its setting
        // names: it used to be told to correct it.
        let (proxy_url, _) = http_proxy(
            "127.0.0.1:0",
            false,
            1,
            ProxyAnswer::Login("Negotiate, NTLM"),
        )
        .unwrap();
        let addr = proxy_url.trim_start_matches("http://").to_string();
        let setting = env(format!("http://me:right@{addr}"));
        let err = fetch(&env_only(&setting));
        assert_eq!(
            err,
            format!(
                "{url} (through the proxy http://…@{addr}): the proxy {addr} answered the \
                 CONNECT to models.lan.invalid:443 with HTTP/1.1 407 Refused: it takes only \
                 Negotiate or NTLM logins, which clew cannot make — name a local relay that \
                 makes such logins for clew, such as cntlm or px, as the proxy instead, or \
                 {ENV_BYPASS}"
            )
        );

        // A 407 to a plain-http request, which the proxy answers itself: the
        // logins its challenges offer ([`Via::answered_407`]).
        let system = ProxyConfig {
            http: Some("http://proxy.corp:3128".into()),
            https: Some("http://proxy.corp:3128".into()),
            bypass: vec!["*.local".into(), "169.254/16".into()],
            bypass_simple: false,
        };
        let plain = Via {
            spec: "http://proxy.corp:3128".into(),
            system: Some(system),
            for_https: false,
        };
        let asks = format!(
            "the proxy http://proxy.corp:3128 answered HTTP 407 (Proxy Authentication \
             Required): it asks for a user name and password{}, or {SYSTEM_BYPASS}",
            keychain(
                "http_proxy=http://user:password@proxy.corp:3128 \
                 https_proxy=http://user:password@proxy.corp:3128 no_proxy=*.local,169.254/16"
            )
        );
        assert_eq!(plain.answered_407(&ureq::http::HeaderMap::new()), asks);
        let mut challenges = ureq::http::HeaderMap::new();
        for value in ["NTLM", "Negotiate"] {
            challenges.append("proxy-authenticate", value.parse().unwrap());
        }
        let named = Via {
            spec: "http://me:right@proxy.corp:3128".into(),
            system: None,
            for_https: false,
        };
        assert_eq!(
            named.answered_407(&challenges),
            format!(
                "the proxy http://…@proxy.corp:3128 answered HTTP 407 (Proxy Authentication \
                 Required): it takes only NTLM or Negotiate logins, which clew cannot make — \
                 name a local relay that makes such logins for clew, such as cntlm or px, as the \
                 proxy instead, or {ENV_BYPASS}"
            )
        );
        challenges.append(
            "proxy-authenticate",
            "Basic realm=\"corp\"".parse().unwrap(),
        );
        assert!(
            named
                .answered_407(&challenges)
                .contains("it refused the user name and password — correct them in its setting"),
            "Basic is offered too"
        );
        assert_eq!(plain.answered_407(&challenges), asks);

        // The example, as the setting's own kind writes it.
        for (spec, example) in [
            (
                "https://[fd00::1]:3128",
                "https://user:password@[fd00::1]:3128",
            ),
            ("proxy.corp:8080", "http://user:password@proxy.corp:8080"),
            (
                "socks5://proxy.corp",
                "socks5://user:password@proxy.corp:1080",
            ),
            (
                "socks4a://proxy.corp:1081",
                "socks4a://user@proxy.corp:1081",
            ),
        ] {
            let via = Via {
                spec: spec.into(),
                system: None,
                for_https: true,
            };
            assert_eq!(via.example(), example, "{spec}");
        }
    }

    /// fixR9 #4: a proxy named in the environment replaces ALL of the
    /// system's settings, so the way to give a system proxy the password the
    /// keychain holds names all of them there: each scheme's proxy, the
    /// credentials in the one that asked for them — the proxy of the
    /// request's scheme — and the exceptions, as `NO_PROXY`. It named the one
    /// variable of the request's scheme: set as advised, the other scheme's
    /// requests went around the proxy, and every host the system exempts
    /// went through it. Set as advised now, every request goes where the
    /// system's settings sent it.
    ///
    /// fixR10 #3: the exceptions as `no_proxy`, lowercase as the proxy
    /// variables are. Of the two spellings the lowercase one is read first,
    /// so in a shell that already set `no_proxy`, an advised `NO_PROXY` went
    /// unread — and every host the system exempts through the proxy.
    #[test]
    fn the_keychain_advice_restates_every_system_setting() {
        let settings = || {
            Some(ProxyConfig {
                http: Some("http://web.corp:8080".into()),
                https: Some("http://secure.corp:8443".into()),
                bypass: vec!["*.local".into(), "169.254/16".into()],
                bypass_simple: false,
            })
        };
        let no_proxy = |name: &str| (name == "NO_PROXY").then(|| "intranet.corp".to_string());
        let system = ProxySources {
            env: &no_proxy,
            system: &settings,
        };
        let (https_url, http_url) = ("https://api.example.com/v1", "http://models.example.com/v1");
        let https = via(https_url, &system).unwrap();
        let http = via(http_url, &system).unwrap();
        assert!(https.for_https && !http.for_https);
        assert_eq!(
            (https.spec.as_str(), http.spec.as_str()),
            ("http://secure.corp:8443", "http://web.corp:8080")
        );
        let exempt = "no_proxy=*.local,169.254/16,intranet.corp";
        assert_eq!(
            https.advice(ProxyFault::Credentials),
            format!(
                "{}, or {SYSTEM_BYPASS}",
                keychain(&format!(
                    "https_proxy=http://user:password@secure.corp:8443 \
                     http_proxy=http://web.corp:8080 {exempt}"
                ))
            )
        );
        assert_eq!(
            http.advice(ProxyFault::Credentials),
            format!(
                "{}, or {SYSTEM_BYPASS}",
                keychain(&format!(
                    "http_proxy=http://user:password@web.corp:8080 \
                     https_proxy=http://secure.corp:8443 {exempt}"
                ))
            )
        );

        // Set as advised — every variable the advice names, over a shell
        // that set its own list of exceptions already, in either spelling —
        // the environment routes as the system did.
        let hop = |url: &str, sources: &ProxySources<'_>| {
            proxy_for(url, sources).and_then(|spec| route(Some(&spec)).ok()?.first_hop())
        };
        let urls = [
            https_url,
            http_url,
            "https://printer.local/",
            "http://169.254.7.7/",
            "https://intranet.corp/",
        ];
        for spelling in ["NO_PROXY", "no_proxy"] {
            let shell = move |name: &str| (name == spelling).then(|| "intranet.corp".to_string());
            let system = ProxySources {
                env: &shell,
                system: &settings,
            };
            for failed in [https_url, http_url] {
                let advice = via(failed, &system)
                    .unwrap()
                    .advice(ProxyFault::Credentials);
                // What it says to set: `name=value`, each with a password.
                let (_, set) = advice.split_once(" — so set ").expect("variables to set");
                let (set, _) = set.split_once(", or ").expect("the bypass after them");
                let set: Vec<(&str, String)> = set
                    .split(' ')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(name, value)| (name, value.replace("user:password", "me:pw")))
                    .collect();
                let advised = |name: &str| {
                    set.iter()
                        .find(|(named, _)| *named == name)
                        .map(|(_, value)| value.clone())
                        .or_else(|| shell(name))
                };
                for url in urls {
                    assert_eq!(
                        hop(url, &env_only(&advised)),
                        hop(url, &system),
                        "{spelling}, {failed} failed: {url}"
                    );
                }
            }
        }

        // One SOCKS proxy for both schemes is ALL_PROXY; names without a
        // dot, which the system exempts as a class, no_proxy exempts one by
        // one.
        let socks = || {
            Some(ProxyConfig {
                http: Some("socks5h://socks.corp:1080".into()),
                https: Some("socks5h://socks.corp:1080".into()),
                bypass: Vec::new(),
                bypass_simple: true,
            })
        };
        let sources = ProxySources {
            env: &|_| None,
            system: &socks,
        };
        let advice = via(http_url, &sources)
            .unwrap()
            .advice(ProxyFault::Credentials);
        assert!(
            advice.ends_with(&format!(
                "set ALL_PROXY=socks5h://user:password@socks.corp:1080 (no_proxy also listing \
                 each name without a dot you use, which the system exempts), or {SYSTEM_BYPASS}"
            )),
            "{advice}"
        );
    }

    /// fixR11 #6: the way around a proxy is `no_proxy`, lowercase, as the
    /// keychain advice names the exceptions (fixR10 #3): of the two
    /// spellings the lowercase one is read, the other only while it is
    /// unset. Listed in `NO_PROXY` as advised, the host went through the
    /// proxy still in a shell that had set `no_proxy`. And the advice says
    /// whose place `no_proxy` takes: followed to the letter in a shell that
    /// had set `NO_PROXY`, it keeps every host that list exempts. Here
    /// followed in a shell that set either, under a proxy the environment
    /// names and under the system's.
    #[test]
    fn the_bypass_advice_names_the_list_clew_reads() {
        type Shell<'a> = &'a dyn Fn(&str) -> Option<String>;
        type System<'a> = &'a dyn Fn() -> Option<ProxyConfig>;
        let (url, host) = ("https://api.example.com/v1", "api.example.com");
        let settings = || {
            Some(ProxyConfig {
                http: None,
                https: Some("http://proxy.corp:3128".into()),
                bypass: Vec::new(),
                bypass_simple: false,
            })
        };
        for spelling in ["NO_PROXY", "no_proxy"] {
            // A shell with a list of its own — and the proxy, when the
            // environment names it.
            let listed = move |name: &str| (name == spelling).then(|| "intranet.corp".to_string());
            let named = move |name: &str| {
                (name == "https_proxy")
                    .then(|| "http://proxy.corp:3128".to_string())
                    .or_else(|| listed(name))
            };
            let cases: [(&str, Shell<'_>, System<'_>); 2] = [
                ("the environment's", &named, &no_system),
                ("the system's", &listed, &settings),
            ];
            for (whose, shell, system) in cases {
                let advice = via(url, &ProxySources { env: shell, system })
                    .expect("through the proxy")
                    .advice(ProxyFault::Refused);
                let what = format!("{spelling} set, {whose} proxy: {advice}");
                // The list it names, and the one it says that list takes
                // the place of.
                let (list, replaced) = match advice.rsplit_once(", which clew reads in place of ") {
                    Some((list, replaced)) => (list, Some(replaced)),
                    None => (advice.as_str(), None),
                };
                let variable = list.rsplit(' ').next().unwrap();
                // Followed to the letter: the host added to that list, with
                // what the list it takes the place of listed.
                let value: Vec<String> = [Some(variable), replaced]
                    .into_iter()
                    .flatten()
                    .filter_map(shell)
                    .chain([host.to_string()])
                    .collect();
                let value = value.join(",");
                let advised = |name: &str| {
                    if name == variable {
                        Some(value.clone())
                    } else {
                        shell(name)
                    }
                };
                let followed = ProxySources {
                    env: &advised,
                    system,
                };
                assert!(
                    via(url, &followed).is_none(),
                    "the host is not exempt: {what}"
                );
                assert!(
                    via("https://intranet.corp/", &followed).is_none(),
                    "the shell's exempt host is not: {what}"
                );
                assert!(
                    via("https://other.example.com/", &followed).is_some(),
                    "{what}"
                );
            }
        }
    }

    /// fixR7 #3: over TLS, a response whose body ends with its connection —
    /// no length, not chunked — is whole only when the peer ends TLS with
    /// its close_notify. A connection that just ends, or is reset — by the
    /// peer, or by anyone on the path — used to pass for the end of the body
    /// (ureq takes rustls's early end, a reset or an abort for the peer
    /// closing), and the part that had come was delivered as all of it.
    #[test]
    fn a_tls_body_ends_only_with_the_peers_close_notify() {
        let head: &'static [u8] = b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\npartial";
        let cut: Script = Box::new(move |tls: &mut TlsConn| {
            let _ = tls.write_all(head);
            let _ = tls.flush();
            let _ = tls.sock.shutdown(Shutdown::Write);
            // Held until the client lets go.
            let _ = tls.sock.set_read_timeout(Some(Duration::from_secs(20)));
            let _ = std::io::Read::read(&mut tls.sock, &mut [0u8; 1]);
        });
        let reset: Script = Box::new(move |tls: &mut TlsConn| {
            let _ = tls.write_all(head);
            let _ = tls.flush();
            // Time for the client to take what came, then a reset: the
            // socket lingers for none, and closes as the script returns.
            std::thread::sleep(Duration::from_millis(300));
            crate::testutil::abort(tls.sock.try_clone().unwrap());
        });
        let whole = answer(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\nwhole");
        let base = tls_serve(vec![cut, reset, whole]);
        let url = format!("{base}/asset");
        let never = AtomicBool::new(false);
        for how in ["cut", "reset"] {
            let err = fetch_on(test_wire(1024), &url, ROOMY, &never)
                .expect_err(&format!("{how}: a cut answer passed for a whole one"));
            // Said as the body's cut, or — a reset that lands before the
            // answer was read — as a peer that never answered.
            assert!(
                err.contains("closed before all bytes were read")
                    || err.contains("closed the connection before answering"),
                "{how}: {err}"
            );
        }
        assert_eq!(
            fetch_on(test_wire(1024), &url, ROOMY, &never).as_deref(),
            Ok(&b"whole"[..])
        );
    }

    /// fixR9 #8, #9: a TLS session that ends under an answer with no
    /// close_notify is judged by the answer's framing, and said as what it
    /// was to the answer.
    ///
    /// - A chunked body whose last chunk (`0\r\n`) had come is whole: every
    ///   byte of it came, framed — only the line that ends the body is
    ///   missing, as from a server that closes without it, whose body ureq
    ///   takes on purpose. fixR7 failed it. One cut before its last chunk,
    ///   or inside a chunk, still fails.
    /// - A peer that closes before answering is said to, not to have cut
    ///   short what it sent — a stale kept connection's usual end.
    #[test]
    fn a_tls_answer_that_ends_early_is_judged_by_its_framing() {
        // `answer`, then the connection's end with no close_notify.
        let fin = |answer: &'static [u8]| -> Script {
            Box::new(move |tls: &mut TlsConn| {
                let _ = tls.write_all(answer);
                let _ = tls.flush();
                let _ = tls.sock.shutdown(Shutdown::Write);
                // Held until the client lets go.
                let _ = tls.sock.set_read_timeout(Some(Duration::from_secs(20)));
                let _ = std::io::Read::read(&mut tls.sock, &mut [0u8; 1]);
            })
        };
        let base = tls_serve(vec![
            fin(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nwhole\r\n0\r\n"),
            fin(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nwhole\r\n"),
            fin(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nwho"),
            fin(b""),
            fin(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\npartial"),
        ]);
        let agent = agent(Route::Direct, test_trust(), test_profile());
        let get = |at: u8| call(&agent, &format!("{base}/{at}"));
        assert_eq!(get(1).as_deref(), Ok("whole"));
        for at in [2, 3] {
            let err = get(at).expect_err("a chunked body cut short passed for a whole one");
            assert!(
                err.contains("the connection closed before the whole response arrived"),
                "{at}: {err}"
            );
        }
        assert_eq!(
            get(4),
            Err("the peer closed the connection before answering".to_string())
        );
        let err = get(5).expect_err("a body its close delimits passed for whole");
        assert!(
            err.ends_with("(no close_notify): the answer may have been cut short"),
            "{err}"
        );
    }

    /// fixR10 #2: an answer whose head ends a line with a bare LF — as
    /// httparse, ureq's parser, lets it — is framed as ureq frames it. Its
    /// head was split on CRLF alone here, and read otherwise than ureq read
    /// it: the first answer below as chunked, where ureq's first
    /// `Transfer-Encoding` is `gzip`; the second read on past the `\n\n`
    /// that ended ureq's head, into a body that looks like a chunked
    /// answer's head. Each body ureq reads to the connection's end, which
    /// ends with no close_notify — and each passed for whole. So does the
    /// CRLF answer after them, which never did. A bare-LF head that is
    /// chunked is so to both: its body whose last chunk came is whole.
    #[test]
    fn a_bare_lf_head_is_framed_as_ureq_frames_it() {
        let fin = |answer: &'static [u8]| -> Script {
            Box::new(move |tls: &mut TlsConn| {
                let _ = tls.write_all(answer);
                let _ = tls.flush();
                let _ = tls.sock.shutdown(Shutdown::Write);
                // Held until the client lets go.
                let _ = tls.sock.set_read_timeout(Some(Duration::from_secs(20)));
                let _ = std::io::Read::read(&mut tls.sock, &mut [0u8; 1]);
            })
        };
        let base = tls_serve(vec![
            fin(b"HTTP/1.1 200 OK\r\nX: 1\nTransfer-Encoding: gzip\r\n\
                  Transfer-Encoding: chunked\r\n\r\npartial"),
            fin(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\n\nHTTP/1.1 200 OK\r\n\
                  Transfer-Encoding: chunked\r\n\r\npartial",
            ),
            fin(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\npartial"),
            fin(b"HTTP/1.1 200 OK\nTransfer-Encoding: chunked\n\n5\r\nwhole\r\n0\r\n"),
        ]);
        let agent = agent(Route::Direct, test_trust(), test_profile());
        let get = |at: u8| call(&agent, &format!("{base}/{at}"));
        for at in 1..=3 {
            let err = get(at).expect_err("a body its close delimits passed for whole");
            assert!(
                err.ends_with("(no close_notify): the answer may have been cut short"),
                "{at}: {err}"
            );
        }
        assert_eq!(get(4).as_deref(), Ok("whole"));
    }

    /// fixR11 #5: a head is read with room for as many headers as ureq's
    /// (`ureq-proto`'s `MAX_RESPONSE_HEADERS`, [`MAX_ANSWER_HEADERS`]), so
    /// the two readings cannot part on how many a head may have. With less
    /// room, a head ureq reads failed here, as one with too many headers —
    /// taken for a head that is not chunked — and a chunked body whose last
    /// chunk came failed as cut short when its TLS session ended. A head
    /// with more than ureq reads fails ureq, and is not chunked here either.
    #[test]
    fn a_head_of_as_many_headers_as_ureq_reads_is_framed_as_ureq_frames_it() {
        // A head of `count` headers, the last saying the body is chunked.
        let head = |count: usize| {
            let mut head = "HTTP/1.1 200 OK\r\n".to_string();
            for n in 1..count {
                head.push_str(&format!("X-{n}: {n}\r\n"));
            }
            head + "Transfer-Encoding: chunked\r\n\r\n"
        };
        assert_eq!(
            answer_head(head(128).as_bytes()),
            AnswerHead::Final { chunked: true }
        );
        assert_eq!(
            answer_head(head(129).as_bytes()),
            AnswerHead::Final { chunked: false }
        );

        // The chunked body whole, and the TLS session's end with no
        // close_notify: whole to ureq, and to clew.
        let answer = format!("{}5\r\nwhole\r\n0\r\n", head(128));
        let base = tls_serve(vec![Box::new(move |tls: &mut TlsConn| {
            let _ = tls.write_all(answer.as_bytes());
            let _ = tls.flush();
            let _ = tls.sock.shutdown(Shutdown::Write);
            // Held until the client lets go.
            let _ = tls.sock.set_read_timeout(Some(Duration::from_secs(20)));
            let _ = std::io::Read::read(&mut tls.sock, &mut [0u8; 1]);
        })]);
        let agent = agent(Route::Direct, test_trust(), test_profile());
        assert_eq!(
            call(&agent, &format!("{base}/many")).as_deref(),
            Ok("whole")
        );
    }

    /// fixR9 #8, fixR10 #2: how an answer's head frames its body, as far as
    /// a TLS session ending under it needs to know ([`Answered`]): chunked
    /// only where ureq reads it as chunked too — never where ureq would read
    /// the body to the connection's end — and interim answers passed over,
    /// whether ureq has taken them off the input yet or not.
    ///
    /// The head is delimited and read as ureq reads it, by httparse: a line
    /// ends at a bare LF as much as at a CRLF, and empty lines before the
    /// status line are passed over. Split on CRLF alone, a bare LF made the
    /// headers read otherwise than ureq read them — one `Transfer-Encoding`,
    /// `chunked`, where ureq's first was `gzip` — and a head that ended at
    /// `\n\n` read on into its body, which looked like a chunked answer's
    /// head. Where ureq took a head off the input that was not whole here,
    /// the two readings parted, and the answer is not chunked.
    #[test]
    fn an_answers_framing_is_read_as_ureq_reads_it_or_narrower() {
        // What has come after each read, the input at each given — ureq
        // having taken off it just the heads whole in it, interim ones.
        let after = |inputs: &[&str]| {
            inputs.iter().fold(Answered::Nothing, |answered, input| {
                let before = match answered {
                    Answered::Head { searched } => searched,
                    _ => 0,
                };
                answered.after(before, input.as_bytes())
            })
        };
        let chunked = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let (part, _) = chunked.split_at(chunked.len() - 2);
        assert_eq!(after(&[""]), Answered::Nothing);
        assert_eq!(
            after(&[part]),
            Answered::Head {
                searched: part.len()
            }
        );
        assert_eq!(after(&[part, chunked]), Answered::Body { chunked: true });
        let early = "HTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\n";
        assert_eq!(
            after(&[&format!("{early}{chunked}")]),
            Answered::Body { chunked: true }
        );
        assert_eq!(after(&[early, chunked]), Answered::Body { chunked: true });
        assert_eq!(
            after(&[chunked, "HTTP/1.1 200 OK\r\n\r\n"]),
            Answered::Body { chunked: true }
        );

        // Heads with a bare LF, as ureq reads them: the verifier's two, the
        // body after each read to the connection's end; one chunked; and
        // an interim answer's before one.
        for (input, framed) in [
            (
                "HTTP/1.1 200 OK\r\nX: 1\nTransfer-Encoding: gzip\r\nTransfer-Encoding: \
                 chunked\r\n\r\npartial",
                false,
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\n\nHTTP/1.1 200 OK\r\n\
                 Transfer-Encoding: chunked\r\n\r\npartial",
                false,
            ),
            (
                "HTTP/1.1 200 OK\nTransfer-Encoding: chunked\n\n5\r\nwhole\r\n",
                true,
            ),
            (
                "HTTP/1.1 103 Early Hints\nLink: </a>\n\nHTTP/1.1 200 OK\n\
                 Transfer-Encoding: chunked\n\n",
                true,
            ),
            (
                "\r\n\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
                true,
            ),
        ] {
            assert_eq!(
                after(&[input]),
                Answered::Body { chunked: framed },
                "{input:?}"
            );
        }
        // The same answer a byte at a time: looked at again only once a
        // line has ended, and found whole where it is.
        let bare = "HTTP/1.1 200 OK\nTransfer-Encoding: chunked\n\n5\r\n";
        let grown: Vec<&str> = (1..=bare.len()).map(|n| &bare[..n]).collect();
        assert_eq!(after(&grown), Answered::Body { chunked: true });
        // ureq took off the input what was no whole head here — what is
        // left reads as a chunked answer's head — so the two readings
        // parted, and a cut is an error.
        let parted = Answered::Head { searched: 10 }.after(4, chunked.as_bytes());
        assert_eq!(parted, Answered::Body { chunked: false });

        // Not HTTP as ureq reads it — ureq fails the answer: not chunked.
        for head in [
            "HTTP/1.1 200 OK\r\nTransfer-Encoding : chunked\r\n\r\n",
            "HTTP/2 200\r\nTransfer-Encoding: chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\n Transfer-Encoding: chunked\r\n\r\n",
        ] {
            assert_eq!(
                answer_head(head.as_bytes()),
                AnswerHead::Final { chunked: false },
                "{head:?}"
            );
        }
        assert_eq!(answer_head(part.as_bytes()), AnswerHead::Partial);
        for (head, chunked) in [
            (
                "HTTP/1.1 200 OK\r\ntransfer-encoding: gzip, chunked\r\n\r\n",
                true,
            ),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding:\tChunked \r\n\r\n",
                true,
            ),
            // ureq reads these as chunked; not taken for such here.
            (
                "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked, gzip\r\n\r\n",
                false,
            ),
            (
                "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\ntransfer-encoding: chunked\r\n\r\n",
                false,
            ),
            // ureq reads these to the connection's end.
            (
                "HTTP/1.0 200 OK\r\ntransfer-encoding: chunked\r\n\r\n",
                false,
            ),
            (
                "HTTP/1.1 200 OK\r\ntransfer-encoding: gzip\r\ntransfer-encoding: chunked\r\n\r\n",
                false,
            ),
            (
                "HTTP/1.1 200 OK\r\ntransfer-encoding: \u{e9}, chunked\r\n\r\n",
                false,
            ),
            ("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\n", false),
            ("HTTP/1.1 200 OK\r\n\r\n", false),
            ("HTTP/1.1 101 Switching Protocols\r\n\r\n", false),
        ] {
            assert_eq!(
                answer_head(head.as_bytes()),
                AnswerHead::Final { chunked },
                "{head:?}"
            );
        }
        assert_eq!(
            answer_head(early.as_bytes()),
            AnswerHead::Interim(early.len())
        );
    }

    /// A peer at the returned address that takes in what it is sent a
    /// little at a time: a receive buffer of 4 KiB, drained 2 KiB every
    /// quarter of a second — after a TLS handshake, when `tls`.
    fn slow_drain(tls: bool) -> SocketAddr {
        use socket2::{Domain, Socket, Type};
        let listener = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        listener.set_recv_buffer_size(4096).unwrap();
        listener
            .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
            .unwrap();
        listener.listen(1).unwrap();
        let addr = listener.local_addr().unwrap().as_socket().unwrap();
        std::thread::spawn(move || {
            let Ok((conn, _)) = listener.accept() else {
                return;
            };
            let mut conn: std::net::TcpStream = conn.into();
            let _ = conn.set_read_timeout(Some(Duration::from_secs(20)));
            if tls {
                let config = crate::testutil::test_tls_server_config();
                let Ok(mut server) = rustls::ServerConnection::new(config) else {
                    return;
                };
                while server.is_handshaking() {
                    if server.complete_io(&mut conn).is_err() {
                        return;
                    }
                }
            }
            let began = Instant::now();
            while began.elapsed() < Duration::from_secs(20) {
                match std::io::Read::read(&mut conn, &mut [0u8; 2048]) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => std::thread::sleep(Duration::from_millis(250)),
                }
            }
        });
        addr
    }

    /// A connection each of whose sends takes `each` — and fails once the
    /// timeout it is given is shorter. What it sent is kept.
    #[derive(Debug)]
    struct Slow {
        buffers: LazyBuffers,
        each: Duration,
        sent: Vec<u8>,
    }

    impl Transport for Slow {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }

        fn transmit_output(
            &mut self,
            amount: usize,
            timeout: NextTimeout,
        ) -> Result<(), ureq::Error> {
            if !timeout.after.is_not_happening() && *timeout.after < self.each {
                std::thread::sleep(*timeout.after);
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            std::thread::sleep(self.each);
            let sent = self.buffers.output()[..amount].to_vec();
            self.sent.extend(sent);
            Ok(())
        }

        fn await_input(&mut self, _: NextTimeout) -> Result<bool, ureq::Error> {
            Ok(false)
        }

        fn is_open(&mut self) -> bool {
            false
        }
    }

    /// fixR9 #6: what one call sends in pieces — a `CONNECT`, a request
    /// addressed to a proxy, a piece at a time as the connection's output
    /// buffer holds it — is bounded as one: each piece by what is left of
    /// the bound. Each piece was given the whole timeout afresh.
    #[test]
    fn a_send_in_pieces_keeps_one_bound() {
        let slow = |output: usize| Slow {
            buffers: LazyBuffers::new(1024, output),
            each: Duration::from_millis(30),
            sent: Vec::new(),
        };
        // Ten pieces of 30 ms, within 100 ms.
        let mut conn = slow(4);
        let due = Bound::connect(Instant::now() + Duration::from_millis(100));
        let result = send_all(&mut conn, &[b'x'; 40], due);
        assert!(matches!(result, Err(ureq::Error::Timeout(_))), "{result:?}");
        assert!(conn.sent.len() < 40, "all of it went");

        // A request to a proxy — its request line in absolute form, then
        // the rest — in pieces of 32 bytes: three of 30 ms, within 70 ms.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut exchange = Exchange {
            inner: Box::new(slow(32)),
            meter: Arc::new(Meter::new(1 << 20)),
            absolute: Some(Absolute {
                authority: "models.example".into(),
                authorization: None,
            }),
            tls: false,
            fresh: true,
            lease: Lease::new(socket),
            answered: Answered::Nothing,
            pace: None,
            pacing: None,
        };
        let request = b"GET /x HTTP/1.1\r\nHost: m\r\n\r\n";
        exchange.buffers().output()[..request.len()].copy_from_slice(request);
        let timeout = NextTimeout {
            after: Duration::from_millis(70).into(),
            reason: Timeout::SendRequest,
        };
        let result = exchange.transmit_output(request.len(), timeout);
        assert!(matches!(result, Err(ureq::Error::Timeout(_))), "{result:?}");
    }

    /// fixR9 #6: a request's overall deadline holds while it is being sent.
    /// Each write of a transmit got the whole of ureq's timeout afresh
    /// (`write_all` under one socket timeout), so a peer that took a large
    /// request in a little at a time held it for twice its deadline. Here
    /// over plain http, over TLS, and through an http proxy, side by side.
    #[test]
    fn a_request_drained_slowly_ends_at_its_deadline() {
        let profile = Profile {
            total: Some(Duration::from_secs(3)),
            ..test_profile()
        };
        let (plain, secure, proxy) = (slow_drain(false), slow_drain(true), slow_drain(false));
        let sends: Vec<_> = [
            (None, format!("http://{plain}/x")),
            (None, format!("https://{secure}/x")),
            (
                Some(format!("http://{proxy}")),
                "http://models.example/x".to_string(),
            ),
        ]
        .into_iter()
        .map(|(through, url)| {
            let agent = agent(route(through.as_deref()).unwrap(), test_trust(), profile);
            std::thread::spawn(move || {
                let body = vec![b'z'; 4 << 20];
                let began = Instant::now();
                let result = guarded(|| agent.post(&url).send(&body[..]));
                (url, result.err().map(|e| describe(&e)), began.elapsed())
            })
        })
        .collect();
        for send in sends {
            let (url, err, took) = send.join().unwrap();
            let err = err.unwrap_or_else(|| panic!("{url}: the request was taken whole"));
            assert!(err.contains("timed out"), "{url}: {err}");
            assert!(
                took < Duration::from_millis(4500),
                "{url}: the deadline held for {took:?}"
            );
        }
    }

    /// Send `plaintext` to the client as one TLS record, a byte of it every
    /// `every`: the peer that trickles a record.
    fn trickle(tls: &mut TlsConn, plaintext: &[u8], every: Duration) {
        let _ = tls.flush();
        if tls.conn.writer().write_all(plaintext).is_err() {
            return;
        }
        let mut record = Vec::new();
        while tls.conn.wants_write() {
            if tls.conn.write_tls(&mut record).is_err() {
                return;
            }
        }
        for byte in record {
            if tls.sock.write_all(&[byte]).is_err() {
                return;
            }
            std::thread::sleep(every);
        }
    }

    /// fixR7 #8: every read of one TLS record shares one bound. rustls reads
    /// a record in as many reads below it as its peer takes to send it, and
    /// each of those used to wait the whole timeout afresh, as ureq's own
    /// `TransportAdapter` does with a handshake's: a peer that trickled a
    /// record a byte at a time held a request with a two-second deadline for
    /// eight seconds. Here the bound is a second, the trickle five: an
    /// overall deadline, and — through an `https://` proxy trickling its
    /// answer to the `CONNECT` — the connect's.
    #[test]
    fn a_trickled_tls_record_cannot_stretch_a_deadline() {
        let body = [b'x'; 32];
        let base = tls_serve(vec![Box::new(move |tls: &mut TlsConn| {
            let _ = tls.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 32\r\n\r\n");
            let _ = tls.flush();
            trickle(tls, &body, Duration::from_millis(100));
        })]);
        let profile = Profile {
            total: Some(Duration::from_secs(1)),
            ..test_profile()
        };
        let agent = agent(Route::Direct, test_trust(), profile);
        let began = Instant::now();
        let result = call(&agent, &format!("{base}/asset"));
        let took = began.elapsed();
        let err = result.expect_err("a trickled record outlasted the deadline");
        assert!(err.contains("timed out"), "{err}");
        assert!(
            took < Duration::from_secs(3),
            "the deadline held for {took:?}"
        );

        // So with a pace ([`Pace`]): the trickle makes up no window, and the
        // request fails at the end of the first past its grace — a second
        // in, the grace and the window half a second each.
        let base = tls_serve(vec![Box::new(move |tls: &mut TlsConn| {
            let _ = tls.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 32\r\n\r\n");
            let _ = tls.flush();
            trickle(tls, &body, Duration::from_millis(100));
        })]);
        let profile = Profile {
            pace: Some(Pace {
                grace: Duration::from_millis(500),
                window: Duration::from_millis(500),
                floor: 4096,
            }),
            ..test_profile()
        };
        let paced = super::agent(Route::Direct, test_trust(), profile);
        let began = Instant::now();
        let result = call(&paced, &format!("{base}/asset"));
        let took = began.elapsed();
        let err = result.expect_err("a trickled record outlasted the pace");
        assert!(err.contains("slower than 4 KiB/s"), "{err}");
        assert!(took < Duration::from_secs(3), "the pace held for {took:?}");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_url = format!("https://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let Ok((tcp, _)) = listener.accept() else {
                return;
            };
            let _ = tcp.set_read_timeout(Some(Duration::from_secs(20)));
            let config = crate::testutil::test_tls_server_config();
            let Ok(server) = rustls::ServerConnection::new(config) else {
                return;
            };
            let mut tls = rustls::StreamOwned::new(server, tcp);
            crate::testutil::read_http(&mut tls);
            trickle(
                &mut tls,
                b"HTTP/1.1 200 Connection established\r\n\r\n",
                Duration::from_millis(100),
            );
        });
        let profile = Profile {
            connect: Duration::from_secs(1),
            ..test_profile()
        };
        let proxied = super::agent(route(Some(&proxy_url)).unwrap(), test_trust(), profile);
        let began = Instant::now();
        let result = call(&proxied, "https://releases.invalid/asset");
        let took = began.elapsed();
        let err = result.expect_err("a trickled CONNECT answer outlasted the connect's deadline");
        assert!(err.contains("timed out"), "{err}");
        assert!(
            took < Duration::from_secs(3),
            "the deadline held for {took:?}"
        );
    }

    /// fixR10 #7: a pace is kept window by window ([`Pacing::due`]), and
    /// nothing is banked: what a window brings past its due counts for none
    /// after it. Here on a clock of its own, to the instant.
    ///
    /// fixR11 #3: a window is the pace's own, not the grace — here half as
    /// long — and fixR11 #5: what one must bring is counted to the byte,
    /// however short the window.
    #[test]
    fn a_pace_is_kept_window_by_window() {
        let pace = Pace {
            grace: Duration::from_secs(1),
            window: Duration::from_millis(500),
            floor: 4096,
        };
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut pacing = Pacing::new(t0);
        // While the grace lasts, the first window's end is the due: a
        // window past the grace, not a second grace.
        assert_eq!(pacing.due(pace, 0, at(100)), at(1500));
        // 40 KiB before the grace ends, then a trickle: the first window
        // begins as the grace ends, the burst in none of it.
        assert_eq!(pacing.due(pace, 40 * 1024, at(900)), at(1500));
        assert_eq!(pacing.due(pace, 40 * 1024 + 5, at(1100)), at(1500));
        assert_eq!(pacing.due(pace, 40 * 1024 + 18, at(1400)), at(1500));
        // A window's due in — 2 KiB of it — begins the next, from then.
        assert_eq!(pacing.due(pace, 42 * 1024 + 5, at(1450)), at(1950));
        // What came past the due earns that window nothing: another 2 KiB,
        // counted from the look that began it.
        assert_eq!(pacing.due(pace, 82 * 1024, at(1700)), at(2200));
        assert_eq!(pacing.due(pace, 82 * 1024 + 2047, at(2100)), at(2200));
        // A request of its own starts afresh.
        assert_eq!(Pacing::new(at(5000)).due(pace, 0, at(5100)), at(6500));

        // A window of 100.25 ms at 4 KiB a second, past a grace of a
        // quarter of a second, must bring 410.624 bytes: 410 do not make it
        // up, 411 do. Counted in whole seconds, the due was none; in whole
        // milliseconds, 409 or 410 bytes.
        let short = Pace {
            grace: Duration::from_millis(250),
            window: Duration::from_micros(100_250),
            floor: 4096,
        };
        let first = at(250) + short.window;
        let mut pacing = Pacing::new(t0);
        assert_eq!(pacing.due(short, 0, at(260)), first);
        assert_eq!(pacing.due(short, 410, at(300)), first);
        assert_eq!(pacing.due(short, 411, at(310)), at(310) + short.window);
    }

    /// fixR10 #7: a burst buys no trickle. The pace was a deadline that grew
    /// by a second for every `floor` bytes, counted from the first: a peer
    /// that sent 40 KiB at once, then a byte every 50 ms, held a request
    /// with a second's grace at 4 KiB a second for eleven seconds — and all
    /// one answer may deliver, for hours. Now the burst makes up no window
    /// past the grace, and the request fails at the end of the first. An
    /// answer that keeps coming faster than the floor, if slowly, completes,
    /// however many windows it takes.
    ///
    /// fixR11 #3: a trickle from the start is cut a window past the grace —
    /// here 2.5 s in, the grace two seconds and the window half of one. The
    /// window was the grace, and a trickle was cut at twice it: an
    /// embeddings batch's six minutes in, where it is now four.
    #[test]
    fn a_burst_buys_no_trickle_under_a_pace() {
        // An endpoint answering `burst` bytes at once, then `piece` every
        // `every`, to a body of `total` (or for twenty seconds).
        let serving = |burst: usize, piece: usize, every: Duration, total: usize| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            std::thread::spawn(move || {
                let Ok((mut conn, _)) = listener.accept() else {
                    return;
                };
                crate::testutil::read_http(&mut conn);
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {total}\r\n\r\n");
                if conn.write_all(head.as_bytes()).is_err()
                    || conn.write_all(&vec![b'x'; burst]).is_err()
                {
                    return;
                }
                let (began, mut sent) = (Instant::now(), burst);
                while sent < total && began.elapsed() < Duration::from_secs(20) {
                    std::thread::sleep(every);
                    if conn.write_all(&vec![b'x'; piece]).is_err() {
                        return;
                    }
                    sent += piece;
                }
            });
            format!("{base}/x")
        };
        let profile = Profile {
            pace: Some(Pace {
                grace: Duration::from_secs(2),
                window: Duration::from_millis(500),
                floor: 4096,
            }),
            ..test_profile()
        };
        let agent = agent(Route::Direct, test_trust(), profile);
        // Each request on a thread of its own, all at once: its result, and
        // how long it took.
        let timed = |url: String| {
            let agent = agent.clone();
            std::thread::spawn(move || {
                let began = Instant::now();
                (call(&agent, &url), began.elapsed())
            })
        };
        let ms = Duration::from_millis;

        // A byte every 50 ms, from the start and after a burst of 40 KiB.
        let trickled = timed(serving(0, 1, ms(50), 1 << 20));
        let burst = timed(serving(40 * 1024, 1, ms(50), 1 << 20));
        // Steady: 2 KiB every 50 ms, ten times the floor, for three seconds
        // — past the grace and two windows.
        let steady = timed(serving(0, 2048, ms(50), 120 * 1024));

        for (what, request) in [("a trickle", trickled), ("a burst, then a trickle", burst)] {
            let (result, took) = request.join().unwrap();
            let err = result.expect_err("a trickle outlasted the pace");
            assert!(
                err.contains("slower than 4 KiB/s over 0.5 s, once the first 2 s were up"),
                "{what}: {err}"
            );
            assert!(
                took > ms(2400) && took < ms(3400),
                "{what}: failed after {took:?}"
            );
        }
        let (result, took) = steady.join().unwrap();
        let body = result.expect("a steady answer was given up");
        assert_eq!(body.len(), 120 * 1024);
        assert!(took > ms(2500), "{took:?}");
    }

    /// fixR10 #8: each request on a kept connection keeps its own pace, from
    /// its own first byte out. One that took the connection up past the
    /// first one's grace and first window was held to the first one's clock,
    /// and failed as too slow before its answer could come.
    #[test]
    fn a_kept_connections_next_request_keeps_its_own_pace() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (served_tx, served) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            let mut answered = 0;
            for _ in 0..2 {
                if crate::testutil::read_http(&mut conn).head.is_empty() {
                    break;
                }
                let _ = conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
                answered += 1;
            }
            let _ = served_tx.send(answered);
        });
        let profile = Profile {
            pooled: true,
            pace: Some(Pace {
                grace: Duration::from_millis(300),
                window: Duration::from_millis(300),
                floor: 4096,
            }),
            ..test_profile()
        };
        let agent = agent(Route::Direct, test_trust(), profile);
        assert_eq!(call(&agent, &format!("{base}/1")).as_deref(), Ok("ok"));
        // Past the first request's grace and its first window.
        std::thread::sleep(Duration::from_millis(900));
        assert_eq!(call(&agent, &format!("{base}/2")).as_deref(), Ok("ok"));
        assert_eq!(
            served.recv_timeout(Duration::from_secs(10)),
            Ok(2),
            "both answers came on one connection"
        );
    }

    /// fixR10 #8: each answer on a kept connection is judged by its own
    /// framing when its TLS session ends under it ([`Answered`]). The
    /// second answer on one was judged by the first's: after a chunked
    /// answer, a body ureq reads to the connection's end passed for a
    /// chunked one, and its cut for all of it; after one of a declared
    /// length, a chunked body whose last chunk came failed.
    #[test]
    fn a_kept_tls_connections_next_answer_is_judged_by_its_own_framing() {
        // `first`, then — to the next request on the connection — `second`,
        // and the connection's end with no close_notify.
        let twice = |first: &'static [u8], second: &'static [u8]| -> Script {
            Box::new(move |tls: &mut TlsConn| {
                let _ = tls.write_all(first);
                let _ = tls.flush();
                let _ = tls.sock.set_read_timeout(Some(Duration::from_secs(20)));
                if crate::testutil::read_http(tls).head.is_empty() {
                    return;
                }
                let _ = tls.write_all(second);
                let _ = tls.flush();
                let _ = tls.sock.shutdown(Shutdown::Write);
                // Held until the client lets go.
                let _ = std::io::Read::read(&mut tls.sock, &mut [0u8; 1]);
            })
        };
        let base = tls_serve(vec![
            twice(
                b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nfirst\r\n0\r\n\r\n",
                b"HTTP/1.1 200 OK\r\n\r\npartial",
            ),
            twice(
                b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nfirst",
                b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nwhole\r\n0\r\n",
            ),
        ]);
        let profile = Profile {
            pooled: true,
            ..test_profile()
        };
        // An agent of its own for each connection, which it keeps.
        let kept = agent(Route::Direct, test_trust(), profile);
        assert_eq!(call(&kept, &format!("{base}/1")).as_deref(), Ok("first"));
        let err = call(&kept, &format!("{base}/2")).expect_err("a cut body passed for whole");
        assert!(
            err.ends_with("(no close_notify): the answer may have been cut short"),
            "{err}"
        );
        let kept = agent(Route::Direct, test_trust(), profile);
        assert_eq!(call(&kept, &format!("{base}/3")).as_deref(), Ok("first"));
        assert_eq!(call(&kept, &format!("{base}/4")).as_deref(), Ok("whole"));
    }

    /// A TLS server for [`a_kept_tls_connection_holds_nothing_past_its_answer`].
    /// Its first connection answers the first request `first` — and then,
    /// unasked, `SMUGG`, as a record of its own: the bytes of both records up
    /// to `cut` at once, the rest only once a second request comes on the
    /// connection. Any other connection answers `fresh`.
    fn smuggler(first: String, cut: Option<usize>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("https://{}", listener.local_addr().unwrap());
        let mut config = (*crate::testutil::test_tls_server_config()).clone();
        // No session tickets: nothing but the answers follows the handshake,
        // so their records land where this server puts them.
        config.send_tls13_tickets = 0;
        let config = Arc::new(config);
        std::thread::spawn(move || {
            for at in 0..2 {
                let Ok((tcp, _)) = listener.accept() else {
                    return;
                };
                let _ = tcp.set_read_timeout(Some(Duration::from_secs(20)));
                let (config, first) = (config.clone(), first.clone());
                std::thread::spawn(move || {
                    let Ok(server) = rustls::ServerConnection::new(config) else {
                        return;
                    };
                    let mut tls = rustls::StreamOwned::new(server, tcp);
                    crate::testutil::read_http(&mut tls);
                    if at > 0 {
                        let _ = tls.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nfresh");
                        let _ = tls.flush();
                        let _ = std::io::Read::read(&mut tls, &mut [0u8; 1]);
                        return;
                    }
                    let mut records = Vec::new();
                    for answer in [
                        first.as_bytes(),
                        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nSMUGG",
                    ] {
                        let _ = tls.conn.writer().write_all(answer);
                        while tls.conn.wants_write() {
                            let _ = tls.conn.write_tls(&mut records);
                        }
                    }
                    let (now, later) = records.split_at(cut.unwrap_or(records.len()));
                    let _ = tls.sock.write_all(now);
                    // A second request on this connection: the rest.
                    if !crate::testutil::read_http(&mut tls).head.is_empty() {
                        let _ = tls.sock.write_all(later);
                        let _ =
                            tls.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 6\r\n\r\nreused");
                        let _ = tls.flush();
                        let _ = std::io::Read::read(&mut tls, &mut [0u8; 1]);
                    }
                });
            }
        });
        base
    }

    /// fixR7 #9: a kept TLS connection is handed back only with nothing the
    /// peer sent past its answer waiting anywhere between the socket and
    /// ureq. Two places no check saw: bytes read off the socket but not yet
    /// handed to rustls (rustls reads 4096 bytes at a time: after a record of
    /// exactly that, the next one waits below it), and the start of a record
    /// inside rustls whose rest has yet to come. Either was the start of an
    /// answer nothing had asked for, which the next request on the
    /// connection took for its own: here, `SMUGG`.
    #[test]
    fn a_kept_tls_connection_holds_nothing_past_its_answer() {
        let profile = Profile {
            pooled: true,
            ..test_profile()
        };
        // A first answer whose record is 4096 bytes on the wire: its text,
        // and 22 bytes of TLS 1.3's (a header, the content type, the tag).
        let head = "HTTP/1.1 200 OK\r\ncontent-length: 4033\r\n\r\n";
        let aligned = format!("{head}{}", "a".repeat(4096 - 22 - head.len()));
        assert_eq!(aligned.len() + 22, 4096);
        let short = "HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nfirst".to_string();
        // Short: its record, and three bytes of the next one's header.
        let short_record = short.len() + 22;
        for (what, first, cut) in [
            ("a record waiting below rustls", aligned, None),
            (
                "a record begun inside rustls",
                short,
                Some(short_record + 3),
            ),
        ] {
            let base = smuggler(first.clone(), cut);
            let agent = agent(Route::Direct, test_trust(), profile);
            let body =
                call(&agent, &format!("{base}/one")).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(
                body.len() + first.find("\r\n\r\n").unwrap() + 4,
                first.len()
            );
            assert_eq!(
                call(&agent, &format!("{base}/two")).as_deref(),
                Ok("fresh"),
                "{what}: the second request took what the first answer's connection held"
            );
        }
    }

    /// A connection whose far side is scripted: each wait for input brings
    /// the next of `chunks` — and none left, the peer's end: it closed, or
    /// (`end`) the wait fails so. What is sent is dropped.
    #[derive(Debug)]
    struct Scripted {
        buffers: LazyBuffers,
        chunks: std::collections::VecDeque<Vec<u8>>,
        end: Option<ureq::Error>,
    }

    impl Transport for Scripted {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }

        fn transmit_output(&mut self, _: usize, _: NextTimeout) -> Result<(), ureq::Error> {
            Ok(())
        }

        fn await_input(&mut self, _: NextTimeout) -> Result<bool, ureq::Error> {
            let Some(chunk) = self.chunks.pop_front() else {
                return match self.end.take() {
                    Some(e) => Err(e),
                    None => Ok(false),
                };
            };
            self.buffers.input_append_buf()[..chunk.len()].copy_from_slice(&chunk);
            self.buffers.input_appended(chunk.len());
            Ok(true)
        }

        fn is_open(&mut self) -> bool {
            false
        }
    }

    /// fixR7 #12: a proxy's answer to a `CONNECT` is held to 64 KiB, the
    /// blank line that ends its head included. The bound used to apply only
    /// while that line had not come: a head of 64 KiB and a bit more that
    /// ended within one read was taken for an answer. And each read of the
    /// answer is searched once — with the three bytes before it, in which
    /// the blank line may have begun — not the whole answer again at every
    /// read, which a proxy trickling its answer made quadratic.
    #[test]
    fn a_connect_answer_is_bounded_and_searched_once() {
        let ProxySetting::Http(proxy) = proxy("http://proxy.corp:3128").unwrap() else {
            panic!("an http proxy");
        };
        let target = Target {
            https: true,
            host: "api.example.com".into(),
            port: 443,
        };
        let answer = |len: usize| {
            let start = b"HTTP/1.1 200 Connection established\r\nX-Pad: ".to_vec();
            let pad = len - start.len() - 4;
            [start, vec![b'a'; pad], b"\r\n\r\n".to_vec()].concat()
        };
        let open = |chunks: Vec<Vec<u8>>| {
            let conn = Scripted {
                buffers: LazyBuffers::new(128 * 1024, 16 * 1024),
                chunks: chunks.into(),
                end: None,
            };
            tunnel(
                Box::new(conn),
                &proxy,
                &target,
                Instant::now() + Duration::from_secs(20),
                None,
            )
        };
        // At the bound, in one read or in two: a tunnel.
        let whole = answer(MAX_PROXY_ANSWER);
        assert!(open(vec![whole.clone()]).is_ok());
        let (early, late) = whole.split_at(60 * 1024);
        assert!(open(vec![early.to_vec(), late.to_vec()]).is_ok());
        // A byte past it, however it comes: refused.
        let over = answer(MAX_PROXY_ANSWER + 1);
        let (early, late) = over.split_at(60 * 1024);
        for chunks in [vec![over.clone()], vec![early.to_vec(), late.to_vec()]] {
            let Err(err) = open(chunks) else {
                panic!("an answer over the bound was taken");
            };
            let err = describe(&err);
            assert!(err.contains("over 64 KiB of headers"), "{err}");
        }
        // A byte at a time: each searched once, and a blank line split
        // across reads still found.
        let began = Instant::now();
        assert!(open(whole.iter().map(|&b| vec![b]).collect()).is_ok());
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(head_end(b"HTTP/1.1 200 OK\r\n\r\n", 0), Some(19));
        assert_eq!(head_end(b"ab\r\n", 0), None);
        assert_eq!(head_end(b"ab\r\n\r\nrest", 4), Some(6));
        assert_eq!(head_end(b"ab\r\n\r\nrest", 5), Some(6));
        // What was searched is not searched again.
        assert_eq!(head_end(b"\r\n\r\nXYZW", 8), None);
    }

    /// fixR9 #1, #3, #5: what a proxy's answer to a `CONNECT` says decides
    /// whether another attempt may get past it.
    ///
    /// - A status that says it could not open the tunnel NOW — the target
    ///   unreachable from it, it too busy — may be got past, as a direct
    ///   connect's failure is; fixR7 made every one a refusal, never resent.
    /// - A `407` is about credentials — or, when its challenges offer only
    ///   logins clew cannot make, about those: correcting a password that
    ///   was never the matter was the advice. A comma inside a quoted
    ///   string separates no challenges (fixR10 #4).
    /// - The connection closed or reset before anything of an answer came,
    ///   to a `CONNECT` that carried a user name and password, may be a
    ///   refusal of them, and counts as one; resent, it went to the proxy
    ///   three times more. fixR10 #1: nothing else that is no answer does —
    ///   not a stall to the deadline, which a slow target behind the proxy
    ///   makes, whether ureq's own deadline (`ureq::Error::Timeout`) or a
    ///   socket's timeout ends it; nor an answer broken off once it began,
    ///   unless it began as a 407. It counted all of them, a slow target
    ///   told as a password to correct and never tried again.
    /// - fixR11 #1: every way of closing counts — an `https://` proxy's TLS
    ///   session that just ends among them. fixR11 #5: a 407 broken off
    ///   offers the logins of its lines that came whole.
    #[test]
    fn a_connect_answer_says_whether_another_attempt_may_pass() {
        use ProxyFault::{
            Credentials, LoginScheme, LoginUnanswered, Refused, Unanswered, Unavailable,
        };
        let target = Target {
            https: true,
            host: "api.example.com".into(),
            port: 443,
        };
        // The proxy `spec` answering `chunks`, then ending as `end` says:
        // how the tunnel failed.
        let failed = |spec: &str, chunks: Vec<Vec<u8>>, end: Option<ureq::Error>| {
            let ProxySetting::Http(proxy) = proxy(spec).unwrap() else {
                panic!("an http proxy");
            };
            let conn = Scripted {
                buffers: LazyBuffers::new(128 * 1024, 16 * 1024),
                chunks: chunks.into(),
                end,
            };
            let deadline = Instant::now() + Duration::from_secs(20);
            tunnel(Box::new(conn), &proxy, &target, deadline, None)
                .expect_err("the answer opened a tunnel")
        };
        // The same: how the proxy failed, and in what words.
        let open = |spec: &str, chunks: Vec<Vec<u8>>, end: Option<ureq::Error>| {
            let err = failed(spec, chunks, end);
            (proxy_failure(&err), describe(&err))
        };
        let head = |lines: &[&str]| vec![format!("{}\r\n\r\n", lines.join("\r\n")).into_bytes()];
        let (plain, named) = ("http://proxy.corp:3128", "http://me:pw@proxy.corp:3128");

        for (status, fault) in [
            ("408 Request Timeout", Unavailable),
            ("429 Too Many Requests", Unavailable),
            ("500 Internal Server Error", Unavailable),
            ("502 Bad Gateway", Unavailable),
            ("503 Service Unavailable", Unavailable),
            ("504 Gateway Timeout", Unavailable),
            ("400 Bad Request", Refused),
            ("403 Forbidden", Refused),
            ("302 Found", Refused),
            ("501 Not Implemented", Refused),
            ("505 HTTP Version Not Supported", Refused),
            ("511 Network Authentication Required", Refused),
        ] {
            let status_line = format!("HTTP/1.1 {status}");
            let (got, why) = open(named, head(&[&status_line]), None);
            assert_eq!(got, Some(fault), "{status}: {why}");
        }
        let (_, why) = open(plain, head(&["HTTP/1.1 502 Bad Gateway"]), None);
        assert_eq!(
            why,
            "the proxy proxy.corp:3128 could not reach api.example.com:443: it answered the \
             CONNECT with HTTP/1.1 502 Bad Gateway"
        );
        let (_, why) = open(plain, head(&["HTTP/1.1 403 Forbidden"]), None);
        assert_eq!(
            why,
            "the proxy proxy.corp:3128 refused the CONNECT to api.example.com:443: HTTP/1.1 403 \
             Forbidden"
        );

        // A 407, by the logins it offers: Basic among them (a comma in a
        // quoted realm included), none named (Basic is tried), or none clew
        // can make.
        let refused = "HTTP/1.1 407 Proxy Authentication Required";
        for (challenges, spec, fault, what) in [
            (
                &["Proxy-Authenticate: Basic realm=\"corp\""][..],
                plain,
                Credentials,
                "it asks for a user name and password",
            ),
            (
                &["Proxy-Authenticate: Negotiate, NTLM, Basic realm=\"a, b\""],
                named,
                Credentials,
                "it refused the user name and password",
            ),
            (
                &[],
                named,
                Credentials,
                "it refused the user name and password",
            ),
            (
                &["Proxy-Authenticate: NTLM", "proxy-authenticate: Negotiate"],
                named,
                LoginScheme,
                "it takes only NTLM or Negotiate logins, which clew cannot make",
            ),
            (
                &["Proxy-Authenticate: Digest realm=\"corp, int\", qop = \"auth\", nonce=\"x\""],
                plain,
                LoginScheme,
                "it takes only Digest logins, which clew cannot make",
            ),
            // A scheme inside a quoted string is none: Basic is not offered.
            (
                &["Proxy-Authenticate: NTLM realm=\"a, Basic b\""],
                named,
                LoginScheme,
                "it takes only NTLM logins, which clew cannot make",
            ),
        ] {
            let lines: Vec<&str> = [refused].iter().chain(challenges).copied().collect();
            let (got, why) = open(spec, head(&lines), None);
            assert_eq!(got, Some(fault), "{challenges:?}: {why}");
            assert_eq!(
                why,
                format!(
                    "the proxy proxy.corp:3128 answered the CONNECT to api.example.com:443 with \
                     {refused}: {what}"
                ),
                "{challenges:?}"
            );
        }

        // No answer: closed or reset before anything came — with a login
        // sent, perhaps a refusal of it; without one, the connection's
        // failure.
        let io_end = |kind: io::ErrorKind| Some(ureq::Error::from(io::Error::from(kind)));
        let closed = "the proxy proxy.corp:3128 gave no answer to the CONNECT to \
                      api.example.com:443, which carried the user name and password of its \
                      setting (the connection closed): it may have refused them";
        let plain_closed = "the proxy proxy.corp:3128 closed the connection instead of \
                            answering the CONNECT";
        assert_eq!(
            open(named, vec![], None),
            (Some(LoginUnanswered), closed.into())
        );
        assert_eq!(
            open(plain, vec![], None),
            (Some(Unanswered), plain_closed.into())
        );
        // fixR11 #1: so however else the connection ends under the wait —
        // reset, aborted, ended early, or (an `https://` proxy's) its TLS
        // session just ending, which is the connection closing. That last
        // was taken for no hang-up at all: a login the proxy may have
        // refused went to it three times more.
        let tls_end = || io::Error::new(io::ErrorKind::InvalidData, CutShort::Session);
        let hang_ups = || {
            [
                ("reset", io::ErrorKind::ConnectionReset.into()),
                ("aborted", io::ErrorKind::ConnectionAborted.into()),
                ("ended early", io::ErrorKind::UnexpectedEof.into()),
                ("its TLS session ended", tls_end()),
            ]
        };
        for (how, end) in hang_ups() {
            let (got, why) = open(named, vec![], Some(end.into()));
            assert_eq!(got, Some(LoginUnanswered), "{how}: {why}");
            assert!(why.contains("it may have refused them"), "{how}: {why}");
        }
        for (how, end) in hang_ups() {
            let (got, why) = open(plain, vec![], Some(end.into()));
            assert_eq!(got, Some(Unanswered), "{how}: {why}");
            assert!(!why.contains("refused"), "{how}: {why}");
        }
        assert_eq!(
            open(named, vec![], Some(tls_end().into())),
            (Some(LoginUnanswered), closed.into())
        );
        assert_eq!(
            open(plain, vec![], Some(tls_end().into())),
            (Some(Unanswered), plain_closed.into())
        );

        // A stall to the deadline, login or not — the request's own
        // deadline running out, as ureq says it, or a socket's timeout.
        for spec in [named, plain] {
            let stalls = [
                Some(ureq::Error::Timeout(Timeout::Connect)),
                io_end(io::ErrorKind::TimedOut),
                io_end(io::ErrorKind::WouldBlock),
            ];
            for end in stalls {
                let what = format!("{end:?}");
                let (got, why) = open(spec, vec![], end);
                assert_eq!(got, Some(Unanswered), "{spec} {what}: {why}");
                assert!(!why.contains("refused"), "{spec} {what}: {why}");
            }
        }
        let (_, why) = open(named, vec![], Some(ureq::Error::Timeout(Timeout::Connect)));
        assert_eq!(
            why,
            "the proxy proxy.corp:3128 failed the CONNECT to api.example.com:443: timed out \
             connecting"
        );

        // An answer broken off once it began, however: not a refusal —
        // unless it began as a 407, which said as much.
        let (got, why) = open(named, vec![b"HTTP/1.1 200 Conn".to_vec()], None);
        assert_eq!(
            (got, why.as_str()),
            (
                Some(Unanswered),
                "the proxy proxy.corp:3128 broke off its answer to the CONNECT to \
                 api.example.com:443 (the connection closed)"
            )
        );
        for end in [
            io_end(io::ErrorKind::ConnectionReset),
            Some(ureq::Error::Timeout(Timeout::Connect)),
        ] {
            let (got, why) = open(named, vec![b"HTTP/1.1 503 Serv".to_vec()], end);
            assert_eq!(got, Some(Unanswered), "{why}");
        }
        let began = || vec![b"HTTP/1.1 407 Proxy Auth".to_vec()];
        let (got, why) = open(named, began(), None);
        assert_eq!(
            (got, why.as_str()),
            (
                Some(Credentials),
                "the proxy proxy.corp:3128 began to answer the CONNECT to api.example.com:443 \
                 with a 407 and broke off (the connection closed): it refused the user name \
                 and password"
            )
        );
        let (got, why) = open(plain, began(), Some(ureq::Error::Timeout(Timeout::Connect)));
        assert_eq!(got, Some(Credentials), "{why}");
        assert!(
            why.ends_with("(timed out connecting): it asks for a user name and password"),
            "{why}"
        );

        // fixR11 #5: which logins a 407 broken off offers, the lines of it
        // that came whole say — up to a blank line, which ends its head
        // however the proxy ends its lines. They were not read at all: a
        // proxy that offered only NTLM before it broke off was said to have
        // refused the password, which there was no correcting. A line cut
        // short counts for nothing, as it may end in a scheme's name cut
        // short: `Basi` is no login clew cannot make, but the start of the
        // one it can.
        for (rest, fault, what) in [
            (
                "\r\nProxy-Authenticate: NTLM\r\n",
                LoginScheme,
                "it takes only NTLM logins, which clew cannot make",
            ),
            (
                "\r\nProxy-Authenticate: NTLM\r\nproxy-authenticate: Negotiate\r\nVia: 1.1 p",
                LoginScheme,
                "it takes only NTLM or Negotiate logins, which clew cannot make",
            ),
            (
                "\nProxy-Authenticate: NTLM\n\nProxy-Authenticate: Basic\n",
                LoginScheme,
                "it takes only NTLM logins, which clew cannot make",
            ),
            (
                "\r\nProxy-Authenticate: Basic realm=\"corp\"\r\nProxy-Authenticate: NTL",
                Credentials,
                "it refused the user name and password",
            ),
            (
                "\r\nProxy-Authenticate: NTLM, Basi",
                Credentials,
                "it refused the user name and password",
            ),
        ] {
            let answer = format!("{refused}{rest}").into_bytes();
            let (got, why) = open(named, vec![answer], None);
            assert_eq!(
                (got, why),
                (
                    Some(fault),
                    format!(
                        "the proxy proxy.corp:3128 began to answer the CONNECT to \
                         api.example.com:443 with a 407 and broke off (the connection closed): \
                         {what}"
                    )
                ),
                "{rest:?}"
            );
        }
        // And what can make the login is what the user is told of.
        let via = Via {
            spec: named.into(),
            system: None,
            for_https: true,
        };
        let answer = format!("{refused}\r\nProxy-Authenticate: NTLM\r\n").into_bytes();
        assert_eq!(
            failure(&failed(named, vec![answer], None), Some(&via)),
            format!(
                "the proxy proxy.corp:3128 began to answer the CONNECT to api.example.com:443 \
                 with a 407 and broke off (the connection closed): it takes only NTLM logins, \
                 which clew cannot make — name a local relay that makes such logins for clew, \
                 such as cntlm or px, as the proxy instead, or {ENV_BYPASS}"
            )
        );
    }

    /// fixR10 #1: an http proxy answers a `CONNECT` only once it has
    /// connected to the target, so a slow or unreachable target stalls its
    /// answer — here past clew's connect timeout: the proxy holds the
    /// `CONNECT` for three seconds, then answers 504, and clew waits one.
    /// With a user name and password in the `CONNECT`, that stall counted as
    /// a refusal of them: never tried again, and the user told to correct a
    /// password that was right. It is the connection's failure, as it is
    /// without them.
    #[test]
    fn a_connect_held_past_the_timeout_is_no_refusal_of_its_login() {
        use crate::testutil::{ProxyAnswer, http_proxy};
        for login in ["me:pw@", ""] {
            let (proxy_url, _) = http_proxy(
                "127.0.0.1:0",
                false,
                1,
                ProxyAnswer::Late(504, Duration::from_secs(3)),
            )
            .unwrap();
            let addr = proxy_url.trim_start_matches("http://").to_string();
            let spec = proxy_url.replacen("://", &format!("://{login}"), 1);
            let profile = Profile {
                connect: Duration::from_secs(1),
                ..test_profile()
            };
            let agent = agent(route(Some(&spec)).unwrap(), test_trust(), profile);
            let began = Instant::now();
            let err = guarded(|| agent.get("https://api.example.invalid/v1").call())
                .expect_err("a held CONNECT opened a tunnel");
            assert!(began.elapsed() < Duration::from_secs(3), "{login:?}");
            let via = Via {
                spec: spec.clone(),
                system: None,
                for_https: true,
            };
            assert_eq!(
                proxy_failure(&err),
                Some(ProxyFault::Unanswered),
                "{login:?}"
            );
            assert!(!proxy_refused(&err), "{login:?}");
            assert_eq!(
                failure(&err, Some(&via)),
                format!(
                    "the proxy {addr} failed the CONNECT to api.example.invalid:443: timed out \
                     connecting — to reach the host without the proxy, {ENV_BYPASS}"
                ),
                "{login:?}"
            );
        }
    }

    /// fixR10 #6: a `407`'s challenges are read to a bound — the first
    /// sixteen schemes, each once, however long the challenges run. A
    /// 64 KiB challenge of distinct names collected thousands, each
    /// compared with every one before it. And fixR10 #4: a comma inside a
    /// quoted string separates no challenges.
    #[test]
    fn a_407s_login_schemes_are_read_to_a_bound() {
        // Distinct three-character names, a comma after each, to 64 KiB.
        let letters: Vec<char> = ('a'..='z').chain('A'..='Z').chain('0'..='9').collect();
        let mut names = Vec::new();
        'fill: for a in &letters {
            for b in &letters {
                for c in &letters {
                    if names.len() * 4 >= 64 * 1024 {
                        break 'fill;
                    }
                    names.push(format!("{a}{b}{c}"));
                }
            }
        }
        let hostile = names.join(",");
        assert!(hostile.len() >= 64 * 1024 - 4, "{}", hostile.len());
        let schemes = login_schemes([hostile.as_str()]);
        assert_eq!(schemes, names[..MAX_SCHEMES]);
        // The same few over and over: each once.
        let repeated = "NTLM, Negotiate, ntlm, ".repeat(3000);
        assert_eq!(login_schemes([repeated.as_str()]), ["NTLM", "Negotiate"]);
        // A scheme's name inside a quoted string is none, an escaped quote
        // not ending the string.
        assert_eq!(login_schemes(["NTLM realm=\"a, Basic b\""]), ["NTLM"]);
        assert_eq!(
            login_schemes(["Digest realm=\"a\\\", Basic b\", NTLM"]),
            ["Digest", "NTLM"]
        );
    }

    /// fixR7 #6: a kept connection is on the line of the request using it,
    /// for that request's time alone. An abandon of the request on it shuts
    /// it down — a kept connection used to be out of every abandon's reach —
    /// and the abandon of a request that is over, arriving late, cannot
    /// reach the request the connection serves by then.
    #[test]
    fn a_kept_connection_is_on_its_current_requests_line_alone() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (arrived_tx, arrived) = std::sync::mpsc::channel();
        let (go_tx, go) = std::sync::mpsc::channel::<()>();
        let (closed_tx, closed) = std::sync::mpsc::channel();
        // One connection, three requests: the second answered when told,
        // the third never.
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            for n in 1..=3 {
                if crate::testutil::read_http(&mut conn).head.is_empty() {
                    return;
                }
                let _ = arrived_tx.send(n);
                if n == 2 && go.recv_timeout(Duration::from_secs(20)).is_err() {
                    return;
                }
                if n == 3 {
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(20)));
                    let _ = std::io::Read::read(&mut conn, &mut [0u8; 1]);
                    let _ = closed_tx.send(Instant::now());
                    return;
                }
                let _ = conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
            }
        });
        let profile = Profile {
            pooled: true,
            ..test_profile()
        };
        let agent = agent(Route::Direct, test_trust(), profile);
        // A request on a thread of its own, tethered to `line`.
        let request = |line: Arc<Line>| {
            let (agent, url) = (agent.clone(), format!("{base}/x"));
            let (done_tx, done) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _tether = Line::tether(line);
                let _ = done_tx.send(call(&agent, &url));
            });
            done
        };
        let next = || arrived.recv_timeout(Duration::from_secs(10));
        let first = Arc::new(Line::default());
        let done = request(first.clone());
        assert_eq!(
            done.recv_timeout(Duration::from_secs(10))
                .unwrap()
                .as_deref(),
            Ok("ok")
        );
        assert_eq!(next(), Ok(1));

        // The first request's abandon, late, while the second is on the
        // connection: the second goes on.
        let done = request(Arc::new(Line::default()));
        assert_eq!(
            next(),
            Ok(2),
            "the second request came on another connection"
        );
        first.close();
        go_tx.send(()).unwrap();
        assert_eq!(
            done.recv_timeout(Duration::from_secs(10))
                .unwrap()
                .as_deref(),
            Ok("ok")
        );

        // The third request's own abandon shuts the connection down.
        let third = Arc::new(Line::default());
        let done = request(third.clone());
        assert_eq!(
            next(),
            Ok(3),
            "the third request came on another connection"
        );
        let abandoned = Instant::now();
        third.close();
        let result = done
            .recv_timeout(Duration::from_secs(2))
            .expect("the abandon did not reach the kept connection");
        assert!(result.unwrap_err().contains(ABANDONED));
        let at = closed.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(at.saturating_duration_since(abandoned) < Duration::from_secs(2));
    }
}
