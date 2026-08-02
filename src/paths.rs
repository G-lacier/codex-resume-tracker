use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::APP_NAME;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set; cannot resolve XDG paths")?;

        let data_root = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        let config_root = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let state_root = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"));

        Ok(Self::from_roots(data_root, config_root, state_root))
    }

    pub fn from_roots(
        data_root: impl AsRef<Path>,
        config_root: impl AsRef<Path>,
        state_root: impl AsRef<Path>,
    ) -> Self {
        Self {
            data_dir: data_root.as_ref().join(APP_NAME),
            config_dir: config_root.as_ref().join(APP_NAME),
            state_dir: state_root.as_ref().join(APP_NAME),
        }
    }

    pub fn database(&self) -> PathBuf {
        self.data_dir.join("tracker.sqlite3")
    }

    pub fn config(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn worker_log(&self) -> PathBuf {
        self.state_dir.join("worker.log")
    }

    pub fn summary_schema(&self) -> PathBuf {
        self.state_dir.join("summary-output.schema.json")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("create {}", self.data_dir.display()))?;
        std::fs::create_dir_all(&self.config_dir)
            .with_context(|| format!("create {}", self.config_dir.display()))?;
        std::fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("create {}", self.state_dir.display()))?;
        Ok(())
    }
}

pub fn codex_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot resolve CODEX_HOME")?;
    Ok(home.join(".codex"))
}
