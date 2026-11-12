use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

/// Errors that abort a `truce` run (exit code 2).
///
/// Parse problems on individual files are recorded in the report instead of
/// using this type, so `--json` can still be emitted.
#[derive(Debug, thiserror::Error)]
pub enum TruceError {
    #[error("not a git repository (or any of the parent directories)")]
    NotAGitRepository,

    #[error("bare git repositories are not supported")]
    BareRepository,

    #[error("failed to run git{}: {message}", format_status(*.status))]
    Git {
        status: Option<ExitStatus>,
        message: String,
    },

    #[error("git is not available on PATH")]
    GitNotFound,

    #[error("path {} is outside the repository", path.display())]
    PathOutsideRepo { path: PathBuf },

    #[error("failed to read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{0}")]
    Io(#[from] io::Error),

    #[error("{0}")]
    Json(#[from] serde_json::Error),

    #[error("OPENROUTER_API_KEY is not set (required for --apply)")]
    MissingApiKey,

    #[error("OpenRouter authentication failed")]
    OpenRouterAuth,

    #[error("OpenRouter request failed: {0}")]
    OpenRouter(String),

    /// Non-auth failure from `POST /systemone` (HTTP error or malformed body).
    /// Callers escalate these per hunk; they are not a process-level failure.
    #[error("decision API request failed: {0}")]
    Decision(String),

    #[error("invalid configuration: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, TruceError>;

fn format_status(status: Option<ExitStatus>) -> String {
    match status {
        Some(status) => format!(" ({status})"),
        None => String::new(),
    }
}

impl TruceError {
    pub fn from_git_output(status: ExitStatus, stderr: &[u8]) -> Self {
        let message = String::from_utf8_lossy(stderr);
        let trimmed = message.trim();
        if trimmed.contains("not a git repository") {
            return Self::NotAGitRepository;
        }
        Self::Git {
            status: Some(status),
            message: if trimmed.is_empty() {
                "git command failed".to_string()
            } else {
                trimmed.to_string()
            },
        }
    }
}
