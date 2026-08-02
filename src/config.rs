use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::paths::AppPaths;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryProvider {
    Local,
    #[default]
    Codex,
    Openai,
}

impl SummaryProvider {
    pub const ALL: [Self; 3] = [Self::Local, Self::Codex, Self::Openai];

    pub fn is_external(self) -> bool {
        !matches!(self, Self::Local)
    }
}

impl fmt::Display for SummaryProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Local => "local",
            Self::Codex => "codex",
            Self::Openai => "openai",
        })
    }
}

impl FromStr for SummaryProvider {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "codex" | "ephemeral-codex" | "ephemeral_codex" => Ok(Self::Codex),
            "openai" | "api" => Ok(Self::Openai),
            _ => bail!("unknown summary provider {value:?}; use local, codex, or openai"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SummaryInput {
    SixteenK,
    #[default]
    SixtyFourK,
    Entire,
    Custom(usize),
}

impl SummaryInput {
    pub const CYCLE: [Self; 3] = [Self::SixteenK, Self::SixtyFourK, Self::Entire];

    pub fn character_limit(self) -> Option<usize> {
        match self {
            Self::SixteenK => Some(16 * 1024),
            Self::SixtyFourK => Some(64 * 1024),
            Self::Entire => None,
            Self::Custom(value) => Some(value),
        }
    }
}

impl fmt::Display for SummaryInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SixteenK => f.write_str("16k"),
            Self::SixtyFourK => f.write_str("64k"),
            Self::Entire => f.write_str("entire"),
            Self::Custom(value) => write!(f, "custom:{value}"),
        }
    }
}

impl FromStr for SummaryInput {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let normalized = value.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "16k" | "16384" => Ok(Self::SixteenK),
            "64k" | "65536" => Ok(Self::SixtyFourK),
            "entire" | "all" | "unlimited" => Ok(Self::Entire),
            _ => {
                let raw = normalized.strip_prefix("custom:").unwrap_or(&normalized);
                let count: usize = raw
                    .parse()
                    .with_context(|| format!("invalid summary character limit {value:?}"))?;
                if count == 0 {
                    bail!("summary character limit must be positive");
                }
                Ok(Self::Custom(count))
            }
        }
    }
}

impl Serialize for SummaryInput {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for SummaryInput {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnClose {
    #[default]
    Automatic,
    LocalFirst,
    Manual,
}

impl fmt::Display for OnClose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Automatic => "automatic",
            Self::LocalFirst => "local_first",
            Self::Manual => "manual",
        })
    }
}

impl FromStr for OnClose {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().replace('-', "_").as_str() {
            "automatic" | "auto" => Ok(Self::Automatic),
            "local_first" | "local" => Ok(Self::LocalFirst),
            "manual" | "manual_only" => Ok(Self::Manual),
            _ => bail!("unknown on-close behavior {value:?}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SourceKind {
    Cli,
    Vscode,
    Exec,
}

impl SourceKind {
    pub fn as_protocol_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Vscode => "vscode",
            Self::Exec => "exec",
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_protocol_str())
    }
}

impl FromStr for SourceKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "cli" => Ok(Self::Cli),
            "vscode" | "ide" | "vs-code" => Ok(Self::Vscode),
            "exec" => Ok(Self::Exec),
            _ => bail!("unknown source {value:?}; use cli, vscode, or exec"),
        }
    }
}

fn default_sources() -> Vec<SourceKind> {
    vec![SourceKind::Cli, SourceKind::Vscode]
}

fn default_terminal() -> Vec<String> {
    [
        "x-terminal-emulator",
        "-e",
        "codex",
        "-C",
        "{cwd}",
        "resume",
        "{thread_id}",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn default_openai_model() -> String {
    "gpt-5-mini".to_owned()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub summary_provider: SummaryProvider,
    pub summary_input: SummaryInput,
    pub on_close: OnClose,
    pub sources: Vec<SourceKind>,
    pub terminal_argv: Vec<String>,
    pub openai_model: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            summary_provider: SummaryProvider::Codex,
            summary_input: SummaryInput::SixtyFourK,
            on_close: OnClose::Automatic,
            sources: default_sources(),
            terminal_argv: default_terminal(),
            openai_model: default_openai_model(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported config version {}; expected 1", self.version);
        }
        if self.sources.is_empty() {
            bail!("at least one source must be visible");
        }
        if self.terminal_argv.is_empty() || self.terminal_argv[0].trim().is_empty() {
            bail!("terminal_argv must contain an executable");
        }
        if !self
            .terminal_argv
            .iter()
            .any(|part| part.contains("{thread_id}"))
        {
            bail!("terminal_argv must contain {{thread_id}}");
        }
        if !self.terminal_argv.iter().any(|part| part.contains("{cwd}")) {
            bail!("terminal_argv must contain {{cwd}}");
        }
        if self.openai_model.trim().is_empty() {
            bail!("openai_model cannot be empty");
        }
        if matches!(self.summary_input, SummaryInput::Custom(0)) {
            bail!("summary character limit must be positive");
        }
        Ok(())
    }

    pub fn load(paths: &AppPaths) -> Result<Self> {
        Self::load_from(&paths.config())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("read tracker config {}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .with_context(|| format!("parse tracker config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn load_or_default(paths: &AppPaths) -> Result<Self> {
        if paths.config().exists() {
            Self::load(paths)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self, paths: &AppPaths) -> Result<()> {
        self.validate()?;
        paths.ensure_dirs()?;
        let rendered = toml::to_string_pretty(self).context("serialize tracker config")?;
        fs::write(paths.config(), rendered)
            .with_context(|| format!("write tracker config {}", paths.config().display()))
    }

    pub fn source_names(&self) -> Vec<&'static str> {
        self.sources
            .iter()
            .map(|source| source.as_protocol_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_product_choices() {
        let config = Config::default();
        assert_eq!(config.summary_provider, SummaryProvider::Codex);
        assert_eq!(config.summary_input, SummaryInput::SixtyFourK);
        assert_eq!(config.on_close, OnClose::Automatic);
        assert_eq!(config.sources, vec![SourceKind::Cli, SourceKind::Vscode]);
        assert!(!config.sources.contains(&SourceKind::Exec));
    }

    #[test]
    fn custom_summary_limit_round_trips_through_toml() {
        let config = Config {
            summary_input: SummaryInput::Custom(12_345),
            ..Config::default()
        };
        let encoded = toml::to_string(&config).expect("serialize");
        let decoded: Config = toml::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, config);
    }

    #[test]
    fn terminal_template_requires_both_placeholders() {
        let config = Config {
            terminal_argv: vec!["terminal".into(), "{thread_id}".into()],
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }
}
