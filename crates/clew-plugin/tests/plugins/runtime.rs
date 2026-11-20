//! The runtime, running the test plugins in `fixtures/`: what each kind of
//! answer becomes, what is refused, and what stops a call.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clew_plugin::runtime::{
    BlameLine, Block, Commit, Failure, Hit, Host, Invocation, Level, Limits, Loaded, Outcome,
    ProjectInfo, Query, Runtime, Symbol,
};
use clew_rules::plugin::{GIT, READ};

/// A project of two files, and a log of what the plugin wrote.
#[derive(Default)]
struct FakeHost {
    log: Mutex<Vec<(Level, String)>>,
    reads: Mutex<Vec<String>>,
    /// The user asks the run to stop this long after it starts.
    stop_after: Option<(Instant, Duration)>,
}

impl Host for FakeHost {
    fn info(&self) -> ProjectInfo {
        ProjectInfo {
            name: "demo".into(),
            files: 2,
            partial: false,
            remote: false,
        }
    }
    fn files(&self) -> Vec<String> {
        vec!["src/main.rs".into(), "README.md".into()]
    }
    fn read(&self, path: &str) -> Result<String, String> {
        self.reads.lock().unwrap().push(path.to_string());
        match path {
            "src/main.rs" => Ok("fn main() {\n    println!(\"hi\");\n}\n".into()),
            _ => Err(format!("no file {path}")),
        }
    }
    fn outline(&self, _path: &str) -> Result<Vec<Symbol>, String> {
        Ok(Vec::new())
    }
    fn search(&self, _query: &Query) -> Result<Vec<Hit>, String> {
        Ok(Vec::new())
    }
    fn git_log(&self, _path: Option<&str>, _limit: u32) -> Result<Vec<Commit>, String> {
        Ok(vec![Commit {
            id: "abc".into(),
            author: "Ada".into(),
            time: 0,
            subject: "First".into(),
        }])
    }
    fn git_blame(&self, _path: &str) -> Result<Vec<BlameLine>, String> {
        Ok(Vec::new())
    }
    fn log(&self, level: Level, message: &str) {
        self.log.lock().unwrap().push((level, message.to_string()));
    }
    fn stop_asked(&self) -> bool {
        self.stop_after
            .is_some_and(|(started, after)| started.elapsed() >= after)
    }
}

fn fixture(name: &str) -> (Vec<u8>, clew_plugin::Inspected) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(format!("{name}.wasm"));
    let bytes = std::fs::read(path).unwrap();
    let inspected = clew_plugin::inspect(&bytes).unwrap();
    (bytes, inspected)
}

fn data(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("clew-plugin-runtime-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn views(runtime: &Runtime) -> Loaded {
    let (bytes, inspected) = fixture("views");
    runtime.load(&bytes, &inspected, READ).unwrap()
}

fn at(file: Option<&str>, line: Option<u32>) -> Invocation {
    Invocation {
        file: file.map(Into::into),
        line,
        selection: None,
        input: None,
    }
}

fn quick() -> Limits {
    Limits {
        deadline: Duration::from_secs(2),
        memory: 64 * 1024 * 1024,
        answer: Limits::COMMAND.answer,
    }
}

/// A plugin's answer is copied out of its memory only up to
/// `Limits::answer`: past it the run fails, said so, rather than Clew
/// copying whatever the plugin names. Its strings may all point at the same
/// bytes, so one within its own memory limit could name gigabytes.
#[test]
fn an_answer_larger_than_clew_takes_fails_and_says_so() {
    let runtime = Runtime::new(None).unwrap();
    let loaded = views(&runtime);
    let small = Limits {
        answer: 64,
        ..quick()
    };
    let failed = loaded
        .run(
            Arc::new(FakeHost::default()),
            "all-blocks",
            &at(Some("src/main.rs"), Some(1)),
            small,
        )
        .unwrap_err();
    assert_eq!(
        failed,
        Failure::Stopped(clew_plugin::runtime::ANSWER_TOO_LARGE.into())
    );
    // The same answer, within the limit, is taken.
    assert!(
        loaded
            .run(
                Arc::new(FakeHost::default()),
                "all-blocks",
                &at(Some("src/main.rs"), Some(1)),
                quick(),
            )
            .is_ok()
    );
}

#[test]
fn permissions_are_read_from_the_code() {
    let (_, views) = fixture("views");
    assert_eq!(views.needs, READ);
    assert_eq!(views.permissions(), ["read"]);
    assert_eq!(views.manifest.commands.len(), 13);
    let (_, git) = fixture("git-log");
    assert_eq!(git.needs, GIT);
    assert!(
        !git.imports
            .iter()
            .any(|i| i.starts_with("clew:plugin/project")),
        "a plugin that never reads files does not import project: {:?}",
        git.imports
    );
}

#[test]
fn a_plugin_is_not_linked_without_what_it_needs() {
    let runtime = Runtime::new(None).unwrap();
    let (bytes, inspected) = fixture("git-log");
    let refused = runtime.load(&bytes, &inspected, READ).err().unwrap();
    assert!(refused.contains("git"), "{refused}");
    let loaded = runtime.load(&bytes, &inspected, GIT).unwrap();
    let said = loaded
        .run(
            Arc::new(FakeHost::default()),
            "latest",
            &at(None, None),
            quick(),
        )
        .unwrap();
    assert_eq!(said, Outcome::Say("First by Ada".into()));
}

#[test]
fn a_view_comes_back_checked() {
    let runtime = Runtime::new(None).unwrap();
    let loaded = views(&runtime);
    let Outcome::Show(view) = loaded
        .run(
            Arc::new(FakeHost::default()),
            "all-blocks",
            &at(None, None),
            quick(),
        )
        .unwrap()
    else {
        panic!("not a view");
    };
    assert_eq!(view.title, "Every block");
    assert_eq!(view.blocks.len(), 8);
    let Block::Items(items) = &view.blocks[3] else {
        panic!()
    };
    assert_eq!(items[0].at.as_ref().unwrap().path, "src/lib.rs");
    assert!(
        items[1].at.is_none(),
        "a place outside the project is dropped"
    );
}

#[test]
fn each_kind_of_answer() {
    let runtime = Runtime::new(None).unwrap();
    let loaded = views(&runtime);
    let host = Arc::new(FakeHost::default());
    let run = |command: &str| {
        loaded.run(
            host.clone(),
            command,
            &at(Some("src/main.rs"), Some(1)),
            quick(),
        )
    };
    assert!(
        matches!(run("open").unwrap(), Outcome::Open(l) if l.path == "src/main.rs" && l.line == 7)
    );
    assert_eq!(
        run("say").unwrap(),
        Outcome::Say("Said in two lines".into())
    );
    assert_eq!(
        run("refuse").unwrap_err(),
        Failure::Plugin("I will not".into())
    );
    assert_eq!(
        run("read").unwrap(),
        Outcome::Say("src/main.rs has 3 lines, 2 files in demo".into())
    );
    assert!(
        host.log
            .lock()
            .unwrap()
            .iter()
            .any(|(level, m)| *level == Level::Info && m == "read src/main.rs")
    );
    let escape = run("escape").unwrap_err();
    assert!(
        matches!(&escape, Failure::Plugin(why) if why.contains("not a path in the project")),
        "{escape:?}"
    );
    assert!(
        !host.reads.lock().unwrap().iter().any(|p| p.contains("..")),
        "the host never saw a path out of the project"
    );
}

/// A plugin that sleeps waits in the host, where the deadline does not
/// reach: it was never stopped, and held its run — and every run after
/// it — for good. Its waits end at the deadline, which it has then used
/// up; a short one is slept whole.
#[test]
fn a_call_that_sleeps_is_stopped_at_its_deadline() {
    let runtime = Runtime::new(None).unwrap();
    let loaded = views(&runtime);
    let limits = Limits {
        deadline: Duration::from_millis(300),
        memory: 64 * 1024 * 1024,
        answer: Limits::COMMAND.answer,
    };
    let started = Instant::now();
    let stopped = loaded
        .run(
            Arc::new(FakeHost::default()),
            "sleep",
            &at(None, None),
            limits,
        )
        .unwrap_err();
    assert!(matches!(&stopped, Failure::TimedOut(_)), "{stopped:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(
        matches!(
            loaded.run(Arc::new(FakeHost::default()), "nap", &at(None, None), limits),
            Ok(Outcome::Say(ref slept)) if slept == "true"
        ),
        "a short sleep was cut"
    );
}

#[test]
fn a_run_the_user_stops_ends_at_once_asleep_or_busy() {
    let runtime = Runtime::new(None).unwrap();
    let loaded = views(&runtime);
    for (command, line) in [("sleep", None), ("spin", Some(1))] {
        let host = FakeHost {
            stop_after: Some((Instant::now(), Duration::from_millis(200))),
            ..FakeHost::default()
        };
        let started = Instant::now();
        let stopped = loaded
            .run(Arc::new(host), command, &at(None, line), Limits::COMMAND)
            .unwrap_err();
        assert_eq!(
            stopped,
            Failure::Stopped(clew_plugin::runtime::STOPPED_BY_USER.into()),
            "{command}"
        );
        // Well before its 30 s deadline.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{command}: {:?}",
            started.elapsed()
        );
    }
}

#[test]
fn a_call_that_never_ends_is_stopped_at_its_deadline() {
    let runtime = Runtime::new(None).unwrap();
    let loaded = views(&runtime);
    let started = Instant::now();
    let stopped = loaded
        .run(
            Arc::new(FakeHost::default()),
            "spin",
            &at(None, Some(1)),
            Limits {
                deadline: Duration::from_millis(300),
                memory: 64 * 1024 * 1024,
                answer: Limits::COMMAND.answer,
            },
        )
        .unwrap_err();
    assert!(
        matches!(&stopped, Failure::TimedOut(_)) && stopped.to_string().contains("took longer"),
        "{stopped:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    // The next call starts afresh.
    assert!(
        loaded
            .run(
                Arc::new(FakeHost::default()),
                "say",
                &at(None, None),
                quick()
            )
            .is_ok()
    );
}

#[test]
fn a_call_that_takes_too_much_memory_is_stopped() {
    let runtime = Runtime::new(None).unwrap();
    let loaded = views(&runtime);
    let stopped = loaded
        .run(
            Arc::new(FakeHost::default()),
            "hog",
            &at(None, None),
            quick(),
        )
        .unwrap_err();
    assert!(
        matches!(&stopped, Failure::OutOfMemory(64))
            && stopped.to_string().contains("more than 64 MB"),
        "{stopped:?}"
    );
}

#[test]
fn a_panic_is_told_in_its_own_words() {
    let runtime = Runtime::new(None).unwrap();
    let loaded = views(&runtime);
    let stopped = loaded
        .run(
            Arc::new(FakeHost::default()),
            "panic",
            &at(None, Some(3)),
            quick(),
        )
        .unwrap_err();
    assert!(
        matches!(&stopped, Failure::Crashed(why) if why.contains("index out of bounds") && why.contains("src/lib.rs")),
        "{stopped:?}"
    );
}

#[test]
fn printing_a_lot_is_kept_in_part_and_never_fails() {
    let runtime = Runtime::new(None).unwrap();
    let loaded = views(&runtime);
    let host = Arc::new(FakeHost::default());
    assert_eq!(
        loaded
            .run(host.clone(), "chatter", &at(None, None), quick())
            .unwrap(),
        Outcome::Nothing
    );
    let log = host.log.lock().unwrap();
    let stdout = log.iter().find(|(l, _)| *l == Level::Info).unwrap();
    assert!(stdout.1.starts_with("line 0 of"));
    assert!(stdout.1.len() <= 64 * 1024);
    assert!(
        log.iter()
            .any(|(l, m)| *l == Level::Warning && m == "done printing")
    );
}

#[test]
fn a_plugin_keeps_its_own_store_between_calls() {
    let dir = data("store");
    let runtime = Runtime::new(Some(dir.clone())).unwrap();
    let loaded = views(&runtime);
    let host: Arc<dyn Host> = Arc::new(FakeHost::default());
    for n in 1..=3 {
        assert_eq!(
            loaded
                .run(host.clone(), "store", &at(None, None), quick())
                .unwrap(),
            Outcome::Say(format!("run {n}"))
        );
    }
    assert!(
        clew_plugin::runtime::store_dir(&dir, "views")
            .unwrap()
            .is_dir()
    );
    // Tried, a build of the same id has a store of its own: it starts
    // empty, does not see or change the installed plugin's, and is gone
    // with the trial. Nor is its compiled code kept.
    let compiled = |dir: &std::path::Path| {
        std::fs::read_dir(dir.join("plugins/compiled"))
            .map(|d| d.count())
            .unwrap_or(0)
    };
    let before = compiled(&dir);
    let (bytes, inspected) = fixture("views");
    let trial = runtime.load_trial(&bytes, &inspected, READ).unwrap();
    for n in 1..=2 {
        assert_eq!(
            trial
                .run(host.clone(), "store", &at(None, None), quick())
                .unwrap(),
            Outcome::Say(format!("run {n}"))
        );
    }
    drop(trial);
    assert!(
        std::fs::read_dir(dir.join("plugins/tries"))
            .map(|d| d.count() == 0)
            .unwrap_or(true),
        "the trial's store is gone"
    );
    assert_eq!(compiled(&dir), before, "no compiled copy kept");
    assert_eq!(
        loaded
            .run(host.clone(), "store", &at(None, None), quick())
            .unwrap(),
        Outcome::Say("run 4".into()),
        "the installed plugin's store was not touched"
    );
    // Without a data folder there is nowhere to keep it, and the plugin is told.
    let bare = Runtime::new(None).unwrap();
    let refused = views(&bare)
        .run(host, "store", &at(None, None), quick())
        .unwrap_err();
    assert!(matches!(refused, Failure::Plugin(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_graph_comes_back_checked() {
    let runtime = Runtime::new(None).unwrap();
    let loaded = views(&runtime);
    let graph = loaded
        .graph(Arc::new(FakeHost::default()), "pair", quick())
        .unwrap();
    assert_eq!(graph.nodes.len(), 2);
    assert_eq!(
        graph.edges.len(),
        1,
        "the edge to a node there is not is dropped"
    );
    assert_eq!(graph.nodes[0].at.as_ref().unwrap().path, "a.rs");
}

#[test]
fn compiled_code_is_cached_and_loads_fast() {
    let dir = data("cache");
    let (bytes, inspected) = fixture("views");
    let runtime = Runtime::new(Some(dir.clone())).unwrap();
    runtime.load(&bytes, &inspected, READ).unwrap();
    let compiled = dir.join("plugins").join("compiled");
    let cached: Vec<_> = std::fs::read_dir(&compiled).unwrap().flatten().collect();
    assert_eq!(cached.len(), 1);
    let again = Runtime::new(Some(dir.clone())).unwrap();
    let started = Instant::now();
    let loaded = again.load(&bytes, &inspected, READ).unwrap();
    let took = started.elapsed();
    assert!(
        loaded
            .run(
                Arc::new(FakeHost::default()),
                "say",
                &at(None, None),
                quick()
            )
            .is_ok()
    );
    assert!(took < Duration::from_secs(2), "{took:?}");
    // A damaged cache entry is compiled again, not trusted. (Replaced as
    // Clew replaces it, by a rename: a loaded plugin maps the file it was
    // loaded from, and writing into that file in place would pull its code
    // out from under it.)
    drop(loaded);
    let damaged = compiled.join("damaged");
    std::fs::write(&damaged, b"not compiled code").unwrap();
    std::fs::rename(&damaged, cached[0].path()).unwrap();
    let third = Runtime::new(Some(dir.clone())).unwrap();
    assert!(
        third
            .load(&bytes, &inspected, READ)
            .unwrap()
            .run(
                Arc::new(FakeHost::default()),
                "say",
                &at(None, None),
                quick()
            )
            .is_ok()
    );
    again.sweep_compiled(&[]);
    assert_eq!(std::fs::read_dir(&compiled).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn calls_run_in_parallel() {
    let runtime = Runtime::new(None).unwrap();
    let loaded = Arc::new(views(&runtime));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let loaded = loaded.clone();
            std::thread::spawn(move || {
                loaded
                    .run(
                        Arc::new(FakeHost::default()),
                        "all-blocks",
                        &at(None, None),
                        quick(),
                    )
                    .is_ok()
            })
        })
        .collect();
    assert!(threads.into_iter().all(|t| t.join().unwrap()));
}

#[test]
fn a_manifest_that_hides_a_permission_is_refused() {
    let (bytes, _) = fixture("views");
    let honest = b"permissions = [\"read\"]";
    let at = bytes
        .windows(honest.len())
        .position(|w| w == honest)
        .expect("the manifest lists read");
    let mut lying = bytes.clone();
    lying[at..at + honest.len()].copy_from_slice(b"permissions = []      ");
    let refused = clew_plugin::inspect(&lying).unwrap_err();
    assert!(refused.contains("does not list \"read\""), "{refused}");
}
