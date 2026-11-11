//! Truce: find, parse, and resolve git merge conflicts in a working tree.

pub mod ci;
pub mod cli;
pub mod config;
pub mod error;
pub mod git;
pub mod openrouter;
pub mod output;
pub mod parse;
pub mod resolve;

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub use cli::Cli;
pub use error::{Result, TruceError};
pub use parse::{parse_conflicts, Hunk, LineEnding, ParsedFile};

use git::{filter_paths, GitRepo};
use openrouter::OpenRouterClient;
use output::{ignore_broken_pipe, print_report, FileScan, FileStatus, Report};
use parse::LineEnding as Eol;
use resolve::{reconstruct, write_atomic, HunkOutcome, Resolution};

/// Run truce with an already-parsed CLI. Returns the process exit code.
pub async fn run(cli: Cli) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let repo = GitRepo::discover(&cwd)?;
    let settings = config::load(&cli, &repo.root)?;
    let merge_in_progress = repo.merge_in_progress()?;
    let unmerged = repo.unmerged_paths()?;
    let selected = filter_paths(&repo.root, &cwd, unmerged, &cli.paths)?;

    let mut scans = Vec::with_capacity(selected.len());
    for rel in selected {
        scans.push(scan_file(&repo.root, rel, cli.context)?);
    }

    let has_hunks = scans
        .iter()
        .any(|s| s.status == FileStatus::Conflicted && !s.hunks.is_empty());

    if has_hunks {
        match settings.api_key.as_deref() {
            None if cli.should_apply() => return Err(TruceError::MissingApiKey),
            None => {
                if !cli.json {
                    eprintln!(
                        "note: OPENROUTER_API_KEY is unset; listing hunks without calling a model"
                    );
                }
            }
            Some(_) => {
                let client = OpenRouterClient::new(&settings)?;
                openrouter::resolve_scans(
                    &mut scans,
                    &client,
                    settings.concurrency,
                    settings.min_confidence,
                )
                .await?;
            }
        }
    }

    let dry_run = !cli.should_apply();
    let mut files_written = 0usize;
    let mut files_staged = 0usize;

    if cli.should_apply() {
        let written = write_resolved_files(&repo.root, &scans)?;
        files_written = written.len();
        if cli.stage && !written.is_empty() {
            repo.stage(&written)?;
            files_staged = written.len();
        }
    }

    let report = Report::from_scans(
        &repo.root,
        merge_in_progress,
        dry_run,
        Some(settings.model),
        &scans,
        files_written,
        files_staged,
    );
    match print_report(&report, &cli) {
        Ok(()) => Ok(report.exit_code()),
        Err(err) => {
            ignore_broken_pipe(err)?;
            Ok(report.exit_code())
        }
    }
}

fn scan_file(repo_root: &Path, rel: PathBuf, context: usize) -> Result<FileScan> {
    let abs = repo_root.join(&rel);
    match fs::read(&abs) {
        Ok(bytes) => classify_bytes(rel, bytes, context),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(FileScan {
            rel_path: rel,
            status: FileStatus::Missing,
            line_ending: Eol::Lf,
            trailing_newline: true,
            lines: Vec::new(),
            hunks: Vec::new(),
            error: Some("working-tree file is missing".into()),
            outcomes: Vec::new(),
        }),
        Err(source) => Err(TruceError::Read { path: abs, source }),
    }
}

fn classify_bytes(rel: PathBuf, bytes: Vec<u8>, context: usize) -> Result<FileScan> {
    if bytes.contains(&0) {
        return Ok(skipped(
            rel,
            FileStatus::Binary,
            "binary file (contains NUL)",
        ));
    }

    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return Ok(skipped(rel, FileStatus::NonUtf8, "file is not valid UTF-8"));
        }
    };

    match parse_conflicts(&text, context) {
        Ok(parsed) => {
            let status = if parsed.hunks.is_empty() {
                FileStatus::UnmergedWithoutMarkers
            } else {
                FileStatus::Conflicted
            };
            let n = parsed.hunks.len();
            Ok(FileScan {
                rel_path: rel,
                status,
                line_ending: parsed.line_ending,
                trailing_newline: parsed.trailing_newline,
                lines: parsed.lines,
                hunks: parsed.hunks,
                error: match status {
                    FileStatus::UnmergedWithoutMarkers => Some(
                        "unmerged index entry but no conflict markers in the working tree".into(),
                    ),
                    _ => None,
                },
                outcomes: vec![HunkOutcome::Unresolved; n],
            })
        }
        Err(message) => {
            let (lines, line_ending, trailing_newline) = parse::split_text(&text);
            Ok(FileScan {
                rel_path: rel,
                status: FileStatus::ParseError,
                line_ending,
                trailing_newline,
                lines,
                hunks: Vec::new(),
                error: Some(message),
                outcomes: Vec::new(),
            })
        }
    }
}

fn skipped(rel: PathBuf, status: FileStatus, reason: &str) -> FileScan {
    FileScan {
        rel_path: rel,
        status,
        line_ending: Eol::Lf,
        trailing_newline: true,
        lines: Vec::new(),
        hunks: Vec::new(),
        error: Some(reason.to_string()),
        outcomes: Vec::new(),
    }
}

fn write_resolved_files(repo_root: &Path, scans: &[FileScan]) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for scan in scans {
        if scan.status != FileStatus::Conflicted || scan.hunks.is_empty() {
            continue;
        }
        let resolutions: Vec<Option<Resolution>> = scan
            .outcomes
            .iter()
            .map(|o| o.resolution().cloned())
            .collect();
        if resolutions.iter().any(Option::is_none) {
            continue;
        }
        let parsed = scan.parsed();
        if let Some(body) = reconstruct(&parsed, &resolutions)? {
            write_atomic(&repo_root.join(&scan.rel_path), body.as_bytes())?;
            written.push(scan.rel_path.clone());
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_bytes_are_skipped() {
        let scan = classify_bytes(PathBuf::from("a.bin"), b"a\0b".to_vec(), 3).unwrap();
        assert_eq!(scan.status, FileStatus::Binary);
        assert!(scan.hunks.is_empty());
    }

    #[test]
    fn non_utf8_bytes_are_skipped() {
        let scan = classify_bytes(PathBuf::from("a.txt"), vec![0xff, 0xfe, 0xfd], 3).unwrap();
        assert_eq!(scan.status, FileStatus::NonUtf8);
    }

    #[test]
    fn parse_error_is_reported_not_panic() {
        let scan = classify_bytes(
            PathBuf::from("bad.txt"),
            b"<<<<<<< HEAD\nours\n".to_vec(),
            3,
        )
        .unwrap();
        assert_eq!(scan.status, FileStatus::ParseError);
        assert!(scan.error.as_ref().unwrap().contains("unterminated"));
    }
}
