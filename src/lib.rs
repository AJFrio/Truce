//! Truce: find and parse git merge conflicts in a working tree.

pub mod cli;
pub mod error;
pub mod git;
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
use output::{ignore_broken_pipe, print_report, FileScan, FileStatus, Report};
use parse::LineEnding as Eol;
use resolve::{reconstruct, write_atomic, NullResolver, Resolver};

/// Run truce with an already-parsed CLI. Returns the process exit code.
pub fn run(cli: Cli) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let repo = GitRepo::discover(&cwd)?;
    let merge_in_progress = repo.merge_in_progress()?;
    let unmerged = repo.unmerged_paths()?;
    let selected = filter_paths(&repo.root, &cwd, unmerged, &cli.paths)?;

    let mut scans = Vec::with_capacity(selected.len());
    for rel in selected {
        scans.push(scan_file(&repo.root, rel, cli.context)?);
    }

    let dry_run = !cli.should_apply();
    let mut files_written = 0usize;

    if cli.should_apply() {
        let mut resolver = NullResolver;
        files_written = apply_resolutions(&repo.root, &mut scans, &mut resolver)?;
    }

    let report = Report::from_scans(
        &repo.root,
        merge_in_progress,
        dry_run,
        &scans,
        files_written,
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
            resolutions: Vec::new(),
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
                resolutions: vec![None; n],
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
                resolutions: Vec::new(),
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
        resolutions: Vec::new(),
    }
}

fn apply_resolutions(
    repo_root: &Path,
    scans: &mut [FileScan],
    resolver: &mut dyn Resolver,
) -> Result<usize> {
    let mut written = 0usize;
    for scan in scans.iter_mut() {
        if scan.status != FileStatus::Conflicted || scan.hunks.is_empty() {
            continue;
        }
        let path_str = scan.rel_path.to_string_lossy().replace('\\', "/");
        for (hunk, slot) in scan.hunks.iter().zip(scan.resolutions.iter_mut()) {
            *slot = resolver.resolve(&path_str, hunk)?;
        }
        if scan.resolutions.iter().any(|r| r.is_none()) {
            continue;
        }
        let parsed = scan.parsed();
        if let Some(body) = reconstruct(&parsed, &scan.resolutions)? {
            write_atomic(&repo_root.join(&scan.rel_path), body.as_bytes())?;
            written += 1;
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
