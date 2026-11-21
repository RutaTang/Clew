//! Clew's test plugin that needs the git permission and nothing else.

use clew_plugin_sdk::{git, Invocation, Outcome, Plugin};

struct GitLog;

impl Plugin for GitLog {
    fn run(_command: &str, _context: Invocation) -> Result<Outcome, String> {
        let commits = git::log(None, 1)?;
        let latest = commits.first().ok_or("no commits")?;
        Ok(Outcome::Say(format!("{} by {}", latest.subject, latest.author)))
    }
}

clew_plugin_sdk::export!(GitLog);
