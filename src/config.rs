use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::cli::Cli;
use crate::error::{Result, TruceError};

/// Decision model used when `--model`, `TRUCE_MODEL`, and config files are unset.
pub const DEFAULT_MODEL: &str = "liquid/d1-ultra";
/// Chat model used when a hunk escalates and no code model was configured.
pub const DEFAULT_CODE_MODEL: &str = "~deepseek/deepseek-flash-latest";
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.7;
pub const DEFAULT_CONCURRENCY: usize = 8;
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_RETRIES: u32 = 3;

/// Merged runtime settings. Precedence: CLI flag > env > repo `.truce.toml` >
/// user config > built-in default.
#[derive(Debug, Clone)]
pub struct Settings {
    /// System One decision model (`--model` / `TRUCE_MODEL`).
    pub model: String,
    /// Chat model for code-generation escalation (`--code-model` / `TRUCE_CODE_MODEL`).
    pub code_model: String,
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
    code_model: Option<String>,
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
            code_model: higher.code_model.or(self.code_model),
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

    let code_model = first_nonempty([
        cli.code_model.clone(),
        env_nonempty("TRUCE_CODE_MODEL"),
        file.code_model,
    ])
    .unwrap_or_else(|| DEFAULT_CODE_MODEL.to_string());

    let base_url = first_nonempty([
        cli.base_url.clone(),
        env_nonempty("TRUCE_BASE_URL"),
        env_nonempty("OPENROUTER_BASE_URL"),
        file.base_url,
    ])
    .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

    let min_confidence = cli
        .min_confidence
        .or(env_f64("TRUCE_MIN_CONFIDENCE")?)
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
        code_model,
        min_confidence,
        concurrency,
        timeout: Duration::from_secs(timeout_secs),
        retries,
        base_url,
        api_key,
    })
}

fn env_f64(name: &str) -> Result<Option<f64>> {
    let Some(raw) = env_nonempty(name) else {
        return Ok(None);
    };
    raw.parse::<f64>()
        .map(Some)
        .map_err(|_| TruceError::Config(format!("{name} must be a number between 0 and 1")))
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
        if env::var_os("TRUCE_CODE_MODEL").is_none() {
            assert_eq!(s.code_model, DEFAULT_CODE_MODEL);
        }
        if env::var_os("TRUCE_BASE_URL").is_none() && env::var_os("OPENROUTER_BASE_URL").is_none() {
            assert_eq!(s.base_url, DEFAULT_BASE_URL);
        }
        if env::var_os("TRUCE_MIN_CONFIDENCE").is_none() {
            assert_eq!(s.min_confidence, DEFAULT_MIN_CONFIDENCE);
        }
        assert_eq!(s.concurrency, DEFAULT_CONCURRENCY);
    }

    #[test]
    fn repo_toml_beats_defaults() {
        if env::var_os("TRUCE_MODEL").is_some()
            || env::var_os("TRUCE_CODE_MODEL").is_some()
            || env::var_os("TRUCE_MIN_CONFIDENCE").is_some()
        {
            return;
        }
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
    fn code_model_cli_beats_repo_toml() {
        if env::var_os("TRUCE_CODE_MODEL").is_some()
            || env::var_os("TRUCE_MIN_CONFIDENCE").is_some()
        {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let defaults = load(&cli(&[]), dir.path()).unwrap();
        assert_eq!(defaults.code_model, DEFAULT_CODE_MODEL);
        fs::write(
            dir.path().join(".truce.toml"),
            "code_model = \"repo/code\"\nmin_confidence = 0.4\n",
        )
        .unwrap();
        let from_file = load(&cli(&[]), dir.path()).unwrap();
        assert_eq!(from_file.code_model, "repo/code");
        assert!((from_file.min_confidence - 0.4).abs() < f64::EPSILON);
        let from_cli = load(
            &cli(&["--code-model", "cli/code", "--min-confidence", "0.8"]),
            dir.path(),
        )
        .unwrap();
        assert_eq!(from_cli.code_model, "cli/code");
        assert!((from_cli.min_confidence - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn accepts_confidence_endpoints() {
        let dir = tempfile::TempDir::new().unwrap();
        let zero = load(&cli(&["--min-confidence", "0"]), dir.path()).unwrap();
        assert_eq!(zero.min_confidence, 0.0);
        let one = load(&cli(&["--min-confidence", "1"]), dir.path()).unwrap();
        assert_eq!(one.min_confidence, 1.0);
    }

    #[test]
    fn rejects_bad_confidence() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = load(&cli(&["--min-confidence", "1.5"]), dir.path()).unwrap_err();
        assert!(err.to_string().contains("min-confidence"), "{err}");
    }
}
