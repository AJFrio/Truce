use std::path::PathBuf;

use clap::Parser;

/// Resolve git merge conflicts using fast decision models.
///
/// Truce scans the current git working tree for unmerged files, parses
/// conflict hunks, and prints a report. Resolution is preview-only by
/// default (`--dry-run`). `--apply` writes accepted resolutions back to
/// the working tree. `--stage` runs `git add` on files that were written.
#[derive(Debug, Parser)]
#[command(
    name = "truce",
    version,
    about = "Resolve git merge conflicts using fast decision models",
    long_about = "Truce finds merge conflicts in a git working tree, parses each \
conflict hunk (including optional diff3/zdiff3 base sections), sends hunks to \
an OpenRouter decision model, and prints a human or JSON report.\n\n\
Dry-run is the default: the model still runs (when OPENROUTER_API_KEY is set) \
and a diff-style preview is printed, but nothing is written. Pass --apply to \
write files that resolved completely. Pass --stage with --apply to `git add` \
those files. Binary and non-UTF-8 files are skipped and reported, never rewritten.\n\n\
Model selection: --model, then TRUCE_MODEL, then .truce.toml in the repo root, \
then $XDG_CONFIG_HOME/truce/config.toml, then the built-in default \
(openai/gpt-4o-mini).\n\n\
Color is used only when stdout is a TTY. It is disabled when --no-color is \
set, when NO_COLOR is present in the environment, or when TERM=dumb. Labels \
such as OURS/THEIRS/RESOLVED/REJECTED/SKIPPED are always printed so the \
report is readable without color.\n\n\
The API key is read from OPENROUTER_API_KEY and is never printed.\n\n\
CI mode (--ci, or GITHUB_ACTIONS=true) disables color, emits GitHub \
::error/::warning annotations, and appends a Markdown job summary to \
$GITHUB_STEP_SUMMARY.",
    after_help = "Exit codes:\n  \
0  No conflicts, or every conflict was resolved\n  \
1  Unresolved conflicts remain\n  \
2  Usage error or runtime failure\n\n\
JSON schema:\n  \
--json emits a single object with schema_version = 1. See README.md for the \
full schema."
)]
pub struct Cli {
    /// Emit a machine-readable JSON report to stdout (schema_version 1).
    #[arg(long)]
    pub json: bool,

    /// Preview only; do not write files. This is the default.
    #[arg(long, overrides_with = "apply")]
    pub dry_run: bool,

    /// Write accepted resolutions back to the working tree.
    ///
    /// A file is rewritten only when every hunk in it has a valid resolution.
    #[arg(long, overrides_with = "dry_run")]
    pub apply: bool,

    /// `git add` files after a successful `--apply` write.
    #[arg(long)]
    pub stage: bool,

    /// OpenRouter model id (overrides TRUCE_MODEL and config files).
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Reject resolutions below this confidence (0–1). Default: 0.7.
    #[arg(long, value_name = "N")]
    pub min_confidence: Option<f64>,

    /// Max in-flight OpenRouter requests.
    #[arg(long, value_name = "N")]
    pub concurrency: Option<usize>,

    /// Per-request timeout in seconds. Default: 30.
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,

    /// Retries after the first attempt for 429/5xx/timeouts. Default: 3.
    #[arg(long, value_name = "N")]
    pub retries: Option<u32>,

    /// OpenRouter API base URL (for tests or a proxy). Default:
    /// https://openrouter.ai/api/v1
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,

    /// Lines of surrounding context to include with each hunk.
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub context: usize,

    /// Disable ANSI color even when stdout is a TTY.
    #[arg(long)]
    pub no_color: bool,

    /// GitHub Actions CI mode (also auto-detected from GITHUB_ACTIONS=true).
    ///
    /// Non-interactive, no color, `::error`/`::warning` annotations, and a
    /// job summary at `$GITHUB_STEP_SUMMARY`.
    #[arg(long)]
    pub ci: bool,

    /// Limit scanning to these paths (relative to the repo or cwd).
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
}

impl Cli {
    /// True when the user asked to write files (`--apply` and not `--dry-run`).
    pub fn should_apply(&self) -> bool {
        self.apply && !self.dry_run
    }

    /// `--ci` or `GITHUB_ACTIONS=true`.
    pub fn ci_mode(&self) -> bool {
        crate::ci::enabled(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn help_mentions_exit_codes_and_json() {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        cmd.write_long_help(&mut buf).unwrap();
        let help = String::from_utf8(buf).unwrap();
        assert!(help.contains("Exit codes:"), "{help}");
        assert!(help.contains("--json"), "{help}");
        assert!(help.contains("--apply"), "{help}");
        assert!(help.contains("--dry-run"), "{help}");
        assert!(help.contains("--model"), "{help}");
        assert!(help.contains("--stage"), "{help}");
        assert!(help.contains("OPENROUTER_API_KEY"), "{help}");
        assert!(help.contains("NO_COLOR"), "{help}");
        assert!(help.contains("--ci"), "{help}");
        assert!(help.contains("GITHUB_ACTIONS"), "{help}");
    }

    #[test]
    fn apply_is_off_by_default() {
        let cli = Cli::parse_from(["truce"]);
        assert!(!cli.should_apply());
        assert!(!cli.json);
        assert!(!cli.stage);
        assert!(!cli.ci);
        assert_eq!(cli.context, 3);
    }

    #[test]
    fn apply_flag_enables_writes() {
        let cli = Cli::parse_from(["truce", "--apply"]);
        assert!(cli.should_apply());
    }

    #[test]
    fn dry_run_overrides_apply() {
        let cli = Cli::parse_from(["truce", "--apply", "--dry-run"]);
        assert!(!cli.should_apply());
    }
}
