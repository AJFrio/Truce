//! GitHub Actions CI mode: annotations and job summaries.

use std::env;
use std::fs::OpenOptions;
use std::io::{self, Write};

use crate::cli::Cli;
use crate::output::{FileStatus, Report};

/// True when `--ci` was passed or `GITHUB_ACTIONS=true`.
pub fn enabled(cli: &Cli) -> bool {
    cli.ci || env_true("GITHUB_ACTIONS")
}

pub fn env_true(name: &str) -> bool {
    matches!(env::var(name).ok().as_deref(), Some("true") | Some("1"))
}

/// Escape a workflow-command **message** (after the second `::`).
pub fn escape_data(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Escape a workflow-command **property** value (`file=`, `title=`, …).
pub fn escape_property(s: &str) -> String {
    escape_data(s).replace(':', "%3A").replace(',', "%2C")
}

fn annotation(
    kind: &str,
    path: &str,
    line: usize,
    end_line: Option<usize>,
    title: &str,
    message: &str,
) -> String {
    let mut props = format!(
        "file={},line={},title={}",
        escape_property(path),
        line.max(1),
        escape_property(title)
    );
    if let Some(end) = end_line {
        if end > line {
            props.push_str(&format!(",endLine={end}"));
        }
    }
    format!("::{kind} {props}::{}", escape_data(message))
}

/// GitHub workflow commands for one report (`::error` / `::warning`).
pub fn annotations(report: &Report) -> Vec<String> {
    let mut out = Vec::new();
    for file in &report.files {
        match file.status {
            FileStatus::Binary => out.push(annotation(
                "warning",
                &file.path,
                1,
                None,
                "Skipped binary file",
                file.error
                    .as_deref()
                    .unwrap_or("binary file cannot be merged by truce"),
            )),
            FileStatus::NonUtf8 => out.push(annotation(
                "warning",
                &file.path,
                1,
                None,
                "Skipped non-UTF-8 file",
                file.error.as_deref().unwrap_or("file is not valid UTF-8"),
            )),
            FileStatus::Missing => out.push(annotation(
                "warning",
                &file.path,
                1,
                None,
                "Missing working-tree file",
                file.error
                    .as_deref()
                    .unwrap_or("working-tree file is missing"),
            )),
            FileStatus::ParseError => out.push(annotation(
                "error",
                &file.path,
                1,
                None,
                "Malformed conflict markers",
                file.error
                    .as_deref()
                    .unwrap_or("malformed or unterminated conflict markers"),
            )),
            FileStatus::UnmergedWithoutMarkers => out.push(annotation(
                "warning",
                &file.path,
                1,
                None,
                "Unmerged without markers",
                file.error
                    .as_deref()
                    .unwrap_or("unmerged index entry but no conflict markers"),
            )),
            FileStatus::Conflicted => {
                for h in &file.hunks {
                    if let Some(reason) = &h.rejection {
                        out.push(annotation(
                            "error",
                            &file.path,
                            h.hunk.start_line,
                            Some(h.hunk.end_line),
                            "Rejected resolution",
                            reason,
                        ));
                    } else if h.resolution.is_none() {
                        out.push(annotation(
                            "error",
                            &file.path,
                            h.hunk.start_line,
                            Some(h.hunk.end_line),
                            "Unresolved merge conflict",
                            &format!(
                                "hunk {} still unresolved (ours={}, theirs={})",
                                h.hunk.id, h.hunk.ours_label, h.hunk.theirs_label
                            ),
                        ));
                    }
                }
            }
        }
    }
    out
}

/// Markdown for `$GITHUB_STEP_SUMMARY`.
pub fn summary_markdown(report: &Report) -> String {
    let s = &report.summary;
    let mut md = String::new();
    md.push_str("## Truce\n\n");
    md.push_str(&format!(
        "| Files | Hunks | Resolved | Unresolved | Skipped | Written |\n\
         | ---: | ---: | ---: | ---: | ---: | ---: |\n\
         | {} | {} | {} | {} | {} | {} |\n\n",
        report.files.len(),
        s.hunks,
        s.resolved_hunks,
        s.unresolved_hunks,
        s.skipped_files,
        s.files_written
    ));
    if let Some(model) = &report.model {
        md.push_str(&format!("Model: `{model}`  \n"));
    }
    md.push_str(&format!(
        "Dry-run: **{}**  \nExit: `{}`\n\n",
        if report.dry_run { "yes" } else { "no" },
        report.exit_code()
    ));

    if report.files.is_empty() {
        md.push_str("No unmerged paths.\n");
        return md;
    }

    md.push_str("| File | Status | Hunks | Notes |\n| --- | --- | ---: | --- |\n");
    for file in &report.files {
        let notes = match file.status {
            FileStatus::Conflicted => {
                let rejected = file.hunks.iter().filter(|h| h.rejection.is_some()).count();
                let resolved = file.hunks.iter().filter(|h| h.resolution.is_some()).count();
                format!("{resolved} resolved, {rejected} rejected")
            }
            FileStatus::ParseError => file.error.clone().unwrap_or_else(|| "parse error".into()),
            _ => file.error.clone().unwrap_or_default(),
        };
        let notes = notes.replace('|', "\\|");
        md.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            file.path,
            file.status.label(),
            file.hunks.len(),
            notes
        ));
    }
    md.push('\n');
    md
}

/// Write annotations to stderr and append the job summary if configured.
pub fn emit(report: &Report) -> io::Result<()> {
    let mut err = io::stderr().lock();
    for line in annotations(report) {
        writeln!(err, "{line}")?;
    }
    if let Some(path) = env::var_os("GITHUB_STEP_SUMMARY") {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(summary_markdown(report).as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{FileReport, FileStatus, HunkReport, Summary};
    use crate::parse::{parse_conflicts, LineEnding};

    fn conflict_report() -> Report {
        let parsed = parse_conflicts("<<<<<<< A\n1\n=======\n2\n>>>>>>> B\n", 0).unwrap();
        let hunk = parsed.hunks[0].clone();
        Report {
            schema_version: 1,
            tool: "truce".into(),
            repo_root: "/repo".into(),
            merge_in_progress: true,
            dry_run: true,
            model: Some("openai/gpt-4o-mini".into()),
            files: vec![FileReport {
                path: "src/app.rs".into(),
                status: FileStatus::Conflicted,
                line_ending: LineEnding::Lf,
                trailing_newline: true,
                error: None,
                hunks: vec![HunkReport {
                    hunk,
                    resolution: None,
                    rejection: None,
                }],
            }],
            summary: Summary {
                conflicted_files: 1,
                hunks: 1,
                skipped_files: 0,
                resolved_hunks: 0,
                unresolved_hunks: 1,
                parse_errors: 0,
                files_written: 0,
                files_staged: 0,
            },
        }
    }

    #[test]
    fn annotation_includes_file_and_line() {
        let lines = annotations(&conflict_report());
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with("::error file=src/app.rs,line=1"),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].contains("Unresolved merge conflict"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("endLine="), "{}", lines[0]);
    }

    #[test]
    fn summary_is_markdown_table() {
        let md = summary_markdown(&conflict_report());
        assert!(md.contains("## Truce"), "{md}");
        assert!(md.contains("| File | Status |"), "{md}");
        assert!(md.contains("`src/app.rs`"), "{md}");
        assert!(md.contains("CONFLICTED"), "{md}");
        assert!(md.contains("openai/gpt-4o-mini"), "{md}");
    }

    #[test]
    fn escape_percent_first() {
        assert_eq!(escape_data("100%\nnext"), "100%25%0Anext");
        assert_eq!(escape_property("a:b,c"), "a%3Ab%2Cc");
    }

    #[test]
    fn skipped_binary_is_warning() {
        let mut report = conflict_report();
        report.files[0].status = FileStatus::Binary;
        report.files[0].hunks.clear();
        report.files[0].error = Some("binary file (contains NUL)".into());
        let lines = annotations(&report);
        assert!(lines[0].starts_with("::warning "), "{}", lines[0]);
        assert!(lines[0].contains("src/app.rs"), "{}", lines[0]);
    }
}
