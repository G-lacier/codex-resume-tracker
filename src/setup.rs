use std::fmt;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::config::{Config, OnClose, SourceKind, SummaryInput, SummaryProvider};
use crate::paths::codex_home;

pub const HOOKS_DOC_URL: &str = "https://developers.openai.com/codex/hooks";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HookChoice {
    #[default]
    Automatic,
    Manual,
    Skip,
}

impl fmt::Display for HookChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Automatic => "automatic",
            Self::Manual => "manual",
            Self::Skip => "skip",
        })
    }
}

impl FromStr for HookChoice {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "automatic" | "auto" | "merge" => Ok(Self::Automatic),
            "manual" | "snippet" => Ok(Self::Manual),
            "skip" | "none" => Ok(Self::Skip),
            _ => bail!("unknown hook choice {value:?}; use automatic, manual, or skip"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookMergeOutcome {
    pub path: PathBuf,
    pub changed: bool,
}

pub fn hooks_path() -> Result<PathBuf> {
    Ok(codex_home()?.join("hooks.json"))
}

pub fn hook_command(executable: &Path) -> String {
    format!(
        "{} hook",
        shell_quote(executable.to_string_lossy().as_ref())
    )
}

pub fn hook_snippet(executable: &Path) -> Value {
    json!({
        "hooks": {
            "SessionEnd": [{
                "matcher": "^other$",
                "hooks": [{
                    "type": "command",
                    "command": hook_command(executable),
                    "timeout": 3,
                    "statusMessage": "Saving Codex resume command"
                }]
            }]
        }
    })
}

pub fn render_hook_snippet(executable: &Path) -> Result<String> {
    Ok(serde_json::to_string_pretty(&hook_snippet(executable))?)
}

pub fn merge_hook_file(path: &Path, executable: &Path) -> Result<HookMergeOutcome> {
    let existing = if path.exists() {
        let source = fs::read_to_string(path)
            .with_context(|| format!("read Codex hooks file {}", path.display()))?;
        serde_json::from_str(&source)
            .with_context(|| format!("parse Codex hooks file {}", path.display()))?
    } else {
        json!({})
    };
    let (merged, changed) = merge_hook_value(existing, &hook_command(executable))?;
    if changed {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create Codex config directory {}", parent.display()))?;
        }
        let mut rendered = serde_json::to_string_pretty(&merged)?;
        rendered.push('\n');
        fs::write(path, rendered)
            .with_context(|| format!("write Codex hooks file {}", path.display()))?;
    }
    Ok(HookMergeOutcome {
        path: path.to_path_buf(),
        changed,
    })
}

pub fn merge_hook_value(mut root: Value, command: &str) -> Result<(Value, bool)> {
    let object = root
        .as_object_mut()
        .context("Codex hooks file must contain a JSON object")?;
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .context("Codex hooks file field 'hooks' must be a JSON object")?;
    let session_end = hooks.entry("SessionEnd").or_insert_with(|| json!([]));
    let groups = session_end
        .as_array_mut()
        .context("Codex hooks SessionEnd value must be a JSON array")?;

    if contains_command(groups, command) {
        return Ok((root, false));
    }
    groups.push(json!({
        "matcher": "^other$",
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 3,
            "statusMessage": "Saving Codex resume command"
        }]
    }));
    Ok((root, true))
}

pub fn hook_is_installed(path: &Path, executable: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let root: Value = serde_json::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("read Codex hooks file {}", path.display()))?,
    )
    .with_context(|| format!("parse Codex hooks file {}", path.display()))?;
    let Some(groups) = root
        .get("hooks")
        .and_then(|hooks| hooks.get("SessionEnd"))
        .and_then(Value::as_array)
    else {
        return Ok(false);
    };
    Ok(contains_command(groups, &hook_command(executable)))
}

fn contains_command(groups: &[Value], expected: &str) -> bool {
    groups.iter().any(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|handler| {
                handler.get("type").and_then(Value::as_str) == Some("command")
                    && handler.get("command").and_then(Value::as_str) == Some(expected)
            })
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn interactive_wizard<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<(Config, HookChoice)> {
    writeln!(output, "Codex Resume Tracker setup")?;
    writeln!(output, "Press Enter to accept the highlighted default.\n")?;

    writeln!(output, "Summary provider:")?;
    writeln!(output, "  1) Local extractive (no external usage)")?;
    writeln!(output, "  2) Ephemeral Codex [default]")?;
    writeln!(output, "  3) OpenAI Responses API")?;
    let provider = match prompt_choice(input, output, "Choice", 2, 3)? {
        1 => SummaryProvider::Local,
        2 => SummaryProvider::Codex,
        3 => SummaryProvider::Openai,
        _ => unreachable!(),
    };

    writeln!(output, "\nSummary input:")?;
    writeln!(output, "  1) 16K characters")?;
    writeln!(output, "  2) 64K characters [default]")?;
    writeln!(output, "  3) Entire visible thread")?;
    writeln!(output, "  4) Custom positive character limit")?;
    let summary_input = match prompt_choice(input, output, "Choice", 2, 4)? {
        1 => SummaryInput::SixteenK,
        2 => SummaryInput::SixtyFourK,
        3 => SummaryInput::Entire,
        4 => {
            write!(output, "Custom character limit: ")?;
            output.flush()?;
            let value = read_line(input)?;
            SummaryInput::Custom(
                value
                    .parse()
                    .context("custom limit must be a positive integer")?,
            )
        }
        _ => unreachable!(),
    };

    writeln!(output, "\nOn-close behavior:")?;
    writeln!(output, "  1) Automatic saved provider [default]")?;
    writeln!(output, "  2) Local-first; AI only when requested")?;
    writeln!(output, "  3) Manual only")?;
    let on_close = match prompt_choice(input, output, "Choice", 1, 3)? {
        1 => OnClose::Automatic,
        2 => OnClose::LocalFirst,
        3 => OnClose::Manual,
        _ => unreachable!(),
    };

    writeln!(output, "\nVisible sources:")?;
    writeln!(output, "  1) CLI + IDE/VS Code; exclude exec [default]")?;
    writeln!(output, "  2) CLI only")?;
    writeln!(output, "  3) IDE/VS Code only")?;
    writeln!(output, "  4) CLI + IDE/VS Code + codex exec")?;
    let sources = match prompt_choice(input, output, "Choice", 1, 4)? {
        1 => vec![SourceKind::Cli, SourceKind::Vscode],
        2 => vec![SourceKind::Cli],
        3 => vec![SourceKind::Vscode],
        4 => vec![SourceKind::Cli, SourceKind::Vscode, SourceKind::Exec],
        _ => unreachable!(),
    };

    writeln!(output, "\nSessionEnd hook installation:")?;
    writeln!(output, "  1) Merge into ~/.codex/hooks.json [default]")?;
    writeln!(output, "  2) Print JSON for manual merging")?;
    writeln!(output, "  3) Skip")?;
    let hook = match prompt_choice(input, output, "Choice", 1, 3)? {
        1 => HookChoice::Automatic,
        2 => HookChoice::Manual,
        3 => HookChoice::Skip,
        _ => unreachable!(),
    };

    let mut config = Config {
        summary_provider: provider,
        summary_input,
        on_close,
        sources,
        ..Config::default()
    };
    if provider == SummaryProvider::Openai {
        write!(output, "\nOpenAI model [gpt-5-mini]: ")?;
        output.flush()?;
        let model = read_line(input)?;
        if !model.is_empty() {
            config.openai_model = model;
        }
    }
    config.validate()?;
    Ok((config, hook))
}

fn prompt_choice<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    label: &str,
    default: usize,
    maximum: usize,
) -> Result<usize> {
    loop {
        write!(output, "{label} [{default}]: ")?;
        output.flush()?;
        let line = read_line(input)?;
        if line.is_empty() {
            return Ok(default);
        }
        if let Ok(value) = line.parse::<usize>() {
            if (1..=maximum).contains(&value) {
                return Ok(value);
            }
        }
        writeln!(output, "Enter a number from 1 to {maximum}.")?;
    }
}

fn read_line<R: BufRead>(input: &mut R) -> Result<String> {
    let mut line = String::new();
    let bytes = input.read_line(&mut line)?;
    if bytes == 0 {
        bail!("setup input ended before all choices were answered");
    }
    Ok(line.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn test_path(name: &str) -> PathBuf {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("TMP/tests")
            .join(format!("setup-{name}-{}-{serial}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root.join("hooks.json")
    }

    #[test]
    fn hook_merge_preserves_existing_definitions_and_is_idempotent() {
        let path = test_path("merge");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "description": "keep me",
                "custom": {"untouched": true},
                "hooks": {
                    "SessionStart": [{"hooks":[{"type":"command","command":"notes start"}]}],
                    "SessionEnd": [{"matcher":"other","hooks":[{"type":"command","command":"notes end"}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let executable = Path::new("/opt/bin/codex-resume-tracker");
        let first = merge_hook_file(&path, executable).unwrap();
        let after_first = fs::read_to_string(&path).unwrap();
        let second = merge_hook_file(&path, executable).unwrap();
        let after_second = fs::read_to_string(&path).unwrap();
        assert!(first.changed);
        assert!(!second.changed);
        assert_eq!(after_first, after_second);
        let root: Value = serde_json::from_str(&after_second).unwrap();
        assert_eq!(root["description"], "keep me");
        assert_eq!(root["custom"]["untouched"], true);
        assert_eq!(root["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert_eq!(root["hooks"]["SessionEnd"].as_array().unwrap().len(), 2);
        assert!(hook_is_installed(&path, executable).unwrap());
    }

    #[test]
    fn invalid_existing_hook_shape_returns_error_without_overwriting() {
        let path = test_path("invalid");
        let original = "{\"hooks\":{\"SessionEnd\":true}}";
        fs::write(&path, original).unwrap();
        assert!(merge_hook_file(&path, Path::new("/bin/tracker")).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn wizard_accepts_documented_defaults() {
        let mut input = Cursor::new("\n\n\n\n\n");
        let mut output = Vec::new();
        let (config, hook) = interactive_wizard(&mut input, &mut output).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(hook, HookChoice::Automatic);
    }

    #[test]
    fn wizard_supports_custom_choices() {
        let mut input = Cursor::new("1\n4\n12345\n2\n4\n2\n");
        let mut output = Vec::new();
        let (config, hook) = interactive_wizard(&mut input, &mut output).unwrap();
        assert_eq!(config.summary_provider, SummaryProvider::Local);
        assert_eq!(config.summary_input, SummaryInput::Custom(12_345));
        assert_eq!(config.on_close, OnClose::LocalFirst);
        assert!(config.sources.contains(&SourceKind::Exec));
        assert_eq!(hook, HookChoice::Manual);
    }
}
