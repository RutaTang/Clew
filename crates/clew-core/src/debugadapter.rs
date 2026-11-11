//! Debug-adapter resolution and provisioning, shared by the clew client
//! (local debugging, `src/dap/adapter.rs`) and clew-server (remote debugging,
//! [`resolve_stdio`]).
//!
//! [`resolve`] maps a language slug (the client's `Lang::slug`) plus a
//! program to the adapter to spawn and the `launch` request body it expects.
//! It never installs anything. An adapter that has to be installed first
//! comes back as [`Resolution::NeedsInstall`], whose
//! [`AdapterInstall::describe`] says exactly what would run; only
//! [`AdapterInstall::install_cancellable`] installs, and callers reach it only
//! through the user's consent — the same rule language-server installs
//! follow. The
//! Debug button used to `pip install` into the user's Python, or download
//! and unpack vscode-js-debug, with no question asked.
//!
//! Every tool is found by absolute path: `PATH` entries that are relative are
//! skipped (see [`store::find_on_path_unresolved`]) and `xcrun` is
//! `/usr/bin/xcrun`. A bare name is resolved by the OS against the inherited
//! `PATH`, where a relative entry would let the directory clew was started in
//! supply the "adapter". A hit is kept as found rather than canonicalized:
//! toolchain managers put shims on `PATH` that dispatch on the name they were
//! run by.
//!
//! A Python program is debugged under the project's own interpreter when it
//! has one — the virtualenv [`langenv`] picks, and hands the language server —
//! so the debugger imports the packages the editor resolves.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Value, json};

use crate::lsp::langenv;
use crate::lsp::registry::{Archive, Download, Sha256};
use crate::lsp::store;

/// The debugpy release a consented install asks pip for.
pub const DEBUGPY_VERSION: &str = "1.8.14";

/// The vscode-js-debug release clew downloads, verified by digest.
pub const JS_DEBUG_VERSION: &str = "1.117.0";

static JS_DEBUG: Download = Download {
    url: "https://github.com/microsoft/vscode-js-debug/releases/download/v1.117.0/js-debug-dap-v1.117.0.tar.gz",
    sha256: Sha256::from_hex("ad8d04ede9d4b75cc290fd5438a65047a06f786d04f604b6112485b36f090772"),
    archive: Archive::TarGz,
    // The tarball has a top-level `js-debug/`.
    binary: "js-debug/src/dapDebugServer.js",
};

/// How long the `python -c` probe may take. It imports nothing but the
/// standard library; a probe this slow is a hung interpreter.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long `pip install debugpy` may take.
const PIP_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long `xcrun -f` may take.
const XCRUN_TIMEOUT: Duration = Duration::from_secs(20);

/// How the client talks to the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterTransport {
    /// DAP over the adapter's own stdin/stdout.
    Stdio,
    /// The adapter listens on a loopback TCP port. It is started on port 0 —
    /// the OS picks a free port and the adapter announces it on its stdout —
    /// so the client connects to the port OUR child bound, never to a port
    /// chosen in advance that another process could have taken meanwhile.
    Tcp,
    /// As [`Self::Tcp`], for an adapter that IS the debuggee (Ruby's `rdbg`
    /// runs the program in its own process): it announces its port on
    /// stderr, and its stdout — and the rest of its stderr — is the program's
    /// output.
    TcpOnStderr,
}

/// A resolved adapter: what to spawn, and the `launch` request body.
#[derive(Debug, Clone)]
pub struct AdapterSpec {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub launch: Value,
    pub transport: AdapterTransport,
}

/// The outcome of [`resolve`].
#[derive(Debug, Clone)]
pub enum Resolution {
    Ready(AdapterSpec),
    /// The adapter must be installed first — with the user's consent.
    NeedsInstall(AdapterInstall),
}

/// A debug-adapter install awaiting the user's consent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterInstall {
    /// `python -m pip install debugpy==<DEBUGPY_VERSION>` into THIS
    /// interpreter — the one the adapter will run under; `user` adds
    /// `--user` (not inside a virtualenv, where pip refuses it).
    Debugpy { python: PathBuf, user: bool },
    /// Download the pinned vscode-js-debug, verify its SHA-256, and unpack it
    /// into clew's data directory.
    JsDebug,
}

impl AdapterInstall {
    /// Short name for the consent prompt ("debugpy").
    pub fn name(&self) -> &'static str {
        match self {
            AdapterInstall::Debugpy { .. } => "debugpy",
            AdapterInstall::JsDebug => "vscode-js-debug",
        }
    }

    pub fn version(&self) -> &'static str {
        match self {
            AdapterInstall::Debugpy { .. } => DEBUGPY_VERSION,
            AdapterInstall::JsDebug => JS_DEBUG_VERSION,
        }
    }

    /// Exactly what [`Self::install_cancellable`] will do, for the consent
    /// prompt: the full command line for pip, the URL and digest for a
    /// download.
    pub fn describe(&self) -> String {
        match self {
            AdapterInstall::Debugpy { python, user } => {
                format!("{} {}", python.display(), pip_args(*user).join(" "))
            }
            AdapterInstall::JsDebug => format!(
                "download {} (SHA-256 {}) into Clew's data directory",
                JS_DEBUG.url, JS_DEBUG.sha256
            ),
        }
    }

    /// Perform the install, uncancellable — so test-only: the app installs
    /// with [`Self::install_cancellable`], which leaving the project stops.
    #[cfg(test)]
    pub fn install(&self) -> Result<(), String> {
        self.install_cancellable(&AtomicBool::new(false))
    }

    /// Perform the install — only ever after the user consented to
    /// [`Self::describe`]; blocking, so run off the UI thread — abandoned once
    /// `cancel` is set: a download between chunks, a running `pip` killed with
    /// everything it started.
    pub fn install_cancellable(&self, cancel: &AtomicBool) -> Result<(), String> {
        match self {
            AdapterInstall::Debugpy { python, user } => install_debugpy(python, *user, cancel),
            AdapterInstall::JsDebug => install_js_debug(cancel).map(|_| ()),
        }
    }
}

/// Resolve the adapter for `lang` (a slug: native, python, dart, go, node)
/// against this host's environment. Blocking: it may run `xcrun` or a
/// `python -c` probe, each time-boxed. Never installs anything.
///
/// `root` is the project root: where a Python program's virtualenv is looked
/// for, exactly as [`langenv`] looks for the language server's. `cwd` is the
/// debug session's working directory — the root unless the launch
/// configuration names another — and only goes into the `launch` body. The
/// venv used to be looked for in `cwd`, so a launch configuration running a
/// script from `tools/` debugged it under whatever `python3` came first on
/// `PATH`, without the project's packages. A Python program is debugged under
/// the interpreter the user chose for the project (`choices`), when they
/// chose one and it can be used.
pub fn resolve(
    lang: &str,
    program: &str,
    args: &[String],
    root: &Path,
    cwd: &Path,
    choices: &clew_protocol::LanguageChoices,
    server: Option<&Path>,
) -> Result<Resolution, String> {
    match lang {
        "native" => {
            let command = lldb_dap_names()
                .iter()
                .find_map(|name| store::find_on_path_unresolved(name))
                .or_else(|| xcrun_find("lldb-dap"))
                .ok_or(
                    "lldb-dap not found — install LLVM (lldb-dap) or the Xcode command-line tools",
                )?;
            // A Zig source file is built for the session, through LLVM: its
            // debug information is then one lldb reads variables from.
            let binary = match is_zig_source(program) {
                true => build_zig(&zig_for(choices)?, &in_dir(cwd, program), cwd)?,
                false => PathBuf::from(program),
            };
            let mut spec = native_spec(command, &binary.to_string_lossy(), args, cwd);
            if dwarf_language(&in_dir(cwd, &binary.to_string_lossy())) == Some(DW_LANG_ZIG) {
                spec.launch[NOTICE] = json!(ZIG_BACKEND_NOTICE);
            }
            Ok(Resolution::Ready(spec))
        }
        "python" => {
            let python = python_for(
                root,
                choices.python.as_deref(),
                std::env::var_os("PATH").as_deref(),
            )
            .ok_or("python not found on PATH")?;
            python_with(&python, program, args, cwd)
        }
        "dart" => {
            let command = match chosen_sdk(choices, "dart") {
                Some(chosen) => store::find_program(chosen)
                    .map_err(|why| format!("the Dart SDK you chose cannot be run: {why}"))?,
                None => store::find_on_path_unresolved("dart")
                    .ok_or("dart not found — install the Dart/Flutter SDK")?,
            };
            Ok(Resolution::Ready(dart_spec(command, program, args, cwd)))
        }
        "go" => {
            let command = store::find_on_path_unresolved("dlv").ok_or(
                "dlv not found — run: go install github.com/go-delve/delve/cmd/dlv@latest",
            )?;
            let tags = choices.go.as_ref().and_then(langenv::go_tags_flag);
            Ok(Resolution::Ready(go_spec(
                command,
                program,
                args,
                cwd,
                tags.as_deref(),
            )))
        }
        "node" => {
            let node =
                store::find_on_path_unresolved("node").ok_or("node not found — install Node.js")?;
            Ok(node_with(node, program, args, cwd))
        }
        "ruby" => {
            // The Ruby whose Ruby LSP the user chose has its own `rdbg`, its
            // gems' Ruby: debugged under it, as the editor reads the code.
            let beside_chosen = chosen_sdk(choices, "ruby")
                .and_then(|chosen| Path::new(chosen.trim()).parent().map(|d| d.join("rdbg")))
                .filter(|rdbg| rdbg.is_absolute() && rdbg.is_file());
            let command = match beside_chosen {
                Some(rdbg) => rdbg,
                None => store::find_on_path_unresolved("rdbg")
                    .ok_or("rdbg not found — install Ruby's debugger: gem install debug")?,
            };
            Ok(Resolution::Ready(ruby_spec(command, program, args, cwd)))
        }
        "csharp" => {
            let command = store::find_on_path_unresolved("netcoredbg").ok_or(
                "netcoredbg not found — install it (github.com/Samsung/netcoredbg) and put it on the PATH",
            )?;
            Ok(Resolution::Ready(csharp_spec(command, program, args, cwd)))
        }
        "php" => {
            let php = store::find_on_path_unresolved("php")
                .ok_or("php not found — install PHP, with Xdebug 3")?;
            let server = server.ok_or("clew's PHP debug adapter (clew-server) is not available")?;
            Ok(Resolution::Ready(php_spec(
                server, &php, program, args, cwd,
            )))
        }
        "java" => {
            // The JDK chosen for the project (Settings → Language servers), else
            // the PATH's.
            let chosen = choices
                .java
                .as_deref()
                .map(|home| Path::new(home.trim()).join("bin"))
                .filter(|bin| bin.is_absolute() && bin.join("java").is_file());
            let (java, javac) = match chosen {
                Some(bin) => (bin.join("java"), bin.join("javac")),
                None => (
                    store::find_on_path_unresolved("java")
                        .ok_or("java not found — install a JDK")?,
                    store::find_on_path_unresolved("javac")
                        .ok_or("javac not found — install a JDK (not only a JRE)")?,
                ),
            };
            let server =
                server.ok_or("clew's Java debug adapter (clew-server) is not available")?;
            let args_out = vec![
                "java-debug".to_string(),
                "--java".to_string(),
                java.to_string_lossy().into_owned(),
                "--javac".to_string(),
                javac.to_string_lossy().into_owned(),
            ];
            Ok(Resolution::Ready(own_spec(
                server, args_out, program, args, cwd,
            )))
        }
        "erlang" => {
            let erl = store::find_on_path_unresolved("erl")
                .ok_or("erl not found — install Erlang/OTP, with its debugger application")?;
            let erlc = store::find_on_path_unresolved("erlc")
                .ok_or("erlc not found — install Erlang/OTP")?;
            let server =
                server.ok_or("clew's Erlang debug adapter (clew-server) is not available")?;
            let args_out = vec![
                "erlang-debug".to_string(),
                "--erl".to_string(),
                erl.to_string_lossy().into_owned(),
                "--erlc".to_string(),
                erlc.to_string_lossy().into_owned(),
            ];
            Ok(Resolution::Ready(own_spec(
                server, args_out, program, args, cwd,
            )))
        }
        "haskell" => {
            // The GHC whose language server the user chose (ghcup keeps
            // them side by side), else the PATH's.
            let beside_chosen = chosen_sdk(choices, "haskell")
                .and_then(|chosen| Path::new(chosen.trim()).parent().map(|d| d.join("ghci")))
                .filter(|ghci| ghci.is_absolute() && ghci.is_file());
            let ghci = match beside_chosen {
                Some(ghci) => ghci,
                None => store::find_on_path_unresolved("ghci")
                    .ok_or("ghci not found — install GHC (ghcup install ghc)")?,
            };
            let server =
                server.ok_or("clew's Haskell debug adapter (clew-server) is not available")?;
            let args_out = vec![
                "haskell-debug".to_string(),
                "--ghci".to_string(),
                ghci.to_string_lossy().into_owned(),
            ];
            Ok(Resolution::Ready(own_spec(
                server, args_out, program, args, cwd,
            )))
        }
        other => Err(format!("no debug adapter for {other}")),
    }
}

/// The SDK program the user chose for `language`'s server, when they chose
/// where its toolchain is (a program with no arguments of their own: the
/// toolchain's, run as clew runs it) — the debugger runs the same SDK.
fn chosen_sdk<'a>(choices: &'a clew_protocol::LanguageChoices, language: &str) -> Option<&'a str> {
    choices
        .servers
        .as_ref()?
        .get(language)
        .filter(|choice| choice.args.is_none())
        .map(|choice| choice.command.as_str())
        .filter(|command| !command.trim().is_empty())
}

/// The interpreter to debug a Python program in `root` under: the one the
/// user `chosen`, when it can be used ([`langenv::chosen_interpreter`]); else
/// the project's own virtualenv, exactly as [`langenv`] picks it for the
/// language server — and held to the same check of what executing it would
/// run — else the first `python3` (then `python`) on `path`.
///
/// Every candidate is kept as found, never resolved: a virtualenv's
/// `bin/python` is a symlink to the base interpreter, and Python finds its
/// environment from the path it was run by, so following the link would
/// debug under the base install instead of the venv whose packages the
/// program imports.
fn python_for(
    root: &Path,
    chosen: Option<&str>,
    path: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let chosen = chosen.and_then(|c| langenv::chosen_interpreter(root, c));
    chosen
        .or_else(|| langenv::find_python_interpreter(root))
        .or_else(|| {
            let path = path?;
            store::find_in_unresolved(path, "python3")
                .or_else(|| store::find_in_unresolved(path, "python"))
        })
}

/// lldb-dap for native (Rust/C/C++) binaries. Ships with Xcode / LLVM.
/// The names LLVM's DAP adapter goes by: `lldb-dap`; the versioned
/// `lldb-dap-18` a Linux distribution installs beside other LLVMs, newest
/// first; and `lldb-vscode`, its name before LLVM 18.
fn lldb_dap_names() -> Vec<String> {
    let mut names = vec!["lldb-dap".to_string()];
    names.extend((14..=25).rev().map(|v| format!("lldb-dap-{v}")));
    names.push("lldb-vscode".into());
    names.extend((11..=17).rev().map(|v| format!("lldb-vscode-{v}")));
    names
}

/// A key of a `launch` body that is Clew's, not the adapter's: a notice the
/// client shows in the debug console as the session starts, and takes out
/// of the body before sending it.
pub const NOTICE: &str = "__clewNotice";

/// DWARF's language code for Zig (`DW_LANG_Zig`), which Zig's own code
/// generator — the default for Debug builds on x86-64 — puts on its compile
/// units. lldb has no plugin for it and shows no variables there.
const DW_LANG_ZIG: u16 = 0x27;

/// What a Zig program built by Zig's own code generator is told.
const ZIG_BACKEND_NOTICE: &str = "This program's debug information is Zig's own (from Zig's \
     built-in code generator, the default for Debug builds on x86-64), and lldb reads no \
     variables from it: breakpoints and stepping work, but no variables show. Build it with \
     -fllvm (or .use_llvm = true in build.zig) to see them.";

/// How long building a Zig program for a session may take.
const ZIG_BUILD_TIMEOUT: Duration = Duration::from_secs(600);

/// `program`, against `cwd` when it is relative.
fn in_dir(cwd: &Path, program: &str) -> PathBuf {
    let program = Path::new(program);
    match program.is_absolute() {
        true => program.to_path_buf(),
        false => cwd.join(program),
    }
}

/// Whether `program` is Zig source, built for the session.
fn is_zig_source(program: &str) -> bool {
    Path::new(program)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zig"))
}

/// The `zig` compiler: the one chosen for the project (Settings →
/// Languages), else the PATH's.
fn zig_for(choices: &clew_protocol::LanguageChoices) -> Result<PathBuf, String> {
    match choices.zig.as_deref().filter(|z| !z.trim().is_empty()) {
        Some(chosen) => store::find_program(chosen)
            .map_err(|why| format!("the Zig toolchain you chose cannot be run: {why}")),
        None => store::find_on_path_unresolved("zig")
            .ok_or_else(|| "zig not found — install Zig to debug a .zig file".to_string()),
    }
}

/// Build `source` for a session — Debug, through LLVM (`-fllvm`), whose
/// debug information lldb reads, unlike that of Zig's own code generator;
/// a Zig built without LLVM builds it its own way. The build and Zig's cache
/// for it go in Clew's data directory, not the project.
fn build_zig(zig: &Path, source: &Path, cwd: &Path) -> Result<PathBuf, String> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    let out = store::data_root()
        .ok_or("no data directory to build the Zig program in")?
        .join("debug-builds")
        .join("zig")
        .join(format!("{:016x}", hasher.finish()));
    std::fs::create_dir_all(&out).map_err(|e| format!("cannot make {}: {e}", out.display()))?;
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "program".into());
    let binary = out.join(match cfg!(windows) {
        true => format!("{stem}.exe"),
        false => stem,
    });
    let build = |llvm: bool| {
        let mut cmd = crate::spawn::command(zig);
        cmd.args(["build-exe", "-O", "Debug"]);
        if llvm {
            cmd.arg("-fllvm");
        }
        cmd.arg("--cache-dir")
            .arg(out.join("cache"))
            .arg(format!("-femit-bin={}", binary.display()))
            .arg(source)
            .current_dir(cwd);
        store::run_bounded(cmd, ZIG_BUILD_TIMEOUT)
    };
    let mut built = build(true)?;
    let said = |o: &std::process::Output| String::from_utf8_lossy(&o.stderr).into_owned();
    if !built.status.success() && said(&built).contains("LLVM") {
        built = build(false)?;
    }
    if !built.status.success() {
        let said = said(&built);
        let tail: Vec<&str> = said.lines().rev().take(40).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(format!(
            "zig could not build {}:\n{}",
            source.display(),
            tail.join("\n")
        ));
    }
    Ok(binary)
}

/// The language DWARF names for an ELF binary's first compile unit
/// (`DW_AT_language`), if it has one this can read: 64-bit little-endian
/// ELF, DWARF 2 to 5, sections not compressed.
fn dwarf_language(binary: &Path) -> Option<u16> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(binary).ok()?;
    let mut head = [0u8; 64];
    file.read_exact(&mut head).ok()?;
    // ELF, 64-bit, little-endian.
    if &head[..4] != b"\x7fELF" || head[4] != 2 || head[5] != 1 {
        return None;
    }
    let u16_at =
        |b: &[u8], at: usize| Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?));
    let u32_at =
        |b: &[u8], at: usize| Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?));
    let u64_at =
        |b: &[u8], at: usize| Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?));
    let shoff = u64_at(&head, 0x28)?;
    let (shentsize, shnum, shstrndx) = (
        usize::from(u16_at(&head, 0x3a)?),
        usize::from(u16_at(&head, 0x3c)?),
        usize::from(u16_at(&head, 0x3e)?),
    );
    if shentsize < 64 || shnum == 0 || shnum > 4096 || shstrndx >= shnum {
        return None;
    }
    let mut table = vec![0u8; shentsize * shnum];
    file.seek(SeekFrom::Start(shoff)).ok()?;
    file.read_exact(&mut table).ok()?;
    // Each section: (name offset, flags, file offset, size).
    let section = |i: usize| {
        let at = i * shentsize;
        Some((
            u32_at(&table, at)? as u64,
            u64_at(&table, at + 8)?,
            u64_at(&table, at + 0x18)?,
            u64_at(&table, at + 0x20)?,
        ))
    };
    let mut read = |offset: u64, size: u64, cap: u64| -> Option<Vec<u8>> {
        let mut bytes = vec![0u8; size.min(cap) as usize];
        file.seek(SeekFrom::Start(offset)).ok()?;
        file.read_exact(&mut bytes).ok()?;
        Some(bytes)
    };
    let (_, _, names_at, names_size) = section(shstrndx)?;
    let names = read(names_at, names_size, 1 << 20)?;
    let mut info = None;
    let mut abbrev = None;
    for i in 0..shnum {
        let (name, flags, offset, size) = section(i)?;
        let name = names.get(name as usize..)?;
        let name = &name[..name.iter().position(|b| *b == 0)?];
        // SHF_COMPRESSED: not read here.
        if flags & 0x800 != 0 {
            continue;
        }
        match name {
            b".debug_info" => info = Some((offset, size)),
            b".debug_abbrev" => abbrev = Some((offset, size)),
            _ => {}
        }
    }
    let (info_at, info_size) = info?;
    let (abbrev_at, abbrev_size) = abbrev?;
    let info = read(info_at, info_size, 1 << 16)?;
    let abbrev = read(abbrev_at, abbrev_size, 16 << 20)?;
    first_unit_language(&info, &abbrev)
}

/// The `DW_AT_language` of the first compile unit in `info` (a
/// `.debug_info`, its start at least), its abbreviations in `abbrev`.
fn first_unit_language(info: &[u8], abbrev: &[u8]) -> Option<u16> {
    fn uleb(b: &[u8], at: &mut usize) -> Option<u64> {
        let (mut value, mut shift) = (0u64, 0u32);
        loop {
            let byte = *b.get(*at)?;
            *at += 1;
            if shift < 64 {
                value |= u64::from(byte & 0x7f) << shift;
            }
            shift += 7;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
    }
    fn sleb(b: &[u8], at: &mut usize) -> Option<i64> {
        let (mut value, mut shift) = (0i64, 0u32);
        loop {
            let byte = *b.get(*at)?;
            *at += 1;
            if shift < 64 {
                value |= i64::from(byte & 0x7f) << shift;
            }
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Some(value);
            }
        }
    }
    let le = |b: &[u8], at: usize, n: usize| -> Option<u64> {
        Some(
            b.get(at..at + n)?
                .iter()
                .rev()
                .fold(0u64, |v, x| (v << 8) | u64::from(*x)),
        )
    };
    let mut at = 0;
    let mut offset_size = 4;
    if le(info, 0, 4)? == 0xffff_ffff {
        offset_size = 8;
        at = 12;
    } else {
        at += 4;
    }
    let version = le(info, at, 2)?;
    at += 2;
    let (abbrev_offset, address_size) = match version {
        5 => {
            // unit_type, address_size, debug_abbrev_offset
            let address_size = le(info, at + 1, 1)?;
            let offset = le(info, at + 2, offset_size)?;
            at += 2 + offset_size;
            (offset, address_size as usize)
        }
        2..=4 => {
            let offset = le(info, at, offset_size)?;
            let address_size = le(info, at + offset_size, 1)?;
            at += offset_size + 1;
            (offset, address_size as usize)
        }
        _ => return None,
    };
    let code = uleb(info, &mut at)?;
    // The unit's abbreviation: its attributes' names and forms.
    let mut j = usize::try_from(abbrev_offset).ok()?;
    let specs = loop {
        let this = uleb(abbrev, &mut j)?;
        if this == 0 {
            return None;
        }
        uleb(abbrev, &mut j)?; // tag
        j += 1; // children
        let mut specs = Vec::new();
        loop {
            let name = uleb(abbrev, &mut j)?;
            let form = uleb(abbrev, &mut j)?;
            let implicit = match form {
                0x21 => Some(sleb(abbrev, &mut j)?),
                _ => None,
            };
            if name == 0 && form == 0 {
                break;
            }
            specs.push((name, form, implicit));
        }
        if this == code {
            break specs;
        }
    };
    for (name, form, implicit) in specs {
        if name == 0x13 {
            return match form {
                0x0b => le(info, at, 1),
                0x05 => le(info, at, 2),
                0x06 => le(info, at, 4),
                0x0f => uleb(info, &mut at),
                0x21 => implicit.map(|v| v as u64),
                _ => None,
            }
            .and_then(|v| u16::try_from(v).ok());
        }
        let size = match form {
            0x01 => address_size,
            0x0b | 0x0c | 0x11 | 0x25 | 0x29 => 1,
            0x05 | 0x12 | 0x26 | 0x2a => 2,
            0x27 | 0x2b => 3,
            0x06 | 0x13 | 0x1c | 0x28 | 0x2c => 4,
            0x07 | 0x14 | 0x20 | 0x24 => 8,
            0x1e => 16,
            0x0e | 0x10 | 0x17 | 0x1d | 0x1f => offset_size,
            0x19 | 0x21 => 0,
            0x08 => info.get(at..)?.iter().position(|b| *b == 0)? + 1,
            0x0f | 0x15 | 0x1a | 0x1b | 0x22 | 0x23 => {
                uleb(info, &mut at)?;
                0
            }
            0x0d => {
                sleb(info, &mut at)?;
                0
            }
            0x09 | 0x18 => usize::try_from(uleb(info, &mut at)?).ok()?,
            0x0a => {
                let n = usize::from(*info.get(at)?);
                at += 1;
                n
            }
            _ => return None,
        };
        at = at.checked_add(size)?;
    }
    None
}

fn native_spec(command: PathBuf, program: &str, args: &[String], cwd: &Path) -> AdapterSpec {
    AdapterSpec {
        command,
        args: Vec::new(),
        launch: json!({
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "stopOnEntry": false,
        }),
        transport: AdapterTransport::Stdio,
    }
}

/// Ruby's debugger, `rdbg` (the `debug` gem): it runs the program itself
/// and serves DAP on a loopback TCP port — port 0, announced on stderr as
/// `Debugger can attach via TCP/IP (<addr>)`. Its `launch` runs the program
/// on once configured.
///
/// `--stop-at-load` holds the program as the debugger loads, before its first
/// line. Held AT that line instead (rdbg's default), it wrote the line's
/// listing to a client that had just connected — as text, before reading
/// whether the client speaks DAP — and the session broke on it.
fn ruby_spec(command: PathBuf, program: &str, args: &[String], cwd: &Path) -> AdapterSpec {
    let mut adapter_args: Vec<String> = [
        "--open",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--stop-at-load",
        "--",
    ]
    .map(String::from)
    .to_vec();
    adapter_args.push(program.to_string());
    adapter_args.extend(args.iter().cloned());
    AdapterSpec {
        command,
        args: adapter_args,
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
        }),
        transport: AdapterTransport::TcpOnStderr,
    }
}

/// PHP, through clew-server's own adapter (`clew-server php-debug`): it
/// runs the program under `php` with Xdebug and speaks Xdebug's DBGp on one
/// side, DAP over stdio on the other.
fn php_spec(server: &Path, php: &Path, program: &str, args: &[String], cwd: &Path) -> AdapterSpec {
    AdapterSpec {
        command: server.to_path_buf(),
        args: vec![
            "php-debug".into(),
            "--php".into(),
            php.to_string_lossy().into_owned(),
        ],
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
        }),
        transport: AdapterTransport::Stdio,
    }
}

/// One of clew-server's own adapters (Erlang's, Haskell's): `server` run with
/// `adapter_args`, speaking DAP over stdio.
fn own_spec(
    server: &Path,
    adapter_args: Vec<String>,
    program: &str,
    args: &[String],
    cwd: &Path,
) -> AdapterSpec {
    AdapterSpec {
        command: server.to_path_buf(),
        args: adapter_args,
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
        }),
        transport: AdapterTransport::Stdio,
    }
}

/// netcoredbg for .NET: DAP over stdio (`--interpreter=vscode`); the
/// program is the built assembly (`bin/Debug/<tfm>/App.dll`).
fn csharp_spec(command: PathBuf, program: &str, args: &[String], cwd: &Path) -> AdapterSpec {
    AdapterSpec {
        command,
        args: vec!["--interpreter=vscode".into()],
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "stopAtEntry": false,
        }),
        transport: AdapterTransport::Stdio,
    }
}

/// debugpy: `python -m debugpy.adapter` speaks DAP over stdio and debugs the
/// same interpreter it runs under.
fn python_with(
    python: &Path,
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<Resolution, String> {
    let probe = probe_python(python)?;
    if !probe.has_debugpy {
        return Ok(Resolution::NeedsInstall(AdapterInstall::Debugpy {
            python: python.to_path_buf(),
            user: !probe.in_venv,
        }));
    }
    let mut launch = json!({
        "request": "launch",
        "program": program,
        "args": args,
        "cwd": cwd.to_string_lossy(),
        "console": "internalConsole",
        "python": [python.to_string_lossy()],
        "stopOnEntry": false,
        "justMyCode": false,
    });
    // A module of a package (`teashop/pricing.py`) run as a script could
    // not import its own package — "No module named 'teashop'": the folder
    // the package sits in goes on the import path. One that imports its
    // siblings relatively (`from .models import …`) has no package at all
    // as a script, so it runs as Python runs a package's module: `-m`.
    if let Some(package) = python_package(&in_dir(cwd, program)) {
        let mut path = vec![package.root.clone()];
        path.extend(
            std::env::var_os("PYTHONPATH")
                .map_or_else(Vec::new, |p| std::env::split_paths(&p).collect::<Vec<_>>()),
        );
        if let Ok(joined) = std::env::join_paths(path) {
            launch["env"] = json!({ "PYTHONPATH": joined.to_string_lossy() });
        }
        if package.relative_imports
            && let Some(launch) = launch.as_object_mut()
        {
            launch.remove("program");
            launch.insert("module".into(), json!(package.module));
        }
    }
    Ok(Resolution::Ready(AdapterSpec {
        command: python.to_path_buf(),
        args: vec!["-m".into(), "debugpy.adapter".into()],
        launch,
        transport: AdapterTransport::Stdio,
    }))
}

/// The package a Python file is a module of: the folder the outermost
/// package sits in (what goes on the import path), the module's dotted
/// name, and whether it imports relatively. `None` for a file outside any
/// package (no `__init__.py` beside it), or a package's own `__init__.py`.
#[derive(Debug, PartialEq)]
struct PythonPackage {
    root: PathBuf,
    module: String,
    relative_imports: bool,
}

fn python_package(file: &Path) -> Option<PythonPackage> {
    let stem = file.file_stem()?.to_str()?;
    if stem == "__init__" || file.extension()? != "py" {
        return None;
    }
    let mut dir = file.parent()?;
    let mut parts = Vec::new();
    while dir.join("__init__.py").is_file() {
        parts.push(dir.file_name()?.to_str()?.to_string());
        dir = dir.parent()?;
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    // `python -m pkg` runs `pkg/__main__.py`.
    if stem != "__main__" {
        parts.push(stem.to_string());
    }
    let source = std::fs::read_to_string(file).unwrap_or_default();
    let relative_imports = source.lines().any(|line| {
        let line = line.trim_start();
        line.strip_prefix("from").is_some_and(|rest| {
            rest.trim_start().starts_with('.') && rest.starts_with(char::is_whitespace)
        })
    });
    Some(PythonPackage {
        root: dir.to_path_buf(),
        module: parts.join("."),
        relative_imports,
    })
}

/// Dart's SDK ships a DAP adapter: `dart debug_adapter` over stdio.
fn dart_spec(command: PathBuf, program: &str, args: &[String], cwd: &Path) -> AdapterSpec {
    AdapterSpec {
        command,
        args: vec!["debug_adapter".into()],
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "toolArgs": [],
        }),
        transport: AdapterTransport::Stdio,
    }
}

/// Delve for Go: `dlv dap` (mode=debug compiles and runs the package). It
/// listens on a TCP port: port 0, announced as `DAP server listening at:
/// <addr>`. `tags` is the `-tags=…` flag of the build tags the user chose
/// for the project, so the program is built as the editor reads it.
fn go_spec(
    command: PathBuf,
    program: &str,
    args: &[String],
    cwd: &Path,
    tags: Option<&str>,
) -> AdapterSpec {
    let mut launch = json!({
        "request": "launch",
        "mode": "debug",
        "program": program,
        "args": args,
        "cwd": cwd.to_string_lossy(),
        // The program's output as DAP output, not on delve's own stdout —
        // which no one reads (and, remotely, which is on another host).
        "outputMode": "remote",
    });
    if let Some(tags) = tags {
        launch["buildFlags"] = json!(tags);
    }
    AdapterSpec {
        command,
        args: vec!["dap".into(), "--listen".into(), "127.0.0.1:0".into()],
        launch,
        transport: AdapterTransport::Tcp,
    }
}

/// vscode-js-debug for JS/TS: `node dapDebugServer.js <port> <host>` listens
/// on TCP — port 0, announced as `Debug server listening at <addr>`. It is
/// provisioned under clew's data dir; until it is, this is a
/// [`Resolution::NeedsInstall`].
fn node_with(node: PathBuf, program: &str, args: &[String], cwd: &Path) -> Resolution {
    let Some(server) = js_debug_server() else {
        return Resolution::NeedsInstall(AdapterInstall::JsDebug);
    };
    Resolution::Ready(AdapterSpec {
        command: node,
        args: vec![
            server.to_string_lossy().into_owned(),
            "0".into(),
            "127.0.0.1".into(),
        ],
        launch: json!({
            "type": "pwa-node",
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "console": "internalConsole",
        }),
        transport: AdapterTransport::Tcp,
    })
}

/// What a `python -c` probe learned about an interpreter.
#[derive(Debug, PartialEq, Eq)]
struct PythonProbe {
    /// `debugpy` is importable (found, not imported: nothing of it runs).
    has_debugpy: bool,
    /// Running inside a virtualenv (`sys.prefix != sys.base_prefix`).
    in_venv: bool,
}

const PROBE: &str = "import sys, importlib.util\n\
print(int(importlib.util.find_spec('debugpy') is not None), \
int(sys.prefix != getattr(sys, 'base_prefix', sys.prefix)))";

/// Ask `python` about itself, time-boxed.
fn probe_python(python: &Path) -> Result<PythonProbe, String> {
    probe_python_within(python, PROBE_TIMEOUT)
}

fn probe_python_within(python: &Path, limit: Duration) -> Result<PythonProbe, String> {
    let mut cmd = crate::spawn::command(python);
    cmd.args(["-c", PROBE]);
    neutral(&mut cmd);
    let out = store::run_bounded(cmd, limit)
        .map_err(|e| format!("could not probe {}: {e}", python.display()))?;
    if !out.status.success() {
        return Err(format!("{} could not be run as Python 3", python.display()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split_whitespace();
    match (fields.next(), fields.next()) {
        (Some(debugpy), Some(venv)) => Ok(PythonProbe {
            has_debugpy: debugpy == "1",
            in_venv: venv == "1",
        }),
        _ => Err(format!("unexpected answer from {}", python.display())),
    }
}

/// Run a Python helper where no project can inject modules into it. `-c`
/// and `-m` put the working directory first on `sys.path`, so a `debugpy/`
/// or `pip/` directory in whatever directory clew was started from would be
/// imported instead of the real one — its code run before the user has
/// agreed to anything. The root directory holds no such thing, and
/// `PYTHONSAFEPATH` (3.11+) drops the entry altogether.
fn neutral(cmd: &mut Command) {
    let dir = if cfg!(unix) {
        PathBuf::from("/")
    } else {
        std::env::temp_dir()
    };
    cmd.current_dir(dir).env("PYTHONSAFEPATH", "1");
}

/// pip's arguments for the debugpy install, pinned. Rendered into the
/// consent prompt as they are passed.
fn pip_args(user: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "-m",
        "pip",
        "install",
        "--disable-pip-version-check",
        "--no-input",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if user {
        args.push("--user".into());
    }
    args.push(format!("debugpy=={DEBUGPY_VERSION}"));
    args
}

fn pip_command(python: &Path, user: bool) -> Command {
    let mut cmd = crate::spawn::command(python);
    cmd.args(pip_args(user));
    neutral(&mut cmd);
    cmd
}

fn install_debugpy(python: &Path, user: bool, cancel: &AtomicBool) -> Result<(), String> {
    let out = store::run_bounded_cancellable(pip_command(python, user), PIP_TIMEOUT, cancel)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("pip failed");
        return Err(format!(
            "pip install debugpy=={DEBUGPY_VERSION}: {}",
            last.trim()
        ));
    }
    // Confirm with the interpreter the adapter will actually run under.
    if probe_python(python)?.has_debugpy {
        Ok(())
    } else {
        Err(format!(
            "installed debugpy but {} still can't import it",
            python.display()
        ))
    }
}

/// Root under clew's data dir where downloadable debug adapters live.
fn adapters_root() -> Option<PathBuf> {
    Some(store::data_root()?.join("debug-adapters"))
}

/// The versioned install directory for vscode-js-debug.
fn js_debug_dir() -> Option<PathBuf> {
    Some(
        adapters_root()?
            .join("vscode-js-debug")
            .join(JS_DEBUG_VERSION),
    )
}

/// The provisioned vscode-js-debug DAP server entrypoint, if installed.
///
/// Installs are staged and swapped in whole (see `lsp::store`), so the
/// entrypoint existing means the whole tree does — the old in-place unpack
/// could leave a half-extracted adapter that passed this check forever.
pub fn js_debug_server() -> Option<PathBuf> {
    let path = js_debug_dir()?.join(JS_DEBUG.binary);
    path.is_file().then_some(path)
}

/// Download, verify (pinned SHA-256), and unpack vscode-js-debug. Blocking.
fn install_js_debug(cancel: &AtomicBool) -> Result<PathBuf, String> {
    let root = adapters_root().ok_or("no data directory")?;
    store::create_store_dir(&root)?;
    let dest = js_debug_dir().ok_or("no data directory")?;
    store::install_download(&JS_DEBUG, &dest, &root, cancel)
}

/// Locate a tool in the active Xcode toolchain via `xcrun -f`, by absolute
/// path: a bare `xcrun` was looked up on the inherited PATH.
fn xcrun_find(tool: &str) -> Option<PathBuf> {
    const XCRUN: &str = "/usr/bin/xcrun";
    if !cfg!(target_os = "macos") || !Path::new(XCRUN).is_file() {
        return None;
    }
    let mut cmd = crate::spawn::command(XCRUN);
    cmd.args(["-f", tool]).current_dir("/");
    let out = store::run_bounded(cmd, XCRUN_TIMEOUT).ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    (path.is_absolute() && path.is_file()).then_some(path)
}

/// A resolved stdio adapter for clew-server: what to spawn, and the `launch`
/// request body serialized (the client passes it to the adapter verbatim,
/// with paths that are native to THIS host).
#[derive(Debug)]
pub struct StdioAdapter {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub launch: String,
}

/// The server's view of [`resolve`], for adapters a remote host can proxy
/// over the protocol, whose process channel carries a process's stdio:
///   - stdio adapters (lldb-dap, debugpy, `dart debug_adapter`, netcoredbg)
///     run as they are;
///   - an adapter that listens on TCP (delve, rdbg) listens on the REMOTE's
///     loopback, which the client cannot reach: it runs behind `bridge` —
///     clew-server's own `dap-bridge`, which serves it over stdio — and is
///     refused without one;
///   - Node runs under clew-server's own adapter (`node-debug`, over Node's
///     inspector), not vscode-js-debug, which opens a connection of its own
///     per debugged process — more than one stdio channel carries;
///   - no provisioning: installing on a remote host without the user's
///     explicit consent would break the rule `LspInstall` exists for. The
///     error names what to install instead.
///
/// `root` is the project root, which is also the session's working directory:
/// `SpawnAdapter` carries no other. `choices` are the user's for the project
/// (see [`resolve`]).
pub fn resolve_stdio(
    lang: &str,
    program: &str,
    args: &[String],
    root: &Path,
    choices: &clew_protocol::LanguageChoices,
    bridge: Option<&Path>,
) -> Result<StdioAdapter, String> {
    let tcp_refusal = || {
        format!(
            "the {lang} debug adapter speaks DAP over a TCP port on this host, \
             which a remote client cannot reach"
        )
    };
    if lang == "node" {
        let server = bridge.ok_or_else(|| {
            "Node is debugged on a remote host by clew-server's own adapter, \
             and there is none to run"
                .to_string()
        })?;
        let node = store::find_on_path_unresolved("node")
            .ok_or("on the remote host: node not found — install Node.js")?;
        let spec = own_spec(
            server,
            vec![
                "node-debug".into(),
                "--node".into(),
                node.to_string_lossy().into_owned(),
            ],
            program,
            args,
            root,
        );
        return Ok(StdioAdapter {
            command: spec.command,
            args: spec.args,
            launch: spec.launch.to_string(),
        });
    }
    match resolve(lang, program, args, root, root, choices, bridge)
        .map_err(|e| format!("on the remote host: {e}"))?
    {
        Resolution::Ready(spec) => match (spec.transport, bridge) {
            (AdapterTransport::Stdio, _) => Ok(StdioAdapter {
                command: spec.command,
                args: spec.args,
                launch: spec.launch.to_string(),
            }),
            (transport, Some(bridge)) => Ok(StdioAdapter {
                command: bridge.to_path_buf(),
                args: bridge_args(transport, &spec.command, &spec.args),
                launch: spec.launch.to_string(),
            }),
            (_, None) => Err(tcp_refusal()),
        },
        Resolution::NeedsInstall(install) => Err(format!(
            "{} is not installed on the remote host — install it there: {}",
            install.name(),
            install.describe()
        )),
    }
}

/// The arguments clew-server's `dap-bridge` runs a TCP adapter with:
/// `dap-bridge --announce stdout|stderr -- <adapter> <args…>` — where it
/// announces its port saying what the rest of its output is.
pub fn bridge_args(transport: AdapterTransport, command: &Path, args: &[String]) -> Vec<String> {
    let announce = match transport {
        AdapterTransport::TcpOnStderr => "stderr",
        AdapterTransport::Tcp | AdapterTransport::Stdio => "stdout",
    };
    let mut out = vec![
        "dap-bridge".to_string(),
        "--announce".to_string(),
        announce.to_string(),
        "--".to_string(),
        command.to_string_lossy().into_owned(),
    ];
    out.extend(args.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::lsp::store::testing::script;
    use crate::lsp::store::testing::with_store;
    #[cfg(unix)]
    use crate::testutil::EnvVars;
    use crate::testutil::TempDir;

    /// A compile unit's DWARF: version 5 or 4, its language as `data1` or
    /// `data2`, after a `strp` producer, before an inline name.
    fn dwarf_unit(version: u16, language: u16, form: u8) -> (Vec<u8>, Vec<u8>) {
        let abbrev = vec![1, 0x11, 1, 0x25, 0x0e, 0x13, form, 0x03, 0x08, 0, 0, 0];
        let mut die = vec![1, 0, 0, 0, 0];
        match form {
            0x0b => die.push(language as u8),
            _ => die.extend_from_slice(&language.to_le_bytes()),
        }
        die.extend_from_slice(b"app.zig\0");
        let mut unit = version.to_le_bytes().to_vec();
        match version {
            5 => unit.extend_from_slice(&[1, 8, 0, 0, 0, 0]),
            _ => unit.extend_from_slice(&[0, 0, 0, 0, 8]),
        }
        unit.extend(die);
        let mut info = (unit.len() as u32).to_le_bytes().to_vec();
        info.extend(unit);
        (info, abbrev)
    }

    /// A 64-bit little-endian ELF file holding `info` and `abbrev` as its
    /// `.debug_info` and `.debug_abbrev`.
    fn elf_with(info: &[u8], abbrev: &[u8]) -> Vec<u8> {
        let names = b"\0.shstrtab\0.debug_info\0.debug_abbrev\0";
        let mut body = Vec::new();
        let names_at = 64;
        body.extend_from_slice(names);
        let info_at = 64 + body.len();
        body.extend_from_slice(info);
        let abbrev_at = 64 + body.len();
        body.extend_from_slice(abbrev);
        let shoff = 64 + body.len();
        let mut file = vec![0u8; 64];
        file[..6].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1]);
        file[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
        file[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        file[0x3c..0x3e].copy_from_slice(&4u16.to_le_bytes());
        file[0x3e..0x40].copy_from_slice(&1u16.to_le_bytes());
        file.extend(body);
        let section = |name: u32, offset: usize, size: usize| {
            let mut header = vec![0u8; 64];
            header[..4].copy_from_slice(&name.to_le_bytes());
            header[0x18..0x20].copy_from_slice(&(offset as u64).to_le_bytes());
            header[0x20..0x28].copy_from_slice(&(size as u64).to_le_bytes());
            header
        };
        file.extend(vec![0u8; 64]);
        file.extend(section(1, names_at, names.len()));
        file.extend(section(11, info_at, info.len()));
        file.extend(section(23, abbrev_at, abbrev.len()));
        file
    }

    /// Zig's own code generator marks its units `DW_LANG_Zig`, LLVM marks
    /// them C99: read from the first unit of an ELF binary, by version and
    /// form, and nothing read from what is not one.
    #[test]
    fn a_binary_s_dwarf_language_is_read() {
        for (version, language, form) in [(5, 0x27, 0x0b), (4, 0x0c, 0x05), (5, 0x1d, 0x05)] {
            let (info, abbrev) = dwarf_unit(version, language, form);
            assert_eq!(
                first_unit_language(&info, &abbrev),
                Some(language),
                "DWARF {version}, form {form:#x}"
            );
        }
        let dir = TempDir::new("dwarf-language");
        let (info, abbrev) = dwarf_unit(5, DW_LANG_ZIG, 0x0b);
        let zig = dir.join("zig-app");
        std::fs::write(&zig, elf_with(&info, &abbrev)).unwrap();
        assert_eq!(dwarf_language(&zig), Some(DW_LANG_ZIG));
        let (info, abbrev) = dwarf_unit(4, 0x0c, 0x05);
        let c = dir.join("c-app");
        std::fs::write(&c, elf_with(&info, &abbrev)).unwrap();
        assert_eq!(dwarf_language(&c), Some(0x0c));
        // Not ELF, cut short, a unit naming an abbreviation that is not there.
        std::fs::write(dir.join("script"), "#!/bin/sh\n").unwrap();
        assert_eq!(dwarf_language(&dir.join("script")), None);
        let whole = elf_with(&info, &abbrev);
        std::fs::write(dir.join("cut"), &whole[..whole.len() - 70]).unwrap();
        assert_eq!(dwarf_language(&dir.join("cut")), None);
        assert_eq!(dwarf_language(&dir.join("missing")), None);
        let (mut info, abbrev) = dwarf_unit(5, 0x27, 0x0b);
        info[12] = 9;
        assert_eq!(first_unit_language(&info, &abbrev), None);
        assert_eq!(first_unit_language(&info[..7], &abbrev), None);
    }

    /// A `.zig` program is built for the session through LLVM — its cache
    /// and binary in Clew's data directory — and debugged as that binary; a
    /// Zig without LLVM builds it its own way, and that binary's debug
    /// information (Zig's own) brings the notice on why no variables show.
    #[test]
    #[cfg(unix)]
    fn a_zig_source_file_is_built_for_the_session() {
        with_store("zig-build", |data| {
            let dir = TempDir::new("debug-zig");
            let bin = dir.join("bin");
            script(&bin.join("lldb-dap"), "#!/bin/sh\n");
            let (info, abbrev) = dwarf_unit(4, 0x0c, 0x05);
            std::fs::write(dir.join("llvm.elf"), elf_with(&info, &abbrev)).unwrap();
            let (info, abbrev) = dwarf_unit(5, DW_LANG_ZIG, 0x0b);
            std::fs::write(dir.join("own.elf"), elf_with(&info, &abbrev)).unwrap();
            let log = dir.join("zig.log");
            script(
                &bin.join("zig"),
                &format!(
                    "#!/bin/sh\necho \"$@\" >> '{log}'\nfor a in \"$@\"; do case \"$a\" in -femit-bin=*) out=\"${{a#-femit-bin=}}\";; esac; done\ncase \"$*\" in *-fllvm*) if [ -f '{dir}/no-llvm' ]; then echo 'error: LLVM has not been enabled' >&2; exit 1; fi; /bin/cp '{dir}/llvm.elf' \"$out\";; *) /bin/cp '{dir}/own.elf' \"$out\";; esac\n",
                    log = log.display(),
                    dir = dir.display()
                ),
            );
            let _env = EnvVars::new().set("PATH", bin.as_os_str());
            std::fs::write(dir.join("app.zig"), "pub fn main() void {}\n").unwrap();
            let resolve_zig = || match resolve(
                "native",
                "app.zig",
                &["x".into()],
                &dir,
                &dir,
                &Default::default(),
                None,
            ) {
                Ok(Resolution::Ready(spec)) => spec,
                other => panic!("{other:?}"),
            };
            let spec = resolve_zig();
            let built = PathBuf::from(spec.launch["program"].as_str().unwrap());
            assert!(
                built.starts_with(data.join("debug-builds/zig")),
                "{built:?}"
            );
            assert_eq!(built.file_name().unwrap(), "app");
            assert!(built.is_file());
            assert_eq!(spec.launch["args"][0], "x");
            assert!(spec.launch.get(NOTICE).is_none(), "LLVM's: variables show");
            let ran = std::fs::read_to_string(&log).unwrap();
            assert!(
                ran.starts_with("build-exe -O Debug -fllvm --cache-dir ")
                    && ran.contains(&format!("-femit-bin={}", built.display()))
                    && ran
                        .trim_end()
                        .ends_with(&*dir.join("app.zig").to_string_lossy()),
                "{ran}"
            );
            assert!(
                !dir.join(".zig-cache").exists(),
                "nothing left in the project"
            );

            // A Zig without LLVM: built its own way, and the notice says why
            // no variables will show.
            std::fs::write(dir.join("no-llvm"), "").unwrap();
            let spec = resolve_zig();
            assert!(
                spec.launch[NOTICE].as_str().unwrap().contains("-fllvm"),
                "{}",
                spec.launch
            );
            // A prebuilt binary of Zig's own: debugged as it is, with the notice.
            let own = dir.join("own.elf").to_string_lossy().into_owned();
            let Ok(Resolution::Ready(spec)) =
                resolve("native", &own, &[], &dir, &dir, &Default::default(), None)
            else {
                panic!("a binary resolves");
            };
            assert_eq!(spec.launch["program"], own.as_str());
            assert!(spec.launch.get(NOTICE).is_some());
            // A build error is the error.
            script(
                &bin.join("zig"),
                "#!/bin/sh\necho 'app.zig:1:1: error: expected type' >&2\nexit 1\n",
            );
            let err = resolve(
                "native",
                "app.zig",
                &[],
                &dir,
                &dir,
                &Default::default(),
                None,
            )
            .unwrap_err();
            assert!(err.contains("expected type"), "{err}");
            // No zig at all.
            let _empty = EnvVars::new().set("PATH", dir.join("none").as_os_str());
            let _lldb = EnvVars::new().set("PATH", bin.as_os_str());
            std::fs::remove_file(bin.join("zig")).unwrap();
            let err = resolve(
                "native",
                "app.zig",
                &[],
                &dir,
                &dir,
                &Default::default(),
                None,
            )
            .unwrap_err();
            assert!(err.contains("zig not found"), "{err}");
        });
    }

    /// On a remote host Node runs under clew-server's own adapter, over its
    /// inspector — not js-debug, whose connection per process no stdio
    /// channel carries; with no clew-server to run it, that is said.
    #[test]
    #[cfg(unix)]
    fn a_remote_node_program_runs_under_clew_servers_own_adapter() {
        let dir = TempDir::new("debug-node-remote");
        let node = script(&dir.join("bin/node"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", dir.join("bin").as_os_str());
        let server = Path::new("/opt/clew/clew-server");
        let remote = resolve_stdio(
            "node",
            "/proj/main.js",
            &["x".into()],
            Path::new("/proj"),
            &Default::default(),
            Some(server),
        )
        .unwrap();
        assert_eq!(remote.command, server);
        assert_eq!(
            remote.args,
            ["node-debug", "--node", &*node.to_string_lossy()]
        );
        let launch: serde_json::Value = serde_json::from_str(&remote.launch).unwrap();
        assert_eq!(launch["program"], "/proj/main.js");
        assert_eq!(launch["args"][0], "x");
        assert_eq!(launch["cwd"], "/proj");
        let err = resolve_stdio(
            "node",
            "/proj/main.js",
            &[],
            Path::new("/proj"),
            &Default::default(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("clew-server"), "{err}");
        let _empty = EnvVars::new().set("PATH", dir.join("nothing").as_os_str());
        let err = resolve_stdio(
            "node",
            "/proj/main.js",
            &[],
            Path::new("/proj"),
            &Default::default(),
            Some(server),
        )
        .unwrap_err();
        assert!(err.contains("install Node.js"), "{err}");
    }

    #[test]
    fn unknown_adapters_are_refused_with_a_reason() {
        assert!(
            resolve_stdio(
                "cobol",
                "/p",
                &[],
                Path::new("/"),
                &Default::default(),
                None
            )
            .unwrap_err()
            .contains("no debug adapter")
        );
    }

    /// A remote TCP adapter runs behind clew-server's bridge, told where it
    /// announces its port; with no bridge it is refused, saying why.
    #[test]
    #[cfg(unix)]
    fn a_remote_tcp_adapter_runs_behind_the_bridge() {
        let dir = TempDir::new("debug-bridge");
        let bin = dir.join("bin");
        let rdbg = script(&bin.join("rdbg"), "#!/bin/sh\n");
        let dlv = script(&bin.join("dlv"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", bin.as_os_str());
        let bridge = Path::new("/opt/clew/clew-server");
        let ruby = resolve_stdio(
            "ruby",
            "app.rb",
            &[],
            &dir,
            &Default::default(),
            Some(bridge),
        )
        .unwrap();
        assert_eq!(ruby.command, bridge);
        assert_eq!(
            ruby.args[..5],
            [
                "dap-bridge",
                "--announce",
                "stderr",
                "--",
                &*rdbg.to_string_lossy()
            ]
        );
        assert!(
            ruby.args.ends_with(&["--".into(), "app.rb".into()]),
            "{:?}",
            ruby.args
        );
        let go = resolve_stdio(
            "go",
            "./cmd/app",
            &[],
            &dir,
            &Default::default(),
            Some(bridge),
        )
        .unwrap();
        assert_eq!(
            go.args[..5],
            [
                "dap-bridge",
                "--announce",
                "stdout",
                "--",
                &*dlv.to_string_lossy()
            ]
        );
        let err =
            resolve_stdio("ruby", "app.rb", &[], &dir, &Default::default(), None).unwrap_err();
        assert!(err.contains("TCP"), "{err}");
    }

    /// F3: consent is given to the prompt's text, so every argument pip runs
    /// with must be in it — including the pin, which used to be absent (an
    /// unpinned `pip install --user debugpy`, run with no prompt at all).
    #[test]
    fn the_pip_argv_is_what_the_consent_prompt_describes() {
        for user in [true, false] {
            let python = PathBuf::from("/opt/py/bin/python3");
            let install = AdapterInstall::Debugpy {
                python: python.clone(),
                user,
            };
            let describe = install.describe();
            let described: Vec<&str> = describe.split_whitespace().collect();
            let cmd = pip_command(&python, user);
            assert_eq!(cmd.get_program(), python.as_os_str());
            assert_eq!(described.first(), Some(&"/opt/py/bin/python3"));
            for arg in cmd.get_args() {
                let arg = arg.to_str().unwrap();
                assert!(
                    described.contains(&arg),
                    "`{arg}` runs but is not in the prompt"
                );
            }
            let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
            assert!(args.contains(&format!("debugpy=={DEBUGPY_VERSION}").as_str()));
            assert_eq!(args.contains(&"--user"), user);
            // Never run from the project: `-m pip` imports from the cwd.
            let neutral = if cfg!(unix) {
                PathBuf::from("/")
            } else {
                std::env::temp_dir()
            };
            assert_eq!(cmd.get_current_dir(), Some(neutral.as_path()));
        }
        let js = AdapterInstall::JsDebug.describe();
        assert!(
            js.contains(JS_DEBUG.url) && js.contains(&JS_DEBUG.sha256.to_hex()),
            "{js}"
        );
    }

    /// A fake interpreter: answers the probe from `probe_answer` (a shell
    /// expression printing "<has_debugpy> <in_venv>"), and records where it
    /// ran and with what. It runs nothing found on `PATH`: a child's `PATH`
    /// is taken once per process (`linerpc::spawn_path`), and a test that
    /// set it to an empty directory may have been the first to take it.
    #[cfg(unix)]
    fn fake_python(dir: &Path, probe_answer: &str) -> PathBuf {
        let log = dir.join("log");
        script(
            &dir.join("bin/python3"),
            &format!(
                "case \"$1\" in -c|-m) echo \"$(pwd) $PYTHONSAFEPATH $*\" >> '{log}' ;; esac\n\
                 case \"$1\" in\n\
                   -c) {probe_answer} ;;\n\
                   -m) : > '{mark}' ;;\n\
                 esac",
                log = log.display(),
                mark = dir.join("installed").display(),
            ),
        )
    }

    /// F3/F14: a missing debugpy is a consent request, not an install; the
    /// venv decides `--user`; the interpreter path is kept exactly as found.
    #[test]
    #[cfg(unix)]
    fn a_missing_debugpy_is_a_consent_request() {
        let dir = TempDir::new("dap-py");
        let python = fake_python(&dir, "echo '0 1'");
        let got = python_with(&python, "/p/main.py", &[], Path::new("/p")).unwrap();
        match got {
            Resolution::NeedsInstall(AdapterInstall::Debugpy { python: p, user }) => {
                assert_eq!(p, python, "the interpreter as found, not resolved");
                assert!(!user, "no --user inside a virtualenv");
            }
            other => panic!("expected a consent request, got {other:?}"),
        }
        assert!(
            !dir.join("installed").exists(),
            "resolving must not install"
        );
        // The probe ran from `/`, with the safe-path flag, never the project.
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert!(log.starts_with("/ 1 -c "), "{log}");
    }

    /// A module of a package runs with the folder its package sits in on
    /// the import path ("No module named 'teashop'" ran it as a lone
    /// script); one importing its siblings relatively runs as `-m`. A lone
    /// script, or a package's `__init__.py`, runs as it always did.
    #[test]
    #[cfg(unix)]
    fn a_package_module_runs_where_its_package_imports() {
        let dir = TempDir::new("dap-py-pkg");
        let python = fake_python(&dir, "echo '1 0'");
        let root = dir.join("proj");
        std::fs::create_dir_all(root.join("teashop/sub")).unwrap();
        std::fs::write(root.join("teashop/__init__.py"), "").unwrap();
        std::fs::write(root.join("teashop/sub/__init__.py"), "").unwrap();
        std::fs::write(
            root.join("teashop/pricing.py"),
            "from teashop.models import Order\n",
        )
        .unwrap();
        std::fs::write(
            root.join("teashop/sub/deep.py"),
            "from ..models import Order\n",
        )
        .unwrap();
        std::fs::write(root.join("main.py"), "import teashop\n").unwrap();
        let launch = |program: &str| {
            let Resolution::Ready(spec) = python_with(&python, program, &[], &root).unwrap() else {
                panic!("debugpy is present");
            };
            spec.launch
        };
        let absolute = launch("teashop/pricing.py");
        assert_eq!(absolute["program"], "teashop/pricing.py");
        let path = absolute["env"]["PYTHONPATH"].as_str().unwrap();
        assert_eq!(
            std::env::split_paths(path).next().as_deref(),
            Some(root.as_path()),
            "the package's folder first on the path"
        );
        let relative = launch(&root.join("teashop/sub/deep.py").to_string_lossy());
        assert!(relative.get("program").is_none(), "{relative}");
        assert_eq!(relative["module"], "teashop.sub.deep");
        let lone = launch("main.py");
        assert_eq!(lone["program"], "main.py");
        assert!(lone.get("env").is_none() && lone.get("module").is_none());
        assert_eq!(python_package(&root.join("teashop/__init__.py")), None);
    }

    #[test]
    #[cfg(unix)]
    fn a_present_debugpy_resolves_to_the_same_interpreter() {
        let dir = TempDir::new("dap-py-ok");
        let python = fake_python(&dir, "echo '1 0'");
        let Resolution::Ready(spec) =
            python_with(&python, "/p/main.py", &["--x".into()], Path::new("/p")).unwrap()
        else {
            panic!("debugpy is present");
        };
        assert_eq!(spec.command, python);
        assert_eq!(spec.args, ["-m", "debugpy.adapter"]);
        assert_eq!(spec.transport, AdapterTransport::Stdio);
        assert_eq!(spec.launch["python"][0], python.to_string_lossy().as_ref());
        assert_eq!(spec.launch["args"][0], "--x");
    }

    /// The consented install runs pip with the pin, then re-probes the same
    /// interpreter before reporting success.
    #[test]
    #[cfg(unix)]
    fn the_consented_install_runs_pip_and_confirms_it() {
        let dir = TempDir::new("dap-pip");
        let mark = dir.join("installed");
        let python = fake_python(
            &dir,
            &format!(
                "if [ -f '{}' ]; then echo '1 0'; else echo '0 0'; fi",
                mark.display()
            ),
        );
        let install = AdapterInstall::Debugpy {
            python: python.clone(),
            user: true,
        };
        install.install().unwrap();
        assert!(mark.exists());
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert!(
            log.contains(&format!("-m pip install --disable-pip-version-check --no-input --user debugpy=={DEBUGPY_VERSION}")),
            "{log}"
        );
    }

    /// A fake virtualenv in `dir/project`: `.venv/bin/python` links to a
    /// base interpreter outside the project (as `python -m venv` makes it)
    /// that has debugpy. Returns the project and the venv's interpreter.
    #[cfg(unix)]
    fn project_with_venv(dir: &Path) -> (PathBuf, PathBuf) {
        let base = fake_python(&dir.join("base"), "echo '1 1'");
        let project = dir.join("project");
        let venv_bin = project.join(".venv/bin");
        std::fs::create_dir_all(&venv_bin).unwrap();
        std::os::unix::fs::symlink(&base, venv_bin.join("python")).unwrap();
        (project, venv_bin.join("python"))
    }

    /// F10: a Python program is debugged under the project's virtualenv —
    /// the interpreter its language server is given — not whatever `python3`
    /// comes first on PATH, which lacks the project's packages. The venv's
    /// path is kept as found: its `bin/python` links to the base install.
    #[test]
    #[cfg(unix)]
    fn python_debugging_uses_the_project_venv() {
        // An activated virtualenv in the developer's shell is a candidate
        // too (`VIRTUAL_ENV`); this is about the project's and PATH's.
        let _env = EnvVars::new().remove("VIRTUAL_ENV");
        let dir = TempDir::new("dap-venv");
        // The project's venv, linked to a base interpreter outside it…
        let (project, venv_python) = project_with_venv(&dir);
        // …and an unrelated python3 first on PATH.
        let elsewhere = fake_python(&dir.join("elsewhere"), "echo '1 0'");
        let search = std::env::join_paths([elsewhere.parent().unwrap()]).unwrap();

        let chosen = python_for(&project, None, Some(&search)).unwrap();
        assert_eq!(chosen, venv_python, "the venv, as found");
        let Resolution::Ready(spec) = python_with(&chosen, "/p/main.py", &[], &project).unwrap()
        else {
            panic!("debugpy is present in the fake venv");
        };
        assert_eq!(spec.command, venv_python);
        assert_eq!(spec.launch["python"][0], chosen.to_string_lossy().as_ref());

        // Without a venv, PATH decides.
        let bare = dir.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(python_for(&bare, None, Some(&search)), Some(elsewhere));
    }

    /// The venv is looked for at the PROJECT ROOT, not in the session's
    /// working directory: a launch configuration that runs a script from a
    /// subdirectory used to be debugged under whatever `python3` came first
    /// on PATH. The working directory still goes into the launch body.
    #[test]
    #[cfg(unix)]
    fn a_subdirectory_cwd_still_debugs_under_the_project_venv() {
        // (The project's `.venv` is looked for before an activated
        // `VIRTUAL_ENV`, so the developer's shell cannot decide this one.)
        let dir = TempDir::new("dap-venv-cwd");
        let (project, venv_python) = project_with_venv(&dir);
        let cwd = project.join("tools");
        std::fs::create_dir_all(&cwd).unwrap();
        let program = cwd.join("run.py");
        let resolved = resolve(
            "python",
            &program.to_string_lossy(),
            &["--fast".into()],
            &project,
            &cwd,
            &Default::default(),
            None,
        )
        .unwrap();
        let Resolution::Ready(spec) = resolved else {
            panic!("debugpy is present in the project's venv: {resolved:?}");
        };
        assert_eq!(spec.command, venv_python, "not the project's interpreter");
        assert_eq!(spec.launch["cwd"], cwd.to_string_lossy().as_ref());
        assert_eq!(spec.launch["program"], program.to_string_lossy().as_ref());
        assert_eq!(spec.launch["args"][0], "--fast");
    }

    /// F10: a debugpy install stops when cancelled, with what it started,
    /// instead of running out its ten-minute limit.
    #[test]
    #[cfg(unix)]
    fn a_cancelled_debugpy_install_stops() {
        let dir = TempDir::new("dap-pip-cancel");
        let python = script(
            &dir.join("bin/python3"),
            "case \"$1\" in -m) /bin/sleep 60 ;; *) echo '0 0' ;; esac",
        );
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flip = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let started = std::time::Instant::now();
        let install = AdapterInstall::Debugpy { python, user: true };
        let err = install.install_cancellable(&cancel).unwrap_err();
        flip.join().unwrap();
        assert!(err.contains("cancelled"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A hung interpreter fails the probe instead of hanging the debugger's
    /// start (the probe had no timeout).
    #[test]
    #[cfg(unix)]
    fn a_hung_interpreter_fails_the_probe() {
        let dir = TempDir::new("dap-py-hang");
        let python = fake_python(&dir, "/bin/sleep 30");
        let started = std::time::Instant::now();
        let err = probe_python_within(&python, Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("did not finish"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// F3/F9/F10: js-debug is a consent request until installed (into a
    /// versioned directory of its own), and then listens on an OS-chosen
    /// port — no fixed fallback port.
    #[test]
    fn js_debug_needs_consent_then_listens_on_an_os_chosen_port() {
        with_store("js-debug", |dir| {
            let node = PathBuf::from("/usr/local/bin/node");
            assert!(matches!(
                node_with(node.clone(), "/p/app.js", &[], Path::new("/p")),
                Resolution::NeedsInstall(AdapterInstall::JsDebug)
            ));
            let entry = dir
                .join("debug-adapters/vscode-js-debug")
                .join(JS_DEBUG_VERSION)
                .join(JS_DEBUG.binary);
            std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
            std::fs::write(&entry, b"// server").unwrap();
            let Resolution::Ready(spec) = node_with(node, "/p/app.js", &[], Path::new("/p")) else {
                panic!("installed now");
            };
            assert_eq!(spec.transport, AdapterTransport::Tcp);
            assert_eq!(Path::new(&spec.args[0]), entry);
            assert_eq!(&spec.args[1..], ["0", "127.0.0.1"]);
        });
        let go = go_spec(PathBuf::from("/bin/dlv"), "/p", &[], Path::new("/p"), None);
        assert_eq!(go.args, ["dap", "--listen", "127.0.0.1:0"]);
        assert_eq!(go.transport, AdapterTransport::Tcp);
        assert_eq!(go.launch.get("buildFlags"), None);
    }

    /// The Go program is built with the tags the user chose for the
    /// project, as gopls reads it — and only tags that are tags.
    #[test]
    fn a_go_program_is_debugged_with_the_chosen_build_tags() {
        let go = clew_protocol::GoChoices {
            tags: vec!["integration".into(), "not a tag".into(), "e2e".into()],
            ..Default::default()
        };
        let flag = langenv::go_tags_flag(&go);
        let spec = go_spec(
            PathBuf::from("/bin/dlv"),
            "/p",
            &[],
            Path::new("/p"),
            flag.as_deref(),
        );
        assert_eq!(spec.launch["buildFlags"], "-tags=integration,e2e");
        assert_eq!(
            spec.launch["outputMode"], "remote",
            "its output comes as DAP output"
        );
    }

    /// A Dart program is debugged with the SDK chosen for its server — when
    /// that choice is where the toolchain is, not a program of its own — and
    /// a chosen SDK that is gone says so rather than falling back.
    #[test]
    #[cfg(unix)]
    fn a_dart_program_is_debugged_with_the_chosen_sdk() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testutil::TempDir::new("debug-dart-sdk");
        let dart = dir.join("flutter/bin/dart");
        std::fs::create_dir_all(dart.parent().unwrap()).unwrap();
        std::fs::write(&dart, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&dart, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sdk = |command: &Path, args: Option<Vec<String>>| clew_protocol::LanguageChoices {
            servers: Some(
                [(
                    "dart".to_string(),
                    clew_protocol::ServerChoice {
                        command: command.to_string_lossy().into_owned(),
                        args,
                    },
                )]
                .into(),
            ),
            ..Default::default()
        };
        let resolved = resolve(
            "dart",
            "/p/main.dart",
            &[],
            &dir,
            &dir,
            &sdk(&dart, None),
            None,
        );
        let Ok(Resolution::Ready(spec)) = resolved else {
            panic!("the chosen SDK is used: {resolved:?}");
        };
        assert_eq!(spec.command, dart);
        assert_eq!(spec.args, ["debug_adapter"]);
        let gone = resolve(
            "dart",
            "/p/main.dart",
            &[],
            &dir,
            &dir,
            &sdk(&dir.join("gone/dart"), None),
            None,
        )
        .unwrap_err();
        assert!(gone.contains("the Dart SDK you chose"), "{gone}");
        // A server program of the user's own (with its own arguments) is
        // not an SDK: the debugger finds Dart as it would otherwise.
        assert_eq!(
            chosen_sdk(&sdk(&dart, Some(vec!["--lsp".into()])), "dart"),
            None
        );
    }

    /// A Ruby program runs under `rdbg`, which serves DAP on a port it picks
    /// and names on stderr: the `rdbg` beside the Ruby LSP the user chose,
    /// else the PATH's. A C# assembly is debugged by netcoredbg over stdio.
    #[test]
    #[cfg(unix)]
    fn ruby_runs_under_rdbg_and_csharp_under_netcoredbg() {
        let dir = TempDir::new("debug-ruby-cs");
        let path_bin = dir.join("path-bin");
        let on_path = script(&path_bin.join("rdbg"), "#!/bin/sh\n");
        script(&path_bin.join("netcoredbg"), "#!/bin/sh\n");
        let chosen_bin = dir.join("rubies/3.3.0/bin");
        script(&chosen_bin.join("ruby-lsp"), "#!/bin/sh\n");
        let beside = script(&chosen_bin.join("rdbg"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", path_bin.as_os_str());
        let ruby_lsp = |command: &Path| clew_protocol::LanguageChoices {
            servers: Some(
                [(
                    "ruby".to_string(),
                    clew_protocol::ServerChoice {
                        command: command.to_string_lossy().into_owned(),
                        args: None,
                    },
                )]
                .into(),
            ),
            ..Default::default()
        };
        let ready =
            |lang: &str, program: &str, choices: &clew_protocol::LanguageChoices| match resolve(
                lang,
                program,
                &["a".into()],
                &dir,
                &dir,
                choices,
                None,
            ) {
                Ok(Resolution::Ready(spec)) => spec,
                other => panic!("{lang}: {other:?}"),
            };

        let spec = ready("ruby", "app.rb", &Default::default());
        assert_eq!(spec.command, on_path);
        assert_eq!(
            spec.args,
            [
                "--open",
                "--host",
                "127.0.0.1",
                "--port",
                "0",
                "--stop-at-load",
                "--",
                "app.rb",
                "a"
            ]
        );
        assert_eq!(spec.transport, AdapterTransport::TcpOnStderr);
        assert_eq!(spec.launch["request"], "launch");
        let spec = ready("ruby", "app.rb", &ruby_lsp(&chosen_bin.join("ruby-lsp")));
        assert_eq!(spec.command, beside, "the chosen Ruby's own rdbg");
        // A chosen Ruby LSP with no rdbg beside it: the PATH's.
        let spec = ready("ruby", "app.rb", &ruby_lsp(&dir.join("other/ruby-lsp")));
        assert_eq!(spec.command, on_path);

        let spec = ready("csharp", "bin/Debug/net8.0/App.dll", &Default::default());
        assert_eq!(spec.args, ["--interpreter=vscode"]);
        assert_eq!(spec.transport, AdapterTransport::Stdio);
        assert_eq!(spec.launch["program"], "bin/Debug/net8.0/App.dll");
        assert_eq!(spec.launch["stopAtEntry"], false);
        let remote =
            resolve_stdio("csharp", "App.dll", &[], &dir, &Default::default(), None).unwrap();
        assert_eq!(remote.args, ["--interpreter=vscode"]);

        let _none = EnvVars::new().set("PATH", dir.join("empty").as_os_str());
        let err =
            resolve("ruby", "app.rb", &[], &dir, &dir, &Default::default(), None).unwrap_err();
        assert!(err.contains("gem install debug"), "{err}");
        let err = resolve(
            "csharp",
            "App.dll",
            &[],
            &dir,
            &dir,
            &Default::default(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("netcoredbg"), "{err}");
    }

    /// A PHP program is debugged by clew-server's own adapter, given the
    /// PATH's `php`; with no clew-server to run it, that is said.
    #[test]
    #[cfg(unix)]
    fn php_runs_under_clew_servers_own_adapter() {
        let dir = TempDir::new("debug-php");
        let php = script(&dir.join("bin/php"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", dir.join("bin").as_os_str());
        let server = Path::new("/opt/clew/clew-server");
        let resolved = resolve(
            "php",
            "app.php",
            &["x".into()],
            &dir,
            &dir,
            &Default::default(),
            Some(server),
        );
        let Ok(Resolution::Ready(spec)) = resolved else {
            panic!("{resolved:?}");
        };
        assert_eq!(spec.command, server);
        assert_eq!(spec.args, ["php-debug", "--php", &*php.to_string_lossy()]);
        assert_eq!(spec.transport, AdapterTransport::Stdio);
        assert_eq!(spec.launch["program"], "app.php");
        assert_eq!(spec.launch["args"][0], "x");
        let err =
            resolve("php", "app.php", &[], &dir, &dir, &Default::default(), None).unwrap_err();
        assert!(err.contains("clew-server"), "{err}");
        // Remotely, the server is its own.
        let remote = resolve_stdio(
            "php",
            "app.php",
            &[],
            &dir,
            &Default::default(),
            Some(server),
        )
        .unwrap();
        assert_eq!(remote.args[0], "php-debug");
    }

    /// A Java program is debugged by clew-server's own adapter, with the JDK
    /// chosen for the project (Settings → Language servers), else the PATH's.
    #[test]
    #[cfg(unix)]
    fn java_runs_under_clew_servers_own_adapter_with_the_chosen_jdk() {
        let dir = TempDir::new("debug-java");
        let java = script(&dir.join("bin/java"), "#!/bin/sh\n");
        let javac = script(&dir.join("bin/javac"), "#!/bin/sh\n");
        let jdk = dir.join("jdks/temurin-21");
        let chosen_java = script(&jdk.join("bin/java"), "#!/bin/sh\n");
        let chosen_javac = script(&jdk.join("bin/javac"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", dir.join("bin").as_os_str());
        let server = Path::new("/opt/clew/clew-server");
        let ready = |choices: &clew_protocol::LanguageChoices| match resolve(
            "java",
            "/p/Main.java",
            &[],
            &dir,
            &dir,
            choices,
            Some(server),
        ) {
            Ok(Resolution::Ready(spec)) => spec,
            other => panic!("{other:?}"),
        };
        let spec = ready(&Default::default());
        assert_eq!(
            spec.args,
            [
                "java-debug",
                "--java",
                &*java.to_string_lossy(),
                "--javac",
                &*javac.to_string_lossy()
            ]
        );
        let chosen = clew_protocol::LanguageChoices {
            java: Some(jdk.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let spec = ready(&chosen);
        assert_eq!(spec.args[2], chosen_java.to_string_lossy());
        assert_eq!(spec.args[4], chosen_javac.to_string_lossy());
        let err = resolve(
            "java",
            "/p/Main.java",
            &[],
            &dir,
            &dir,
            &Default::default(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("clew-server"), "{err}");
    }

    /// Erlang and Haskell programs are debugged by clew-server's own
    /// adapters, given the PATH's `erl`/`erlc` and `ghci` — Haskell's the
    /// `ghci` beside the language server the user chose, when there is one.
    #[test]
    #[cfg(unix)]
    fn erlang_and_haskell_run_under_clew_servers_own_adapters() {
        let dir = TempDir::new("debug-erl-hs");
        let erl = script(&dir.join("bin/erl"), "#!/bin/sh\n");
        let erlc = script(&dir.join("bin/erlc"), "#!/bin/sh\n");
        let ghci = script(&dir.join("bin/ghci"), "#!/bin/sh\n");
        script(
            &dir.join("ghcup/bin/haskell-language-server-wrapper"),
            "#!/bin/sh\n",
        );
        let chosen_ghci = script(&dir.join("ghcup/bin/ghci"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", dir.join("bin").as_os_str());
        let server = Path::new("/opt/clew/clew-server");
        let ready = |lang: &str, choices: &clew_protocol::LanguageChoices| match resolve(
            lang,
            "/p/main",
            &["a".into()],
            &dir,
            &dir,
            choices,
            Some(server),
        ) {
            Ok(Resolution::Ready(spec)) => spec,
            other => panic!("{lang}: {other:?}"),
        };
        let spec = ready("erlang", &Default::default());
        assert_eq!(spec.command, server);
        assert_eq!(
            spec.args,
            [
                "erlang-debug",
                "--erl",
                &*erl.to_string_lossy(),
                "--erlc",
                &*erlc.to_string_lossy()
            ]
        );
        assert_eq!(spec.launch["args"][0], "a");
        let spec = ready("haskell", &Default::default());
        assert_eq!(
            spec.args,
            ["haskell-debug", "--ghci", &*ghci.to_string_lossy()]
        );
        let chosen = clew_protocol::LanguageChoices {
            servers: Some(
                [(
                    "haskell".to_string(),
                    clew_protocol::ServerChoice {
                        command: dir
                            .join("ghcup/bin/haskell-language-server-wrapper")
                            .to_string_lossy()
                            .into_owned(),
                        args: None,
                    },
                )]
                .into(),
            ),
            ..Default::default()
        };
        let spec = ready("haskell", &chosen);
        assert_eq!(
            spec.args[2],
            chosen_ghci.to_string_lossy(),
            "the chosen GHC's ghci"
        );
        for lang in ["erlang", "haskell"] {
            let err =
                resolve(lang, "/p/main", &[], &dir, &dir, &Default::default(), None).unwrap_err();
            assert!(err.contains("clew-server"), "{err}");
        }
        let _none = EnvVars::new().set("PATH", dir.join("empty").as_os_str());
        let err = resolve(
            "erlang",
            "/p/main",
            &[],
            &dir,
            &dir,
            &Default::default(),
            Some(server),
        )
        .unwrap_err();
        assert!(err.contains("Erlang/OTP"), "{err}");
    }

    /// LLVM's adapter is found by its plain name, a distribution's versioned
    /// one (the newest), or its name before LLVM 18.
    #[test]
    #[cfg(unix)]
    fn lldb_dap_is_found_by_any_of_its_names() {
        let dir = TempDir::new("debug-lldb-names");
        let native = |path: &Path| {
            let _env = EnvVars::new().set("PATH", path.as_os_str());
            match resolve(
                "native",
                "/p/app",
                &[],
                &dir,
                &dir,
                &Default::default(),
                None,
            ) {
                Ok(Resolution::Ready(spec)) => Some(spec.command),
                _ => None,
            }
        };
        let versioned = dir.join("versioned");
        script(&versioned.join("lldb-dap-17"), "#!/bin/sh\n");
        let newest = script(&versioned.join("lldb-dap-18"), "#!/bin/sh\n");
        assert_eq!(native(&versioned), Some(newest));
        let old = dir.join("old");
        let vscode = script(&old.join("lldb-vscode"), "#!/bin/sh\n");
        assert_eq!(native(&old), Some(vscode));
        let plain = dir.join("plain");
        let dap = script(&plain.join("lldb-dap"), "#!/bin/sh\n");
        script(&plain.join("lldb-dap-18"), "#!/bin/sh\n");
        assert_eq!(native(&plain), Some(dap), "the plain name first");
    }

    /// The js-debug download goes through the store's verified, staged
    /// installer — refused outside the data dir before any network access.
    #[test]
    fn the_js_debug_download_is_confined_to_the_data_dir() {
        with_store("js-debug-confined", |dir| {
            let outside = dir.join("elsewhere");
            let err = store::install_download(
                &JS_DEBUG,
                &outside,
                &dir.join("debug-adapters"),
                &AtomicBool::new(false),
            )
            .unwrap_err();
            assert!(err.contains("outside the store"), "{err}");
        });
    }
}
