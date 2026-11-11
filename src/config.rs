use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::cli::Cli;
use crate::error::{Result, TruceError};

/// Fast default used when `--model`, `TRUCE_MODEL`, and config files are unset.
pub const DEFAULT_MODEL: &str = "openai/gpt-4o-mini";
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.7;
pub const DEFAULT_CONCURRENCY: usize = 8;
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_RETRIES: u32 = 3;

/// Merged runtime settings. Precedence: CLI flag > env > repo `.truce.toml` >
/// user config > built-in default.
#[derive(Debug, Clone)]
pub struct Settings {
    pub model: String,
    pub min_confidence: f64,
    pub concurrency: usize,
    pub timeout: Duration,
    pub retries: u32,
    pub base_url: String,
    /// Present only when `OPENROUTER_API_KEY` is set and non-empty.
    pub api_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    model: Option<String>,
    min_confidence: Option<f64>,
    concurrency: Option<usize>,
    timeout_secs: Option<u64>,
    retries: Option<u32>,
    base_url: Option<String>,
}

impl FileConfig {
    fn load_path(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).map_err(|source| TruceError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text)
            .map_err(|err| TruceError::Config(format!("{}: {err}", path.display())))
    }

    fn overlay(self, higher: Self) -> Self {
        Self {
            model: higher.model.or(self.model),
            min_confidence: higher.min_confidence.or(self.min_confidence),
            concurrency: higher.concurrency.or(self.concurrency),
            timeout_secs: higher.timeout_secs.or(self.timeout_secs),
            retries: higher.retries.or(self.retries),
            base_url: higher.base_url.or(self.base_url),
        }
    }
}

pub fn load(cli: &Cli, repo_root: &Path) -> Result<Settings> {
    let user = FileConfig::load_path(&user_config_path())?;
    let repo = FileConfig::load_path(&repo_root.join(".truce.toml"))?;
    let file = user.overlay(repo);

    let model = first_nonempty([cli.model.clone(), env_nonempty("TRUCE_MODEL"), file.model])
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    let base_url = first_nonempty([
        cli.base_url.clone(),
        env_nonempty("TRUCE_BASE_URL"),
        env_nonempty("OPENROUTER_BASE_URL"),
        file.base_url,
    ])
    .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

    let min_confidence = cli
        .min_confidence
        .or(file.min_confidence)
        .unwrap_or(DEFAULT_MIN_CONFIDENCE);
    if !(0.0..=1.0).contains(&min_confidence) {
        return Err(TruceError::Config(
            "--min-confidence must be between 0 and 1".into(),
        ));
    }

    let concurrency = cli
        .concurrency
        .or(file.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);
    if concurrency == 0 {
        return Err(TruceError::Config(
            "--concurrency must be at least 1".into(),
        ));
    }

    let timeout_secs = cli
        .timeout
        .or(file.timeout_secs)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    if timeout_secs == 0 {
        return Err(TruceError::Config(
            "--timeout must be at least 1 second".into(),
        ));
    }

    let retries = cli.retries.or(file.retries).unwrap_or(DEFAULT_RETRIES);

    let api_key = env_nonempty("OPENROUTER_API_KEY");

    Ok(Settings {
        model,
        min_confidence,
        concurrency,
        timeout: Duration::from_secs(timeout_secs),
        retries,
        base_url,
        api_key,
    })
}

fn env_nonempty(name: &str) -> Option<String> {
    env::var(name).ok().and_then(|s| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })
}

fn first_nonempty<const N: usize>(vals: [Option<String>; N]) -> Option<String> {
    vals.into_iter().flatten().find(|s| !s.trim().is_empty())
}

pub fn user_config_path() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("truce").join("config.toml");
    }
    #[cfg(windows)]
    {
        if let Some(app) = env::var_os("APPDATA") {
            return PathBuf::from(app).join("truce").join("config.toml");
        }
    }
    match env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".config/truce/config.toml"),
        None => PathBuf::from("truce-config.toml"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        let mut all = vec!["truce"];
        all.extend_from_slice(args);
        Cli::parse_from(all)
    }

    #[test]
    fn default_model_when_nothing_set() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = load(&cli(&[]), dir.path()).unwrap();
        if env::var_os("TRUCE_MODEL").is_none() {
            assert_eq!(s.model, DEFAULT_MODEL);
        }
        if env::var_os("TRUCE_BASE_URL").is_none() && env::var_os("OPENROUTER_BASE_URL").is_none() {
            assert_eq!(s.base_url, DEFAULT_BASE_URL);
        }
        assert_eq!(s.min_confidence, DEFAULT_MIN_CONFIDENCE);
        assert_eq!(s.concurrency, DEFAULT_CONCURRENCY);
    }

    #[test]
    fn repo_toml_beats_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(
            dir.path().join(".truce.toml"),
            "model = \"repo/model\"\nmin_confidence = 0.9\nconcurrency = 2\n",
        )
        .unwrap();
        let s = load(&cli(&[]), dir.path()).unwrap();
        assert_eq!(s.model, "repo/model");
        assert_eq!(s.min_confidence, 0.9);
        assert_eq!(s.concurrency, 2);
    }

    #[test]
    fn cli_model_beats_repo_toml() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join(".truce.toml"), "model = \"repo/model\"\n").unwrap();
        let s = load(&cli(&["--model", "cli/model"]), dir.path()).unwrap();
        assert_eq!(s.model, "cli/model");
    }

    #[test]
    fn rejects_bad_confidence() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = load(&cli(&["--min-confidence", "1.5"]), dir.path()).unwrap_err();
        assert!(err.to_string().contains("min-confidence"), "{err}");
    }
}
