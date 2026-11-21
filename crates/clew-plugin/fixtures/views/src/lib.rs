//! Clew's test plugin (crates/clew-plugin/tests/runtime.rs): every kind of
//! answer a command can give, and every way a call can fail.

use clew_plugin_sdk::{
    host, project, Graph, Invocation, Item, Level, Location, Outcome, Plugin, Row, Table,
    TreeRow, View,
};

struct Views;

impl Plugin for Views {
    fn run(command: &str, context: Invocation) -> Result<Outcome, String> {
        match command {
            "all-blocks" => Ok(Outcome::Show(
                View::new("Every block")
                    .heading("A heading")
                    .text("Some *Markdown*.")
                    .notice(Level::Warning, "A notice")
                    .items(vec![
                        Item::new("An item").detail("its detail").at("src/lib.rs", 3).tag("tag"),
                        Item::new("Outside").at("../secret", 1),
                    ])
                    .tree(vec![TreeRow::new(0, "root"), TreeRow::new(1, "child").at("a.rs", 2)])
                    .table(Table::new(["Name", "Count"]).row(Row::new(["a", "1"]).at("b.rs", 1)))
                    .facts([("Files", "3")])
                    .code(Some("rust"), "fn main() {}"),
            )),
            "open" => Ok(Outcome::Open(Location::new("src/main.rs", 7))),
            "say" => Ok(Outcome::Say("Said\nin two lines".into())),
            "refuse" => Err("I will not".into()),
            "read" => {
                let path = context.file.ok_or("no file is open")?;
                let text = project::read(&path)?;
                host::info(&format!("read {path}"));
                Ok(Outcome::Say(format!(
                    "{} has {} lines, {} files in {}",
                    path,
                    text.lines().count(),
                    project::files().len(),
                    project::info().name
                )))
            }
            "escape" => project::read("../outside.txt").map(Outcome::Say),
            "store" => {
                let runs = host::load("runs")
                    .and_then(|b| String::from_utf8(b).ok())
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0)
                    + 1;
                host::save("runs", runs.to_string().as_bytes())?;
                Ok(Outcome::Say(format!("run {runs}")))
            }
            "spin" => {
                let mut n: u64 = 0;
                loop {
                    n = n.wrapping_add(context.line.unwrap_or(1) as u64);
                    if n == u64::MAX {
                        return Ok(Outcome::Nothing);
                    }
                }
            }
            // Sleeps in the host, where the deadline does not reach.
            "sleep" => {
                std::thread::sleep(std::time::Duration::from_secs(3600));
                Ok(Outcome::Say("woke up".into()))
            }
            // A short sleep is slept whole.
            "nap" => {
                let started = std::time::Instant::now();
                std::thread::sleep(std::time::Duration::from_millis(50));
                Ok(Outcome::Say(format!("{}", started.elapsed().as_millis() >= 50)))
            }
            "hog" => {
                let mut kept: Vec<Vec<u8>> = Vec::new();
                loop {
                    kept.push(vec![1u8; 16 * 1024 * 1024]);
                    if kept.len() == usize::MAX {
                        return Ok(Outcome::Nothing);
                    }
                }
            }
            "panic" => {
                let words: Vec<&str> = Vec::new();
                Ok(Outcome::Say(words[context.line.unwrap_or(3) as usize].to_string()))
            }
            "chatter" => {
                for i in 0..20_000 {
                    println!("line {i} of a plugin that prints too much");
                }
                eprintln!("done printing");
                Ok(Outcome::Nothing)
            }
            other => Err(format!("no command {other}")),
        }
    }

    fn graph(graph: &str) -> Result<Graph, String> {
        if graph != "pair" {
            return Err(format!("no graph {graph}"));
        }
        let mut g = Graph::new();
        let a = g.node_at("module", "a", "a.rs", 1);
        let b = g.node("module", "b");
        g.edge(a, b, "uses");
        g.edge(b, 9, "dangling");
        Ok(g)
    }
}

clew_plugin_sdk::export!(Views);
