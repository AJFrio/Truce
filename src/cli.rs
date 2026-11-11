use std::path::PathBuf;

use clap::Parser;

/// Resolve git merge conflicts using fast decision models.
///
/// Truce scans the current git working tree for unmerged files, parses
/// conflict hunks, and prints a report. Resolution is preview-only by
/// default (`--dry-run`). `--apply` writes accepted resolutions back to
/// the working tree when a resolver is configured.
#[derive(Debug, Parser)]
#[command(
    name = "truce",
    version,
    about = "Resolve git merge conflicts using fast decision models",
    long_about = "Truce finds merge conflicts in a git working tree, parses each \
conflict hunk (including optional diff3/zdiff3 base sections), and prints a \
human or JSON report.\n\n\
Dry-run is the default: nothing is written. Pass --apply to write resolutions \
once a resolver is available. Binary and non-UTF-8 files are skipped and \
reported, never rewritten.\n\n\
Color is used only when stdout is a TTY. It is disabled when --no-color is \
set, when NO_COLOR is present in the environment, or when TERM=dumb. Labels \
such as OURS/THEIRS/SKIPPED are always printed so the report is readable \
without color.",
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
    /// Truce does not `git add` the result.
    #[arg(long, overrides_with = "dry_run")]
    pub apply: bool,

    /// Lines of surrounding context to include with each hunk.
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub context: usize,

    /// Disable ANSI color even when stdout is a TTY.
    #[arg(long)]
    pub no_color: bool,

    /// Limit scanning to these paths (relative to the repo or cwd).
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
}

impl Cli {
    /// True when the user asked to write files (`--apply` and not `--dry-run`).
    pub fn should_apply(&self) -> bool {
        self.apply && !self.dry_run
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
        assert!(help.contains("NO_COLOR"), "{help}");
    }

    #[test]
    fn apply_is_off_by_default() {
        let cli = Cli::parse_from(["truce"]);
        assert!(!cli.should_apply());
        assert!(!cli.json);
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
