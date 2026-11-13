use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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
`truce sync` fetches a base branch (default origin/main), merges it into the \
current branch, resolves conflicts, and commits. It pushes only with --push. \
See `truce sync --help` for abort rules and exit codes.\n\n\
`truce update` compares this binary with the latest GitHub Release of \
AJFrio/Truce and replaces it when a newer version exists. `--check` prints \
the result without downloading. See `truce update --help`.\n\n\
Model selection for the decision model: --model, then TRUCE_MODEL, then \
.truce.toml in the repo root, then $XDG_CONFIG_HOME/truce/config.toml, then \
liquid/d1-ultra. That model is called via OpenRouter System One \
(POST /api/v1/systemone) and returns a choice, not generated text.\n\n\
Code model (--code-model): --code-model, then TRUCE_CODE_MODEL, then \
.truce.toml code_model, then the user config, then \
~deepseek/deepseek-flash-latest. A hunk escalates to this chat model when \
the decision is needs_code, confidence is below --min-confidence, the \
choice is unknown, the decision API errors, or the hunk does not fit the \
32k decision context.\n\n\
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
`truce sync` uses the same 0 / 1 / 2 codes. Exit 1 is an abort that restores \
HEAD (unresolved, low confidence, binary, modify/delete, rename, or a failed \
--verify-cmd). Exit 2 is a refusal or error (dirty tree, detached HEAD, merge \
already in progress, git or API failure). A failed --push keeps the local \
commit and still exits 2.\n\n\
JSON schema:\n  \
--json emits a single object with schema_version = 1. See README.md for the \
full schema. `truce sync --json` emits a sync report, also schema_version 1, \
with \"command\": \"sync\"."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

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

    #[command(flatten)]
    pub models: ModelArgs,

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

/// `truce sync` fetches a base, merges or rebases it, and resolves conflicts.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Fetch the base, merge or rebase it into HEAD, and resolve conflicts.
    Sync(SyncArgs),
    /// Download and install a GitHub Release of this binary.
    Update(UpdateArgs),
}

/// Flags for `truce sync`.
///
/// Merge is the default. `--rebase` replays the branch onto the base and is
/// experimental: each conflicted commit is resolved, then `git rebase
/// --continue` keeps that commit's original message. A failure runs
/// `git rebase --abort` and restores the original HEAD.
#[derive(Debug, Args)]
#[command(after_help = "Exit codes:\n  \
0  Up to date, fast-forward, clean merge, clean rebase, or conflicts resolved and committed\n  \
1  Aborted and HEAD restored: unresolved, below --min-confidence after escalation, \
binary, modify/delete, rename/delete, rename/rename, or --verify-cmd failed\n  \
2  Refused or error: dirty tree, detached HEAD, merge already in progress, \
missing git identity, fetch failure, API failure, or push failure\n\n\
A clean fast-forward moves HEAD and does not create a commit. A clean merge of \
diverged histories creates git's ordinary merge commit and no second commit. \
Truce writes its own commit message only when it resolves conflicts.\n\n\
Push runs only with --push, and only after the sync commit succeeds. A push \
failure keeps that commit and exits 2.\n\n\
--verify-cmd runs after resolution and before the commit, through sh -c \
(or cmd /C on Windows). A non-zero status aborts the merge or rebase, \
restores HEAD, and exits 1.")]
pub struct SyncArgs {
    /// Base ref to fetch and integrate. Default: origin/main.
    ///
    /// When this ref does not exist, Truce uses the remote's default branch
    /// (`origin/HEAD` after `git remote set-head --auto`).
    #[arg(long, value_name = "REF", default_value = "origin/main")]
    pub base: String,

    /// Rebase onto the base instead of merging. Experimental.
    ///
    /// Conflicts are resolved per commit. Commit messages stay the originals.
    /// The JSON report lists every resolved hunk. On failure, `git rebase --abort`
    /// restores the original HEAD.
    #[arg(long)]
    pub rebase: bool,

    /// Push the current branch to origin after a successful sync. Never force-pushes.
    #[arg(long)]
    pub push: bool,

    /// Shell command run after resolution and before the commit.
    ///
    /// Unix: `sh -c <cmd>`. Windows: `cmd /C <cmd>`. A non-zero exit aborts
    /// and restores HEAD (exit 1).
    #[arg(long, value_name = "CMD")]
    pub verify_cmd: Option<String>,

    /// Emit the sync JSON report to stdout (schema_version 1, command "sync").
    #[arg(long)]
    pub json: bool,

    #[command(flatten)]
    pub models: ModelArgs,

    /// Lines of surrounding context to include with each hunk.
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub context: usize,

    /// Disable ANSI color even when stdout is a TTY.
    ///
    /// Sync's human report is plain text either way.
    #[arg(long)]
    pub no_color: bool,

    /// GitHub Actions CI mode (also auto-detected from GITHUB_ACTIONS=true).
    #[arg(long)]
    pub ci: bool,
}

/// Decision-model flags shared by `truce` and `truce sync`.
#[derive(Debug, Clone, Args)]
pub struct ModelArgs {
    /// OpenRouter System One decision model (overrides TRUCE_MODEL and config).
    ///
    /// Default: liquid/d1-ultra. Called via POST /api/v1/systemone, not chat
    /// completions. The model returns a choice (ours, theirs, both, or
    /// needs_code), not generated text.
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Chat model used when a hunk needs generated code.
    ///
    /// Overrides TRUCE_CODE_MODEL and config. Default:
    /// ~deepseek/deepseek-flash-latest. Used for needs_code, low confidence,
    /// unknown choices, decision-API errors, and hunks that do not fit the
    /// 32k decision context.
    #[arg(long, value_name = "MODEL")]
    pub code_model: Option<String>,

    /// Accept a decision at or above this confidence (0–1). Default: 0.7.
    ///
    /// Overrides TRUCE_MIN_CONFIDENCE and config. A side choice below this
    /// threshold escalates the hunk to the code model. After escalation, a
    /// result still below this threshold is unresolved.
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
}

/// Flags for `truce update`.
///
/// With no flags, the command installs the latest GitHub Release when it is
/// newer than this binary. The archive checksum is verified before the
/// running binary is replaced.
#[derive(Debug, Args)]
#[command(after_help = "Exit codes:\n  \
0  Already up to date, --check printed the result, or the binary was replaced\n  \
2  The release could not be downloaded, verified, or installed\n\n\
When this binary is already the latest release, or already the tag passed to \
--version, truce prints a message and exits 0 without downloading.\n\n\
A newer release is chosen from the published assets: linux x64, linux arm64, \
macOS arm64, macOS x64, and windows x64. The archive is checked against \
SHA256SUMS before it is extracted. The running binary is replaced only after \
that check. On Windows the running .exe is renamed aside and the new .exe \
takes its place.\n\n\
If the install directory is not writable, the command exits 2 and tells you \
to re-run with sudo (or an elevated terminal on Windows).")]
pub struct UpdateArgs {
    /// Check for an update and print the result. Do not download or install.
    #[arg(long)]
    pub check: bool,

    /// Install this release tag instead of the latest (for example v0.2.0).
    #[arg(long, value_name = "TAG")]
    pub version: Option<String>,
}

impl SyncArgs {
    /// `--ci` or `GITHUB_ACTIONS=true`.
    pub fn ci_mode(&self) -> bool {
        self.ci || crate::ci::env_true("GITHUB_ACTIONS")
    }
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
        assert!(help.contains("--code-model"), "{help}");
        assert!(help.contains("liquid/d1-ultra"), "{help}");
        assert!(help.contains("TRUCE_CODE_MODEL"), "{help}");
        assert!(help.contains("--stage"), "{help}");
        assert!(help.contains("OPENROUTER_API_KEY"), "{help}");
        assert!(help.contains("NO_COLOR"), "{help}");
        assert!(help.contains("--ci"), "{help}");
        assert!(help.contains("GITHUB_ACTIONS"), "{help}");
        assert!(help.contains("sync"), "{help}");
        assert!(help.contains("update"), "{help}");
        assert!(help.contains("truce update"), "{help}");
    }

    #[test]
    fn update_subcommand_parses_check_and_version() {
        let cli = Cli::parse_from(["truce", "update", "--check", "--version", "v1.2.3"]);
        let Some(Command::Update(args)) = cli.command else {
            panic!("expected update");
        };
        assert!(args.check);
        assert_eq!(args.version.as_deref(), Some("v1.2.3"));
    }

    #[test]
    fn update_help_documents_check_checksum_and_permissions() {
        let mut cmd = Cli::command();
        let update = cmd
            .find_subcommand_mut("update")
            .expect("update subcommand");
        let mut buf = Vec::new();
        update.write_long_help(&mut buf).unwrap();
        let help = String::from_utf8(buf).unwrap();
        assert!(help.contains("--check"), "{help}");
        assert!(help.contains("--version"), "{help}");
        assert!(help.contains("SHA256SUMS"), "{help}");
        assert!(help.contains("sudo"), "{help}");
        assert!(help.contains("without downloading"), "{help}");
    }

    #[test]
    fn sync_subcommand_parses_its_flags() {
        let cli = Cli::parse_from([
            "truce",
            "sync",
            "--base",
            "origin/dev",
            "--rebase",
            "--push",
            "--verify-cmd",
            "cargo test",
            "--min-confidence",
            "0.5",
            "--model",
            "m",
            "--code-model",
            "c",
            "--json",
        ]);
        let Some(Command::Sync(args)) = cli.command else {
            panic!("expected sync");
        };
        assert_eq!(args.base, "origin/dev");
        assert!(args.rebase);
        assert!(args.push);
        assert_eq!(args.verify_cmd.as_deref(), Some("cargo test"));
        assert_eq!(args.models.min_confidence, Some(0.5));
        assert_eq!(args.models.model.as_deref(), Some("m"));
        assert_eq!(args.models.code_model.as_deref(), Some("c"));
        assert!(args.json);
        assert!(!args.ci);
    }

    #[test]
    fn sync_defaults_to_merge_origin_main() {
        let cli = Cli::parse_from(["truce", "sync"]);
        let Some(Command::Sync(args)) = cli.command else {
            panic!("expected sync");
        };
        assert_eq!(args.base, "origin/main");
        assert!(!args.rebase);
        assert!(!args.push);
        assert!(args.verify_cmd.is_none());
        assert_eq!(args.context, 3);
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
