use std::fmt;
use std::path::Path;
use std::process::Command;

use semver::Version;

use crate::app_server::CodexAppServer;
use crate::config::{Config, SummaryProvider};
use crate::db::Database;
use crate::paths::AppPaths;
use crate::setup;
use crate::terminal::find_executable;

pub const MINIMUM_CODEX_VERSION: &str = "0.144.1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckLevel {
    Pass,
    Warn,
    Fail,
}

impl fmt::Display for CheckLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Check {
    pub level: CheckLevel,
    pub name: &'static str,
    pub detail: String,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            level: CheckLevel::Pass,
            name,
            detail: detail.into(),
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            level: CheckLevel::Warn,
            name,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            level: CheckLevel::Fail,
            name,
            detail: detail.into(),
        }
    }
}

pub fn run(paths: &AppPaths, executable: &Path) -> Vec<Check> {
    let mut checks = Vec::new();

    let config = match Config::load(paths) {
        Ok(config) => {
            checks.push(Check::pass(
                "config",
                format!("valid: {}", paths.config().display()),
            ));
            Some(config)
        }
        Err(error) => {
            checks.push(Check::fail("config", format!("{error:#}")));
            None
        }
    };

    checks.push(check_codex_version());

    match CodexAppServer::connect().and_then(|mut server| server.validate_connection()) {
        Ok(()) => checks.push(Check::pass(
            "app-server",
            "initialize and thread/list succeeded",
        )),
        Err(error) => checks.push(Check::fail("app-server", format!("{error:#}"))),
    }

    match Database::open(paths.database()).and_then(|database| database.integrity_check()) {
        Ok(result) if result == "ok" => checks.push(Check::pass(
            "database",
            format!("integrity ok: {}", paths.database().display()),
        )),
        Ok(result) => checks.push(Check::fail(
            "database",
            format!("integrity check: {result}"),
        )),
        Err(error) => checks.push(Check::fail("database", format!("{error:#}"))),
    }

    if let Some(config) = &config {
        let terminal = &config.terminal_argv[0];
        match find_executable(terminal) {
            Some(path) => checks.push(Check::pass("terminal", format!("{}", path.display()))),
            None => checks.push(Check::fail(
                "terminal",
                format!("executable {terminal:?} not found on PATH"),
            )),
        }
    }

    match find_executable("setsid") {
        Some(path) => checks.push(Check::pass("worker detach", format!("{}", path.display()))),
        None => checks.push(Check::fail(
            "worker detach",
            "setsid was not found; install util-linux",
        )),
    }

    match setup::hooks_path() {
        Ok(path) => match setup::hook_is_installed(&path, executable) {
            Ok(true) => checks.push(Check::pass(
                "SessionEnd hook",
                format!("installed in {}; approve it with /hooks", path.display()),
            )),
            Ok(false) => checks.push(Check::warn(
                "SessionEnd hook",
                format!(
                    "not installed in {}; run setup or merge the snippet",
                    path.display()
                ),
            )),
            Err(error) => checks.push(Check::fail("SessionEnd hook", format!("{error:#}"))),
        },
        Err(error) => checks.push(Check::fail("SessionEnd hook", format!("{error:#}"))),
    }

    if let Some(config) = config {
        checks.push(check_provider(config.summary_provider));
    }
    checks
}

pub fn has_failures(checks: &[Check]) -> bool {
    checks.iter().any(|check| check.level == CheckLevel::Fail)
}

fn check_codex_version() -> Check {
    let output = match Command::new("codex").arg("--version").output() {
        Ok(output) => output,
        Err(error) => return Check::fail("Codex version", error.to_string()),
    };
    if !output.status.success() {
        return Check::fail(
            "Codex version",
            format!("codex --version exited with {}", output.status),
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    match parse_codex_version(&stdout) {
        Ok(version) => {
            let minimum = Version::parse(MINIMUM_CODEX_VERSION).expect("valid minimum version");
            if version >= minimum {
                Check::pass(
                    "Codex version",
                    format!("{version} is compatible (requires {MINIMUM_CODEX_VERSION}+)"),
                )
            } else {
                Check::fail(
                    "Codex version",
                    format!("{version} is too old; requires {MINIMUM_CODEX_VERSION}+"),
                )
            }
        }
        Err(error) => Check::fail("Codex version", error),
    }
}

pub fn parse_codex_version(output: &str) -> Result<Version, String> {
    output
        .split_whitespace()
        .find_map(|word| Version::parse(word.trim_start_matches('v')).ok())
        .ok_or_else(|| format!("could not parse version from {output:?}"))
}

fn check_provider(provider: SummaryProvider) -> Check {
    match provider {
        SummaryProvider::Local => {
            Check::pass("provider", "local extractive provider needs no credentials")
        }
        SummaryProvider::Openai => match std::env::var("OPENAI_API_KEY") {
            Ok(value) if !value.trim().is_empty() => Check::pass(
                "provider",
                "OPENAI_API_KEY is present in the environment (value not displayed or stored)",
            ),
            _ => Check::fail(
                "provider",
                "OPENAI_API_KEY is not present in the environment",
            ),
        },
        SummaryProvider::Codex => match Command::new("codex").args(["login", "status"]).output() {
            Ok(output) if output.status.success() => Check::pass(
                "provider",
                "Codex login status is valid for ephemeral summaries",
            ),
            Ok(output) => Check::fail(
                "provider",
                format!("codex login status exited with {}", output.status),
            ),
            Err(error) => Check::fail("provider", format!("run codex login status: {error}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parser_accepts_codex_cli_output() {
        assert_eq!(
            parse_codex_version("codex-cli 0.144.1\n").unwrap(),
            Version::new(0, 144, 1)
        );
        assert!(parse_codex_version("codex unknown").is_err());
    }

    #[test]
    fn required_minimum_is_enforced_semantically() {
        let minimum = Version::parse(MINIMUM_CODEX_VERSION).unwrap();
        assert!(Version::new(0, 144, 0) < minimum);
        assert!(Version::new(0, 145, 0) > minimum);
    }

    #[test]
    fn warnings_do_not_make_doctor_fail() {
        let checks = vec![Check::pass("a", "ok"), Check::warn("b", "optional")];
        assert!(!has_failures(&checks));
        assert!(has_failures(&[Check::fail("c", "broken")]));
    }
}
