# Formal verification

**English** · [简体中文](verification.zh-CN.md)

Some of Clew's rules are proved, not only tested. For each rule, `verification/` holds a Lean 4 model, theorems proved about that model, and test cases that tie the model to the Rust code. The rules chosen are the ones Clew's safety rests on, and the ones that took the most review rounds to get right:
- which paths a project may name;
- which updates may be installed;
- which server binary may be deployed to a remote;
- what a recorded approval covers;
- how messages are framed on a stream;
- what Clew hands to a shell or to ssh;
- which programs Clew runs where they are, and which headers it sends;
- where spoken words go.

## What is proved

**Path confinement** — `check_lexical` in `crates/clew-core/src/confine.rs`, modelled in `verification/Clew/Confine.lean`.
- A path from outside (a client's request, a tool argument a model wrote) that the check accepts never leads above the project root. This holds when the path is resolved against any root, and at every step of the resolution, not only at the end.
- The check refuses exactly an empty path, an absolute one, and one with `..` in it, and nothing else.

**Updates never go backwards** — `Version::parse` and `ensure_upgrade` in `crates/clew-core/src/update.rs`, modelled in `Update.lean`.
- An update is installed only if it is strictly newer than the version running.
- The version order is a strict total order. So a run of updates only climbs, and no version is ever installed twice, the running one included.
- The version Clew displays and the `vX.Y.Z` tag it publishes both read back as that version.

**Which server binary may be deployed** — `slug`, `asset_name`, `parse_digest_list`, `embedded_digest`, `parse_sha256` and `verify` in `crates/clew-core/src/server_dist.rs`, modelled in `ServerDist.lean`.
- The platform a remote reports becomes a plain token: no `/`, no `.`, nothing a URL treats as syntax. This holds whatever the remote said, and whatever Unicode lower-casing does to it.
- Two different platform-and-protocol pairs never share an asset name, so they never share the digest pinned under it.
- In a release build, a binary is deployed only if it hashes to the digest pinned for its own asset. The `.sha256` file next to the download has no say. An asset the release does not list is never deployed.
- The digest list the release assembles from its `sha256sum` lines parses to exactly its entries.

**An approval covers what was approved and nothing else** — `args_part`, the length-prefixed fields every fingerprint hashes, and the key an approval is filed under: `crates/clew-core/src/trust.rs`, `project_key` in `crates/clew-core/src/derived.rs` and `scope_label` in `src/backend/connect.rs`, modelled in `Trust.lean`.
- No two argument lists read alike.
- No two field lists read alike, including with the command's content digest after them.
- An approval of a command, of options alone, and leave to build hash disjoint inputs. So one kind of approval is never taken for another.
- A remote approval's key names one user, one host, one port and one project, whatever the project's path, and never equals a local project's key.

Proving the last of these found two holes:
- A host may contain `:` (an IPv6 address). So `u@2001:db8::1:2222` on port 22 and `u@2001:db8::1` on port 2222 were given the same label, and an approval for one machine covered the other. A host containing `:` is now bracketed (`[2001:db8::1]:2222`), and `%`, `/`, `[`, `]` and `@` are escaped.
- A project path on a remote may be relative (`clew proj` with `CLEW_SSH` set). So `2222:/srv/p` on `u@h` and `/srv/p` on `u@h:2222` had the same key. A relative path is now written after `./`.

An ordinary `user@host` and an absolute path keep the key they had, so approvals already given still apply.

The user, host and port in a key are the ones ssh connects with. With `CLEW_SSH`, they are read as OpenSSH reads its arguments: from `-l`, `-o User=`, `-p`, `-o Port=`, `user@host` or an `ssh://user@host:port` destination, each the first one given. A port is kept as written, so `-p http` is never taken for port 22. A path that is not UTF-8 keeps its bytes in the key, so two such paths are never one project.

**Message framing** — `read_frame`, `read_json_line`, `read_line_capped` and `encode` in `crates/clew-core/src/framing.rs`, modelled byte for byte in `Framing.lean`.
- A frame Clew writes is read back as exactly its body, whatever follows it, and the stream is left at the next frame. So any number of frames written one after another read back as exactly those messages, and the stream never falls out of step.
- A clean end of the stream is no frame, not an error.
- A body is never longer than the cap, whatever its header claimed.
- Every frame read consumes input, so the reader always makes progress.
- The cap on Clew's own protocol applies to a line's content, not its newline. A line of text exactly at the cap is read; a longer one is refused, never read in part, and so is one that is not UTF-8.

**What Clew hands to a shell or to ssh** — `shell_command` and `sh_c` in `src/backend/server.rs`, `quote` in `src/macos/install.rs`, and `ssh_args`, `ssh_endpoint`, `known_hosts_pattern` and `plain_host_field` in `src/backend/connect.rs`, modelled in `Ssh.lean`.
- A command Clew writes for a shell — one it shows you to paste, every path in the macOS updater's swap script — is read by a POSIX shell as exactly the words meant. This holds whatever the words hold: spaces, quotes, `$`, backticks, newlines.
- A command Clew runs on a remote, `sh -c '<script>'`, is read as `sh`, `-c` and the script.
- No field of a saved connection ever reaches ssh as an option, whatever it holds. ssh reads exactly the options Clew wrote, and the destination exactly `user@host`. So a host typed as `-oProxyCommand=…` is a host, not a command.
- A machine has one label, whether it is reached as a saved connection or through `CLEW_SSH` with the same arguments, for any user name that is not itself an `ssh://` address. So approvals given through either are filed under one name.
- The known_hosts field Clew writes a host key under names one host and one port. It holds none of the pattern syntax (`*`, `?`, `!`, `,`, a hashed `|1|` name) that would let one key vouch for other hosts.

**Two guards** — `trusted_by` in `crates/clew-core/src/trust.rs` and `check_headers` in `crates/clew-core/src/mcp/config.rs`, modelled in `Guards.lean`.
- A language server a project names runs from Clew's own copy of the approved bytes, unless it sits where nobody but you or root can change it. When `trusted_by` lets it run in place, no other user can write it or replace any directory entry on the way to it. This uses the Unix permission rules, and counts every user as possibly in the file's group.
- The headers `check_headers` accepts for an MCP server are read back from the request as exactly those headers, one line each, under their own names. This holds however leniently the server splits lines: at a CR, an LF or a NUL. No value can start a header of its own.

**Where spoken words go** — `route` in `src/app/voice.rs`, modelled in `VoiceRoute.lean`.
- "Bye, Clew" is always heard first.
- A word that stops Clew is never sent to Ask. If the question waiting offers an answer by that name, the word is that answer.
- A stop word is never taken for a yes either: "okay, stop" begins with a yes.
- A command waiting for a yes runs only on a yes.
- What is said to a question that is waiting is never sent to Ask.
- While an answer is being written, a stop word stops it before any command is tried. The exceptions: a command waiting for a yes, which the stop word declines, and a question waiting with an answer by that name.
- Every route ends in exactly one step that acts. It never takes a step whose subject isn't there, such as a waiting command or a waiting question.

## How the proofs reach the code

A theorem is about a model. What connects each model to the code:

- **The model reads like the code.** It takes the same steps in the same order. Every Rust standard-library function it relies on, down to what `char::is_whitespace` means, is spelled out in `Text.lean`. Those are tested through the functions that use them: every `White_Space` code point and its neighbours are among the test inputs.
- **Test cases.** `lake exe vectors` runs each model on a fixed set of inputs and writes the answers to `verification/vectors/*.json`. The inputs are every short input over an alphabet chosen to reach every branch, plus the edge cases each file names. `cargo test` runs the real Rust functions on the same inputs and requires the same answers. If the code and the model ever disagree, a test fails.
- **The shell model is checked against a real shell.** Every shell test case is also run through `/bin/sh`, which must read it back as the same words.
- **For the voice routing, the inputs are every possible input**: all 1,152 combinations of facts. The Rust `route` is therefore the model, and the theorems hold of it outright. For the other rules the inputs are a large sample, not all possible inputs.

`verification/check.sh` checks three things:
- every proof checks;
- each proof rests on Lean's three standard axioms and nothing else (`Audit.lean`): no `sorry`, and no `Lean.ofReduceBool`, which trusts compiled code the kernel didn't check;
- the committed test cases are exactly what the models compute today.

CI runs it in the `verification` job. The Lean toolchain is pinned in `verification/lean-toolchain`, and CI checks the release's SHA-256.

## What the proofs rest on

- Lean's kernel and its three standard axioms.
- SHA-256 being collision resistant. The theorems about approvals and server digests are about the bytes that are hashed. That different bytes give different hashes is SHA-256's own promise.
- What the operating system does with a path. Symlinks and canonicalization are not modelled. `confine` checks the resolved path again at run time, against the canonical root.
- The test cases. Outside the voice routing they are a sample of inputs, not all of them.
- A POSIX shell. A login shell that is not one (fish, csh) is outside the model; the scripts Clew runs remotely avoid what those read differently, and a test checks that they do.
- What ssh's own configuration does. A key's user, host and port are the ones on ssh's command line. What `~/.ssh/config`, `-F` or `-o HostName=` maps them to is not read: a `Host` alias is known by its name.

## Every decision, and how it is checked

These are the places where Clew decides who may do what, or what to trust. Each is proved where a rule can be stated and modelled. Otherwise the table says what it rests on.

| Decision | Code | How it is checked |
|---|---|---|
| Which paths a request or a tool may name | `confine::check_lexical` | Proved |
| That a resolved path is still inside the project | `confine::confine` | Tested: symlinks and canonical paths are the file system's |
| Whether an update is newer | `update::ensure_upgrade` | Proved |
| Where an update may be downloaded from | `updater::validate_dmg_url` | Tested: it rests on the `url` crate's URL parser |
| Whether a downloaded update is genuine (macOS) | `macos::install::verify` | Rests on Apple's code signing, which Clew calls |
| Which server binary may be deployed | `server_dist` | Proved |
| What an approval covers | `trust` fingerprints | Proved |
| Which machine and project an approval is filed under | `derived::project_key`, `scope_label`, `ssh_endpoint` | Proved |
| Whether a project's language server may run | `lsp_gate::lsp_command_allowed` | The rule, that its fingerprint was approved, rests on the proved fingerprints. Running exactly the bytes that were hashed is file-system work, tested |
| Whether a folder may hold a program Clew runs unstaged | `trust::trusted_by` | Proved. Reading the owners and modes it decides from is the file system's |
| How messages are framed | `framing` | Proved |
| What Clew hands to a shell | `shell_command`, `sh_c`, `quote` | Proved, and checked against `/bin/sh` |
| What Clew hands to ssh | `SavedConnection::ssh_args` | Proved |
| Which host a recorded host key vouches for | `known_hosts_pattern`, `plain_host_field` | Proved |
| Where spoken words go | `voice::route` | Proved |
| Whether hovering may run the debugged program's code | `dap::hover_eval_allowed` | Tested: one `or` of the debugger's promise and your choice |
| An MCP server's request headers | `mcp::config::check_headers` | Proved |
| Signing in to an MCP server | `mcp` OAuth | Tested: a network protocol |

## Why the models are written by hand

Aeneas, with Charon, translates Rust into Lean, so a model would be the code itself rather than a copy of it. It was considered and not used, for two reasons:
- Its Lean library depends on Mathlib. The check would take tens of minutes and gigabytes, where it now takes seconds.
- The functions here lean on parts of Rust that Aeneas models only in part: `str` methods, iterators and closures, and async I/O.

So each model is written step for step after the code, and the test cases tie the two together. If Aeneas comes to cover these, an extracted model can replace a hand-written one, and the theorems stay as they are.

## What is not verified, and why

- **Drawing**: layout, rendering, animation. What they should produce is pixels, not a statement a proof can be about.
- **Language heuristics**: which words count as "stop" in which language, what Whisper writes, whether a sound was the wake phrase. These come from models and statistics, not rules with a precise statement. What is verified in voice mode is where words go once they have been classified.
- **Concurrency and the outside world**: task scheduling, timeouts, child processes, the network, the file system.
- **Answers from a language model.**

## Running it

```sh
cd verification
lake build                  # the proofs
./check.sh                  # the proofs, what they rest on, and that the test cases are current
lake exe vectors vectors    # rewrite the test cases after changing a model
```

[elan](https://github.com/leanprover/elan) installs the Lean version `lean-toolchain` names. The project needs nothing beyond Lean itself.

## Adding a rule

1. Model the function in `verification/Clew/<Area>.lean`, step for step, and prove what it should guarantee.
2. Add its test cases to `Vectors.lean`, run `lake exe vectors vectors`, and commit the JSON file.
3. In the Rust code, add a test that runs the real function on them with `clew_core::testutil::lean_cases`.
4. List the new theorems in `Audit.lean`, then run `./check.sh`.
