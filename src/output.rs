use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cli::Cli;
use crate::parse::{Hunk, LineEnding};
use crate::resolve::{
    AttemptInfo, DecisionInfo, EscalationReason, HunkOutcome, Resolution, ResolverPath,
};

pub const SCHEMA_VERSION: u32 = 1;

/// Why an unmerged path was not parsed into hunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Conflicted,
    Binary,
    NonUtf8,
    Missing,
    ParseError,
    UnmergedWithoutMarkers,
}

impl FileStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Conflicted => "CONFLICTED",
            Self::Binary => "SKIPPED",
            Self::NonUtf8 => "SKIPPED",
            Self::Missing => "SKIPPED",
            Self::ParseError => "PARSE ERROR",
            Self::UnmergedWithoutMarkers => "UNMERGED",
        }
    }

    pub fn is_skipped(self) -> bool {
        matches!(self, Self::Binary | Self::NonUtf8 | Self::Missing)
    }

    pub fn is_fatal(self) -> bool {
        matches!(self, Self::ParseError)
    }
}

/// One scanned path, ready to serialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReport {
    /// Repository-relative path, using `/` separators in JSON.
    pub path: String,
    pub status: FileStatus,
    pub line_ending: LineEnding,
    pub trailing_newline: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub hunks: Vec<HunkReport>,
}

/// A hunk plus an optional resolution or rejection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HunkReport {
    #[serde(flatten)]
    pub hunk: Hunk,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection: Option<String>,
    /// System One answer, when the decision API returned one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionInfo>,
    /// `decision` when Rust applied a choice, `code` when chat completions ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolver: Option<ResolverPath>,
    /// Why this hunk was sent to the code model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalation_reason: Option<EscalationReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub conflicted_files: usize,
    pub hunks: usize,
    pub skipped_files: usize,
    pub resolved_hunks: usize,
    pub unresolved_hunks: usize,
    pub parse_errors: usize,
    pub files_written: usize,
    pub files_staged: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub tool: String,
    pub repo_root: String,
    pub merge_in_progress: bool,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Chat model used for code-generation escalation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_model: Option<String>,
    pub min_confidence: f64,
    pub files: Vec<FileReport>,
    pub summary: Summary,
}

/// In-memory scan of one path, including original lines for `--apply`.
#[derive(Debug, Clone)]
pub struct FileScan {
    pub rel_path: PathBuf,
    pub status: FileStatus,
    pub line_ending: LineEnding,
    pub trailing_newline: bool,
    pub lines: Vec<String>,
    pub hunks: Vec<Hunk>,
    pub error: Option<String>,
    pub outcomes: Vec<HunkOutcome>,
    pub attempts: Vec<AttemptInfo>,
}

impl FileScan {
    pub fn to_report(&self) -> FileReport {
        FileReport {
            path: self.rel_path.to_string_lossy().replace('\\', "/"),
            status: self.status,
            line_ending: self.line_ending,
            trailing_newline: self.trailing_newline,
            error: self.error.clone(),
            hunks: self
                .hunks
                .iter()
                .zip(self.outcomes.iter())
                .enumerate()
                .map(|(index, (hunk, outcome))| {
                    let attempt = self.attempts.get(index).cloned().unwrap_or_default();
                    HunkReport {
                        hunk: hunk.clone(),
                        resolution: outcome.resolution().cloned(),
                        rejection: outcome.rejection().map(str::to_string),
                        decision: attempt.decision,
                        resolver: attempt.resolver,
                        escalation_reason: attempt.escalation_reason,
                    }
                })
                .collect(),
        }
    }

    pub fn parsed(&self) -> crate::parse::ParsedFile {
        crate::parse::ParsedFile {
            lines: self.lines.clone(),
            line_ending: self.line_ending,
            trailing_newline: self.trailing_newline,
            hunks: self.hunks.clone(),
        }
    }
}

impl Report {
    #[allow(clippy::too_many_arguments)]
    pub fn from_scans(
        repo_root: &std::path::Path,
        merge_in_progress: bool,
        dry_run: bool,
        model: Option<String>,
        code_model: Option<String>,
        min_confidence: f64,
        scans: &[FileScan],
        files_written: usize,
        files_staged: usize,
    ) -> Self {
        let files: Vec<FileReport> = scans.iter().map(FileScan::to_report).collect();
        let conflicted_files = files
            .iter()
            .filter(|f| {
                matches!(
                    f.status,
                    FileStatus::Conflicted | FileStatus::UnmergedWithoutMarkers
                )
            })
            .count();
        let skipped_files = files.iter().filter(|f| f.status.is_skipped()).count();
        let parse_errors = files.iter().filter(|f| f.status.is_fatal()).count();
        let hunks = files.iter().map(|f| f.hunks.len()).sum();
        let resolved_hunks = files
            .iter()
            .flat_map(|f| f.hunks.iter())
            .filter(|h| h.resolution.is_some())
            .count();
        let unresolved_hunks = hunks - resolved_hunks;

        Self {
            schema_version: SCHEMA_VERSION,
            tool: "truce".to_string(),
            repo_root: repo_root.display().to_string(),
            merge_in_progress,
            dry_run,
            model,
            code_model,
            min_confidence,
            files,
            summary: Summary {
                conflicted_files,
                hunks,
                skipped_files,
                resolved_hunks,
                unresolved_hunks,
                parse_errors,
                files_written,
                files_staged,
            },
        }
    }

    /// Exit code per the CLI contract.
    ///
    /// * 2 — parse errors (runtime failure after a report was produced)
    /// * 1 — unresolved hunks or skipped unmerged files remain
    /// * 0 — nothing left to resolve
    pub fn exit_code(&self) -> i32 {
        if self.summary.parse_errors > 0 {
            2
        } else if self.summary.unresolved_hunks > 0
            || self.summary.skipped_files > 0
            || self
                .files
                .iter()
                .any(|f| f.status == FileStatus::UnmergedWithoutMarkers)
        {
            1
        } else {
            0
        }
    }
}

pub fn wants_color(cli: &Cli) -> bool {
    if cli.no_color || cli.json || cli.ci_mode() {
        return false;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if matches!(std::env::var("TERM").as_deref(), Ok("dumb")) {
        return false;
    }
    io::stdout().is_terminal()
}

struct Palette {
    enabled: bool,
}

impl Palette {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    fn ours(&self, text: &str) -> String {
        self.paint("32", text)
    }

    fn theirs(&self, text: &str) -> String {
        self.paint("31", text)
    }

    fn base(&self, text: &str) -> String {
        self.paint("36", text)
    }

    fn skip(&self, text: &str) -> String {
        self.paint("33", text)
    }

    fn error(&self, text: &str) -> String {
        self.paint("1;31", text)
    }

    fn resolved(&self, text: &str) -> String {
        self.paint("1;32", text)
    }

    fn rejected(&self, text: &str) -> String {
        self.paint("1;33", text)
    }
}

/// Print the report. JSON is compact-pretty; human form uses labels, not color alone.
pub fn print_report(report: &Report, cli: &Cli) -> io::Result<()> {
    if crate::ci::enabled(cli) {
        crate::ci::emit(report)?;
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if cli.json {
        serde_json::to_writer_pretty(&mut out, report)?;
        writeln!(out)?;
        return Ok(());
    }
    write_human(&mut out, report, wants_color(cli))
}

fn write_human(out: &mut dyn Write, report: &Report, color: bool) -> io::Result<()> {
    let p = Palette { enabled: color };
    let s = &report.summary;

    writeln!(
        out,
        "{}",
        p.bold(&format!(
            "truce  files={}  hunks={}  skipped={}  resolved={}  parse_errors={}",
            report.files.len(),
            s.hunks,
            s.skipped_files,
            s.resolved_hunks,
            s.parse_errors
        ))
    )?;
    writeln!(
        out,
        "merge_in_progress={}  dry_run={}  repo={}",
        yes_no(report.merge_in_progress),
        yes_no(report.dry_run),
        report.repo_root
    )?;
    if let Some(model) = &report.model {
        writeln!(out, "decision_model={model}")?;
    }
    if let Some(code) = &report.code_model {
        writeln!(out, "code_model={code}")?;
    }
    writeln!(out, "min_confidence={:.2}", report.min_confidence)?;
    writeln!(out)?;

    if report.files.is_empty() {
        if report.merge_in_progress {
            writeln!(out, "No unmerged paths. Nothing to resolve.")?;
        } else {
            writeln!(
                out,
                "No merge in progress. Working tree has no unmerged paths."
            )?;
        }
        return Ok(());
    }

    for file in &report.files {
        writeln!(out, "{}", p.bold(&file.path))?;
        let status_text = match file.status {
            FileStatus::Binary => p.skip("SKIPPED  binary file"),
            FileStatus::NonUtf8 => p.skip("SKIPPED  non-UTF-8 file"),
            FileStatus::Missing => p.skip("SKIPPED  missing working-tree file"),
            FileStatus::ParseError => p.error(&format!(
                "PARSE ERROR  {}",
                file.error
                    .as_deref()
                    .unwrap_or("malformed conflict markers")
            )),
            FileStatus::UnmergedWithoutMarkers => {
                p.skip("UNMERGED  no conflict markers in the working tree")
            }
            FileStatus::Conflicted => format!(
                "CONFLICTED  {} hunk{}",
                file.hunks.len(),
                if file.hunks.len() == 1 { "" } else { "s" }
            ),
        };
        writeln!(out, "  {status_text}")?;

        for h in &file.hunks {
            let hunk = &h.hunk;
            let style = if hunk.base.is_some() {
                "diff3"
            } else {
                "2-way"
            };
            writeln!(
                out,
                "  hunk {}  lines {}–{}  ({style})",
                hunk.id, hunk.start_line, hunk.end_line
            )?;
            if let Some(res) = &h.resolution {
                write!(
                    out,
                    "    {}  choice={}  confidence={:.2}",
                    p.resolved("RESOLVED"),
                    res.choice,
                    res.confidence
                )?;
                if let Some(resolver) = h.resolver {
                    write!(out, "  resolver={resolver}")?;
                }
                if let Some(reason) = h.escalation_reason {
                    write!(out, "  escalation={reason}")?;
                }
                writeln!(out)?;
                if !res.rationale.trim().is_empty() {
                    writeln!(out, "    rationale: {}", res.rationale.trim())?;
                }
                write_diff(out, &p, hunk, &res.text)?;
            } else {
                writeln!(
                    out,
                    "    {}  {}",
                    p.ours("OURS"),
                    label_or_empty(&hunk.ours_label)
                )?;
                write_block(out, "    ", &hunk.ours)?;
                if let (Some(label), Some(lines)) = (&hunk.base_label, &hunk.base) {
                    writeln!(out, "    {}  {}", p.base("BASE"), label_or_empty(label))?;
                    write_block(out, "    ", lines)?;
                }
                writeln!(
                    out,
                    "    {}  {}",
                    p.theirs("THEIRS"),
                    label_or_empty(&hunk.theirs_label)
                )?;
                write_block(out, "    ", &hunk.theirs)?;
                if let Some(reason) = &h.rejection {
                    write!(out, "    {}", p.rejected("REJECTED"))?;
                    if let Some(resolver) = h.resolver {
                        write!(out, "  resolver={resolver}")?;
                    }
                    if let Some(escalation) = h.escalation_reason {
                        write!(out, "  escalation={escalation}")?;
                    }
                    writeln!(out, "  {reason}")?;
                }
            }
        }
        writeln!(out)?;
    }

    if s.parse_errors > 0 {
        writeln!(
            out,
            "{}",
            p.error("Parse errors present; nothing was written.")
        )?;
    } else if s.unresolved_hunks > 0 || s.skipped_files > 0 {
        writeln!(
            out,
            "{} hunk(s) unresolved, {} file(s) skipped. Dry-run is the default; pass --apply to write files whose hunks all resolved.",
            s.unresolved_hunks, s.skipped_files
        )?;
    } else {
        writeln!(out, "No unresolved conflicts.")?;
    }

    if report.dry_run && s.resolved_hunks > 0 {
        writeln!(
            out,
            "Dry-run preview only; pass --apply to write fully resolved files."
        )?;
    }
    if s.files_written > 0 {
        writeln!(out, "Wrote {} file(s).", s.files_written)?;
    }
    if s.files_staged > 0 {
        writeln!(out, "Staged {} file(s).", s.files_staged)?;
    }

    Ok(())
}

fn write_diff(out: &mut dyn Write, p: &Palette, hunk: &Hunk, resolved: &str) -> io::Result<()> {
    let resolved_lines: Vec<String> = {
        let t = resolved.replace("\r\n", "\n");
        let trailing = t.ends_with('\n');
        let mut lines: Vec<String> = t.split('\n').map(str::to_string).collect();
        if trailing {
            lines.pop();
        }
        if lines.len() == 1 && lines[0].is_empty() && resolved.is_empty() {
            lines.clear();
        }
        lines
    };
    let same_ours = resolved_lines == hunk.ours;
    let same_theirs = resolved_lines == hunk.theirs;
    if same_ours {
        writeln!(out, "    (identical to OURS)")?;
        return Ok(());
    }
    if same_theirs {
        writeln!(out, "    (identical to THEIRS)")?;
        return Ok(());
    }
    writeln!(out, "    --- {}", p.ours("OURS"))?;
    if hunk.ours.is_empty() {
        writeln!(out, "    - (empty)")?;
    } else {
        for line in &hunk.ours {
            writeln!(out, "    - {line}")?;
        }
    }
    writeln!(out, "    +++ {}", p.resolved("RESOLVED"))?;
    if resolved_lines.is_empty() {
        writeln!(out, "    + (empty)")?;
    } else {
        for line in &resolved_lines {
            writeln!(out, "    + {line}")?;
        }
    }
    Ok(())
}

fn write_block(out: &mut dyn Write, indent: &str, lines: &[String]) -> io::Result<()> {
    if lines.is_empty() {
        writeln!(out, "{indent}  (empty)")?;
        return Ok(());
    }
    for line in lines {
        writeln!(out, "{indent}  {line}")?;
    }
    Ok(())
}

fn label_or_empty(label: &str) -> &str {
    if label.is_empty() {
        "(none)"
    } else {
        label
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// Map `BrokenPipe` to success so `truce | head` is a good Unix citizen.
pub fn ignore_broken_pipe(err: io::Error) -> crate::error::Result<()> {
    if err.kind() == io::ErrorKind::BrokenPipe {
        Ok(())
    } else {
        Err(err.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_conflicts;

    #[test]
    fn exit_zero_when_empty() {
        let report = Report::from_scans(
            std::path::Path::new("/repo"),
            false,
            true,
            None,
            None,
            0.7,
            &[],
            0,
            0,
        );
        assert_eq!(report.exit_code(), 0);
        assert_eq!(report.schema_version, 1);
    }

    #[test]
    fn exit_one_when_hunks_remain() {
        let parsed = parse_conflicts("<<<<<<< A\n1\n=======\n2\n>>>>>>> B\n", 0).unwrap();
        let scan = FileScan {
            rel_path: PathBuf::from("f.txt"),
            status: FileStatus::Conflicted,
            line_ending: parsed.line_ending,
            trailing_newline: parsed.trailing_newline,
            lines: parsed.lines,
            outcomes: vec![HunkOutcome::Unresolved],
            attempts: Vec::new(),
            hunks: parsed.hunks,
            error: None,
        };
        let report = Report::from_scans(
            std::path::Path::new("/repo"),
            true,
            true,
            None,
            None,
            0.7,
            &[scan],
            0,
            0,
        );
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.summary.hunks, 1);
    }

    #[test]
    fn json_round_trip_stable_keys() {
        let parsed =
            parse_conflicts("<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feat\n", 0).unwrap();
        let scan = FileScan {
            rel_path: PathBuf::from("src/app.rs"),
            status: FileStatus::Conflicted,
            line_ending: parsed.line_ending,
            trailing_newline: parsed.trailing_newline,
            lines: parsed.lines,
            outcomes: vec![HunkOutcome::Unresolved],
            attempts: Vec::new(),
            hunks: parsed.hunks,
            error: None,
        };
        let report = Report::from_scans(
            std::path::Path::new("/repo"),
            true,
            true,
            None,
            None,
            0.7,
            &[scan],
            0,
            0,
        );
        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"path\": \"src/app.rs\""));
        assert!(json.contains("\"ours_label\": \"HEAD\""));
        let back: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(back.files[0].hunks[0].hunk.theirs, ["theirs"]);
    }
}
