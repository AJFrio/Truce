//! `truce sync`: fetch a base, merge or rebase it, resolve conflicts, commit.
//!
//! Merge is the default. A fast-forward moves the branch and does not create a
//! commit. A clean merge of diverged histories keeps git's ordinary merge
//! commit and does not add a second one. Truce writes its own message only
//! when it resolves conflicts.
//!
//! `--rebase` is experimental. Each conflicted commit is resolved, then
//! `git rebase --continue` keeps that commit's original message. During a
//! rebase, OURS is the branch being replayed onto and THEIRS is the commit
//! being replayed.
//!
//! Any failure before a successful commit aborts with `git merge --abort` or
//! `git rebase --abort` and, if that is not enough, `git reset --hard` back to
//! the original HEAD. A failed `--push` is the exception: the local commit
//! stays, and the process exits 2.
//!
//! Exit codes: 0 ok or already up to date, 1 aborted (unresolved, low
//! confidence, binary, modify/delete, rename, or `--verify-cmd`), 2 refused
//! or error.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::ci;
use crate::cli::SyncArgs;
use crate::config::{self, Settings};
use crate::error::{Result, TruceError};
use crate::git::{output_message, GitRepo};
use crate::output::{
    ignore_broken_pipe, FileReport, FileScan, FileStatus, Report, Summary, SCHEMA_VERSION,
};
use crate::resolve::has_conflict_markers;

const MAX_REBASE_STEPS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    UpToDate,
    FastForward,
    CleanMerge,
    CleanRebase,
    Resolved,
    Aborted,
    Refused,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::UpToDate => "up_to_date",
            Self::FastForward => "fast_forward",
            Self::CleanMerge => "clean_merge",
            Self::CleanRebase => "clean_rebase",
            Self::Resolved => "resolved",
            Self::Aborted => "aborted",
            Self::Refused => "refused",
        }
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Strategy {
    Merge,
    Rebase,
}

impl Strategy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        }
    }
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize)]
struct ResolvedEntry {
    path: String,
    hunk: usize,
    resolver: String,
    confidence: f64,
    choice: String,
}

#[derive(Debug, Serialize)]
struct SyncReport {
    schema_version: u32,
    tool: &'static str,
    command: &'static str,
    repo_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    requested_base: String,
    base: String,
    strategy: Strategy,
    outcome: Outcome,
    exit_code: i32,
    aborted: bool,
    pushed: bool,
    merge_in_progress: bool,
    head: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    model: String,
    code_model: String,
    min_confidence: f64,
    resolved: Vec<ResolvedEntry>,
    files: Vec<FileReport>,
    summary: Summary,
}

struct Run {
    args: SyncArgs,
    repo: GitRepo,
    settings: Settings,
    branch: Option<String>,
    requested_base: String,
    base: String,
    strategy: Strategy,
    original_head: String,
    /// When set, [`Drop`] aborts the in-progress merge or rebase.
    active: bool,
}

impl Drop for Run {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            let _ = rollback(&self.repo, self.strategy, &self.original_head);
        }
    }
}

struct Halt {
    code: i32,
    error: String,
    scans: Vec<FileScan>,
    resolved: Vec<ResolvedEntry>,
}

struct Prepared {
    scans: Vec<FileScan>,
    resolved: Vec<ResolvedEntry>,
    written: usize,
    staged: usize,
}

enum Finished {
    Done {
        resolved: Vec<ResolvedEntry>,
        scans: Vec<FileScan>,
        written: usize,
        staged: usize,
    },
    Halt(Halt),
}

pub(crate) async fn run(args: SyncArgs) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let repo = GitRepo::discover(&cwd)?;
    let settings = config::load_models(&args.models, &repo.root)?;
    let strategy = if args.rebase {
        Strategy::Rebase
    } else {
        Strategy::Merge
    };
    let requested_base = args.base.clone();
    let branch = repo.current_branch()?;
    let original_head = repo.rev_parse("HEAD").unwrap_or_default();
    let mut run = Run {
        args,
        repo,
        settings,
        branch,
        requested_base: requested_base.clone(),
        base: requested_base,
        strategy,
        original_head,
        active: false,
    };

    if run.branch.is_none() {
        return run.refuse(2, "refusing to run on a detached HEAD");
    }
    if run.repo.merge_in_progress()? {
        return run.refuse(
            2,
            "a merge, rebase, cherry-pick, or revert is already in progress",
        );
    }
    if !run.repo.unmerged_paths()?.is_empty() {
        return run.refuse(
            2,
            "unmerged paths are already present; finish or abort that operation before truce sync",
        );
    }
    if run.repo.is_dirty()? {
        return run.refuse(2, "refusing to run with a dirty working tree");
    }
    if run.original_head.is_empty() {
        return run.refuse(2, "HEAD is not a commit");
    }

    match resolve_base(&run.repo, &run.requested_base) {
        Ok(base) => run.base = base,
        Err(err) => {
            return run.refuse(2, &format!("failed to resolve --base: {err}"));
        }
    }

    if run.repo.is_ancestor(&run.base, "HEAD")? {
        return run.finish(Outcome::UpToDate, None, &[], &[], 0, 0, true);
    }

    if let Err(err) = run.repo.require_identity() {
        return run.refuse(2, &err.to_string());
    }

    run.active = true;
    let finished = match run.integrate().await {
        Ok(finished) => finished,
        Err(err) => return run.fail(err),
    };

    match finished {
        Finished::Halt(halt) => run.halted(halt),
        Finished::Done {
            resolved,
            scans,
            written,
            staged,
            ..
        } => {
            let after = match run.repo.rev_parse("HEAD") {
                Ok(sha) => sha,
                Err(err) => return run.fail(err),
            };
            let base_sha = match run.repo.rev_parse(&run.base) {
                Ok(sha) => sha,
                Err(err) => return run.fail(err),
            };
            let outcome = if !resolved.is_empty() {
                Outcome::Resolved
            } else {
                classify_clean(&run.original_head, &after, &base_sha, run.strategy)
            };
            if matches!(
                outcome,
                Outcome::Resolved | Outcome::CleanMerge | Outcome::CleanRebase
            ) && after == run.original_head
            {
                return run.halted(Halt {
                    code: 2,
                    error: "sync finished without moving HEAD".into(),
                    scans,
                    resolved,
                });
            }
            let commit = match outcome {
                Outcome::CleanMerge | Outcome::CleanRebase | Outcome::Resolved => Some(after),
                _ => None,
            };
            // Keep the result. Drop must not abort it.
            run.active = false;
            run.finish(outcome, commit, &scans, &resolved, written, staged, true)
        }
    }
}

impl Run {
    async fn integrate(&self) -> Result<Finished> {
        if self.strategy == Strategy::Rebase {
            self.rebase().await
        } else {
            self.merge().await
        }
    }

    async fn merge(&self) -> Result<Finished> {
        let output = self.repo.merge_ref(&self.base)?;
        if output.status.success() {
            return Ok(Finished::Done {
                resolved: Vec::new(),
                scans: Vec::new(),
                written: 0,
                staged: 0,
            });
        }
        let unmerged = self.repo.unmerged_paths()?;
        if unmerged.is_empty() {
            return Ok(Finished::Halt(Halt {
                code: 2,
                error: format!("git merge failed: {}", output_message(&output)),
                scans: Vec::new(),
                resolved: Vec::new(),
            }));
        }
        match self.prepare(&output_message(&output)).await {
            Ok(prepared) => self.commit_merge(prepared),
            Err(halt) => Ok(Finished::Halt(halt)),
        }
    }

    fn commit_merge(&self, prepared: Prepared) -> Result<Finished> {
        let message = commit_message(&self.base, &prepared.resolved);
        let commit = self.repo.commit_with_message(&message)?;
        if !commit.status.success() {
            return Ok(Finished::Halt(Halt {
                code: 2,
                error: format!("git commit failed: {}", output_message(&commit)),
                scans: prepared.scans,
                resolved: prepared.resolved,
            }));
        }
        Ok(Finished::Done {
            resolved: prepared.resolved,
            scans: prepared.scans,
            written: prepared.written,
            staged: prepared.staged,
        })
    }

    async fn rebase(&self) -> Result<Finished> {
        let owned = editor_env()?;
        let env: Vec<(&str, &str)> = owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut output = self.repo.rebase_onto(&self.base, &env)?;
        let mut resolved = Vec::new();
        let mut scans = Vec::new();
        let mut written = 0usize;
        let mut staged = 0usize;
        let mut steps = 0usize;
        loop {
            if output.status.success() {
                return Ok(Finished::Done {
                    resolved,
                    scans,
                    written,
                    staged,
                });
            }
            steps += 1;
            if steps > MAX_REBASE_STEPS {
                return Ok(Finished::Halt(Halt {
                    code: 2,
                    error: format!("rebase exceeded {MAX_REBASE_STEPS} conflicted commits"),
                    scans,
                    resolved,
                }));
            }
            if self.repo.unmerged_paths()?.is_empty() {
                return Ok(Finished::Halt(Halt {
                    code: 2,
                    error: format!("git rebase failed: {}", output_message(&output)),
                    scans,
                    resolved,
                }));
            }
            match self.prepare(&output_message(&output)).await {
                Ok(prepared) => {
                    written += prepared.written;
                    staged += prepared.staged;
                    resolved.extend(prepared.resolved);
                    scans.extend(prepared.scans);
                    output = self.repo.rebase_continue(&env)?;
                }
                Err(halt) => {
                    return Ok(Finished::Halt(Halt {
                        code: halt.code,
                        error: halt.error,
                        scans: if halt.scans.is_empty() {
                            scans
                        } else {
                            halt.scans
                        },
                        resolved: {
                            let mut all = resolved;
                            all.extend(halt.resolved);
                            all
                        },
                    }));
                }
            }
        }
    }

    async fn prepare(&self, integration_output: &str) -> std::result::Result<Prepared, Halt> {
        let paths = self
            .repo
            .unmerged_paths()
            .map_err(|err| halt_err(2, format!("failed to list conflicts: {err}")))?;
        let mut scans = Vec::with_capacity(paths.len());
        for rel in paths {
            let scan = crate::scan_file(&self.repo.root, rel, self.args.context)
                .map_err(|err| halt_err(2, format!("failed to read a conflict: {err}")))?;
            scans.push(scan);
        }
        let porcelain = self
            .repo
            .status_porcelain_z()
            .map_err(|err| halt_err(2, format!("failed to read git status: {err}")))?;
        let blockers = collect_blockers(integration_output, &porcelain, &scans);
        if !blockers.is_empty() {
            return Err(Halt {
                code: blocker_code(&scans),
                error: blockers.join("; "),
                scans,
                resolved: Vec::new(),
            });
        }
        if self.settings.api_key.is_none() {
            return Err(Halt {
                code: 2,
                error: "OPENROUTER_API_KEY is not set (required to resolve conflicts)".into(),
                scans,
                resolved: Vec::new(),
            });
        }
        if let Err(err) = crate::engine::resolve_scans(&mut scans, &self.settings).await {
            let resolved = resolved_entries(&scans);
            return Err(Halt {
                code: 2,
                error: format!("escalation failed: {err}"),
                scans,
                resolved,
            });
        }
        let preview = file_report(&self.repo, &self.settings, &scans, 0, 0);
        if preview.exit_code() != 0 {
            let resolved = resolved_entries(&scans);
            return Err(Halt {
                code: preview.exit_code(),
                error: summarize_failures(&scans),
                scans,
                resolved,
            });
        }
        let written = match crate::write_resolved_files(&self.repo.root, &scans) {
            Ok(paths) => paths,
            Err(err) => {
                return Err(Halt {
                    code: 2,
                    error: format!("failed to write resolutions: {err}"),
                    resolved: resolved_entries(&scans),
                    scans,
                });
            }
        };
        let conflicted = conflicted_paths(&scans);
        if !same_paths(&written, &conflicted) {
            return Err(Halt {
                code: 2,
                error: "internal error: not every conflicted file was written".into(),
                resolved: resolved_entries(&scans),
                scans,
            });
        }
        match markers_remaining(&self.repo.root, &written) {
            Ok(Some(path)) => {
                return Err(Halt {
                    code: 1,
                    error: format!("{path} still contains conflict markers"),
                    resolved: resolved_entries(&scans),
                    scans,
                });
            }
            Ok(None) => {}
            Err(err) => {
                return Err(Halt {
                    code: 2,
                    error: err,
                    resolved: resolved_entries(&scans),
                    scans,
                });
            }
        }
        if let Err(err) = self.repo.stage(&written) {
            return Err(Halt {
                code: 2,
                error: format!("git add failed: {err}"),
                resolved: resolved_entries(&scans),
                scans,
            });
        }
        match self.repo.unmerged_paths() {
            Ok(left) if left.is_empty() => {}
            Ok(_) => {
                return Err(Halt {
                    code: 1,
                    error: "unmerged paths remain after resolution".into(),
                    resolved: resolved_entries(&scans),
                    scans,
                });
            }
            Err(err) => {
                return Err(Halt {
                    code: 2,
                    error: format!("failed to recheck conflicts: {err}"),
                    resolved: resolved_entries(&scans),
                    scans,
                });
            }
        }
        if let Some(cmd) = self.args.verify_cmd.clone() {
            if let Err(err) = run_verify(&self.repo.root, &cmd) {
                return Err(Halt {
                    code: err.code,
                    error: err.message,
                    resolved: resolved_entries(&scans),
                    scans,
                });
            }
        }
        let resolved = resolved_entries(&scans);
        let count = written.len();
        Ok(Prepared {
            scans,
            resolved,
            written: count,
            staged: count,
        })
    }

    fn refuse(&self, code: i32, error: &str) -> Result<i32> {
        let (report, _) = self.report(
            Outcome::Refused,
            code,
            Some(error.to_string()),
            false,
            None,
            &[],
            &[],
            0,
            0,
            false,
        );
        self.present(&report, None)
    }

    fn halted(&mut self, halt: Halt) -> Result<i32> {
        let restored = self.stop();
        let mut error = halt.error;
        let mut code = halt.code;
        let aborted = match restored {
            Ok(()) => true,
            Err(err) => {
                error = format!("{error}; failed to restore HEAD: {err}");
                code = 2;
                false
            }
        };
        let (report, files) = self.report(
            Outcome::Aborted,
            code,
            Some(error),
            aborted,
            None,
            &halt.scans,
            &halt.resolved,
            0,
            0,
            false,
        );
        self.present(&report, Some(&files))
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        outcome: Outcome,
        commit: Option<String>,
        scans: &[FileScan],
        resolved: &[ResolvedEntry],
        written: usize,
        staged: usize,
        allow_push: bool,
    ) -> Result<i32> {
        self.active = false;
        let (mut report, files) = self.report(
            outcome, 0, None, false, commit, scans, resolved, written, staged, false,
        );
        if allow_push && self.args.push {
            match self.repo.push_head() {
                Ok(()) => report.pushed = true,
                Err(err) => {
                    report.pushed = false;
                    report.exit_code = 2;
                    report.error = Some(format!("push failed: {err}"));
                }
            }
        }
        self.present(&report, Some(&files))
    }

    fn fail(&mut self, err: TruceError) -> Result<i32> {
        match self.stop() {
            Ok(()) => Err(err),
            Err(rollback) => Err(TruceError::Git {
                status: None,
                message: format!("{err}; failed to restore HEAD: {rollback}"),
            }),
        }
    }

    fn stop(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        match rollback(&self.repo, self.strategy, &self.original_head) {
            Ok(()) => {
                self.active = false;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn report(
        &self,
        outcome: Outcome,
        exit_code: i32,
        error: Option<String>,
        aborted: bool,
        commit: Option<String>,
        scans: &[FileScan],
        resolved: &[ResolvedEntry],
        written: usize,
        staged: usize,
        pushed: bool,
    ) -> (SyncReport, Report) {
        let merge_in_progress = self.repo.merge_in_progress().unwrap_or(false);
        let files = file_report_with(
            &self.repo,
            &self.settings,
            merge_in_progress,
            scans,
            written,
            staged,
        );
        let head = self.repo.rev_parse("HEAD").unwrap_or_else(|_| {
            if self.original_head.is_empty() {
                "unknown".into()
            } else {
                self.original_head.clone()
            }
        });
        let report = SyncReport {
            schema_version: SCHEMA_VERSION,
            tool: "truce",
            command: "sync",
            repo_root: self.repo.root.display().to_string(),
            branch: self.branch.clone(),
            requested_base: self.requested_base.clone(),
            base: self.base.clone(),
            strategy: self.strategy,
            outcome,
            exit_code,
            aborted,
            pushed,
            merge_in_progress,
            head,
            commit,
            error,
            model: self.settings.model.clone(),
            code_model: self.settings.code_model.clone(),
            min_confidence: self.settings.min_confidence,
            resolved: resolved.to_vec(),
            files: files.files.clone(),
            summary: files.summary.clone(),
        };
        (report, files)
    }

    fn present(&self, report: &SyncReport, files: Option<&Report>) -> Result<i32> {
        if self.args.ci_mode() {
            emit_ci(report, files)?;
        }
        if report.exit_code != 0 {
            if let Some(err) = &report.error {
                let _ = writeln!(io::stderr(), "error: {err}");
            }
        }
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let write_result = if self.args.json {
            match serde_json::to_writer_pretty(&mut out, report) {
                Ok(()) => writeln!(out),
                Err(err) => Err(io::Error::other(err)),
            }
        } else {
            write_human(&mut out, report)
        };
        match write_result {
            Ok(()) => Ok(report.exit_code),
            Err(err) => {
                ignore_broken_pipe(err)?;
                Ok(report.exit_code)
            }
        }
    }
}

fn halt_err(code: i32, error: String) -> Halt {
    Halt {
        code,
        error,
        scans: Vec::new(),
        resolved: Vec::new(),
    }
}

fn file_report(
    repo: &GitRepo,
    settings: &Settings,
    scans: &[FileScan],
    written: usize,
    staged: usize,
) -> Report {
    let merge_in_progress = repo.merge_in_progress().unwrap_or(false);
    file_report_with(repo, settings, merge_in_progress, scans, written, staged)
}

fn file_report_with(
    repo: &GitRepo,
    settings: &Settings,
    merge_in_progress: bool,
    scans: &[FileScan],
    written: usize,
    staged: usize,
) -> Report {
    Report::from_scans(
        &repo.root,
        merge_in_progress,
        false,
        Some(settings.model.clone()),
        Some(settings.code_model.clone()),
        settings.min_confidence,
        scans,
        written,
        staged,
    )
}

fn emit_ci(report: &SyncReport, files: Option<&Report>) -> Result<()> {
    let mut lines = Vec::new();
    if report.exit_code != 0 {
        if let Some(err) = &report.error {
            lines.push(format!(
                "::error title=truce sync::{}",
                ci::escape_data(err)
            ));
        }
    }
    if let Some(files) = files {
        lines.extend(ci::annotations(files));
    }
    ci::write_annotations(&lines)?;
    ci::append_summary(&sync_markdown(report))?;
    if let Some(files) = files {
        ci::append_summary(&ci::summary_markdown(files))?;
    }
    Ok(())
}

fn sync_markdown(report: &SyncReport) -> String {
    let branch = report.branch.as_deref().unwrap_or("—");
    let mut md = String::new();
    md.push_str("## Truce sync\n\n");
    md.push_str(&format!(
        "| Outcome | Strategy | Base | Branch | Exit |\n| --- | --- | --- | --- | ---: |\n| {outcome} | {strategy} | `{base}` | `{branch}` | {exit} |\n\n",
        outcome = report.outcome,
        strategy = report.strategy,
        base = report.base,
        exit = report.exit_code,
    ));
    if report.requested_base != report.base {
        md.push_str(&format!(
            "Requested base `{requested}` was missing; used `{base}`.\n\n",
            requested = report.requested_base,
            base = report.base
        ));
    }
    if let Some(err) = &report.error {
        let flat = err.replace('\n', " ").replace('|', "\\|");
        md.push_str(&format!("**{flat}**\n\n"));
    }
    if report.aborted {
        md.push_str("The merge or rebase was aborted and HEAD was restored.\n\n");
    }
    if !report.resolved.is_empty() {
        md.push_str(
            "| File | Hunk | Resolver | Confidence | Choice |\n| --- | ---: | --- | ---: | --- |\n",
        );
        for hunk in &report.resolved {
            md.push_str(&format!(
                "| `{}` | {} | {} | {:.2} | {} |\n",
                hunk.path.replace('|', "\\|"),
                hunk.hunk,
                hunk.resolver,
                hunk.confidence,
                hunk.choice
            ));
        }
        md.push('\n');
    }
    if let Some(commit) = &report.commit {
        md.push_str(&format!("Commit: `{commit}`  \n"));
    }
    md.push_str(&format!(
        "Pushed: **{}**  \nHead: `{}`\n\n",
        if report.pushed { "yes" } else { "no" },
        report.head
    ));
    md
}

fn write_human(out: &mut dyn Write, report: &SyncReport) -> io::Result<()> {
    writeln!(
        out,
        "truce sync  outcome={}  strategy={}  exit={}",
        report.outcome, report.strategy, report.exit_code
    )?;
    writeln!(
        out,
        "branch={}  base={}  head={}",
        report.branch.as_deref().unwrap_or("detached"),
        report.base,
        report.head
    )?;
    if report.requested_base != report.base {
        writeln!(out, "requested_base={}", report.requested_base)?;
    }
    if let Some(commit) = &report.commit {
        writeln!(out, "commit={commit}")?;
    }
    writeln!(
        out,
        "pushed={}  aborted={}  merge_in_progress={}",
        yes_no(report.pushed),
        yes_no(report.aborted),
        yes_no(report.merge_in_progress)
    )?;
    if let Some(err) = &report.error {
        writeln!(out, "error: {err}")?;
    }
    if !report.resolved.is_empty() {
        writeln!(out, "resolved:")?;
        for hunk in &report.resolved {
            writeln!(
                out,
                "  {}  hunk {}  resolver={}  confidence={:.2}  choice={}",
                hunk.path, hunk.hunk, hunk.resolver, hunk.confidence, hunk.choice
            )?;
        }
    }
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn classify_clean(before: &str, after: &str, base_sha: &str, strategy: Strategy) -> Outcome {
    if after == before {
        Outcome::UpToDate
    } else if strategy == Strategy::Rebase {
        Outcome::CleanRebase
    } else if after == base_sha {
        Outcome::FastForward
    } else {
        Outcome::CleanMerge
    }
}

fn rollback(repo: &GitRepo, strategy: Strategy, original: &str) -> Result<()> {
    match strategy {
        Strategy::Merge => {
            let _ = repo.merge_abort();
        }
        Strategy::Rebase => {
            let _ = repo.rebase_abort();
        }
    }
    if repo.merge_in_progress().unwrap_or(false) {
        let _ = repo.merge_abort();
        let _ = repo.rebase_abort();
    }
    let head_ok = matches!(repo.rev_parse("HEAD"), Ok(head) if head == original);
    if !head_ok {
        repo.reset_hard(original)?;
    }
    let head = repo.rev_parse("HEAD")?;
    if head != original {
        return Err(TruceError::Git {
            status: None,
            message: format!("failed to restore HEAD to {original} (at {head})"),
        });
    }
    if repo.merge_in_progress()? {
        return Err(TruceError::Git {
            status: None,
            message: "failed to leave the merge or rebase after abort".into(),
        });
    }
    Ok(())
}

fn resolve_base(repo: &GitRepo, requested: &str) -> Result<String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Err(TruceError::Config("--base must not be empty".into()));
    }
    let remotes = repo.remote_names()?;
    if let Some((remote, branch)) = split_tracking(requested, &remotes) {
        let fetched = repo.fetch_ref(&remote, &branch)?;
        if fetched.status.success() && repo.rev_exists(requested)? {
            return Ok(requested.to_string());
        }
        let message = output_message(&fetched);
        let missing = !fetched.status.success() && missing_remote_ref(&message);
        let gone = fetched.status.success() && !repo.rev_exists(requested)?;
        if !missing && !gone {
            return Err(TruceError::Git {
                status: Some(fetched.status),
                message: format!("failed to fetch {requested}: {message}"),
            });
        }
        let fallback = remote_default(repo, &remote)?;
        if fallback == requested {
            return Err(TruceError::Git {
                status: None,
                message: format!("base {requested} was not found on {remote}"),
            });
        }
        let fallback_branch = fallback
            .strip_prefix(&format!("{remote}/"))
            .filter(|branch| !branch.is_empty())
            .unwrap_or(&fallback);
        let again = repo.fetch_ref(&remote, fallback_branch)?;
        if again.status.success() && repo.rev_exists(&fallback)? {
            return Ok(fallback);
        }
        return Err(TruceError::Git {
            status: Some(again.status),
            message: format!(
                "base {requested} was not found and the default branch {fallback} could not be fetched: {}",
                output_message(&again)
            ),
        });
    }

    if remotes.iter().any(|remote| remote == "origin") {
        let fetched = repo.fetch_remote("origin")?;
        if !fetched.status.success() && !repo.rev_exists(requested)? {
            return Err(TruceError::Git {
                status: Some(fetched.status),
                message: format!(
                    "failed to fetch origin while resolving {requested}: {}",
                    output_message(&fetched)
                ),
            });
        }
    }
    if repo.rev_exists(requested)? {
        return Ok(requested.to_string());
    }
    Err(TruceError::Git {
        status: None,
        message: format!("base {requested} does not exist"),
    })
}

fn remote_default(repo: &GitRepo, remote: &str) -> Result<String> {
    let sym = format!("refs/remotes/{remote}/HEAD");
    if let Some(name) = repo.symbolic_ref_short(&sym)? {
        if repo.rev_exists(&name)? {
            return Ok(name);
        }
    }
    let set = repo.set_head_auto(remote)?;
    if !set.status.success() {
        return Err(TruceError::Git {
            status: Some(set.status),
            message: format!(
                "could not determine the default branch for {remote}: {}",
                output_message(&set)
            ),
        });
    }
    match repo.symbolic_ref_short(&sym)? {
        Some(name) => Ok(name),
        None => Err(TruceError::Git {
            status: None,
            message: format!("{remote} has no default branch"),
        }),
    }
}

fn split_tracking(requested: &str, remotes: &[String]) -> Option<(String, String)> {
    let mut names: Vec<&str> = remotes.iter().map(String::as_str).collect();
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    for remote in names {
        let prefix = format!("{remote}/");
        if let Some(branch) = requested.strip_prefix(&prefix) {
            if !branch.is_empty() {
                return Some((remote.to_string(), branch.to_string()));
            }
        }
    }
    None
}

fn missing_remote_ref(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("couldn't find remote ref")
        || lower.contains("could not find remote ref")
        || lower.contains("remote ref does not exist")
}

fn editor_env() -> Result<Vec<(String, String)>> {
    let exe = std::env::current_exe()?.to_string_lossy().to_string();
    Ok(vec![
        ("TRUCE_INTERNAL_NOOP_EDITOR".into(), "1".into()),
        ("GIT_EDITOR".into(), exe.clone()),
        ("GIT_SEQUENCE_EDITOR".into(), exe),
    ])
}

struct VerifyError {
    code: i32,
    message: String,
}

fn run_verify(dir: &Path, command: &str) -> std::result::Result<(), VerifyError> {
    let mut cmd = if cfg!(windows) {
        let mut command_builder = Command::new("cmd");
        command_builder.arg("/C").arg(command);
        command_builder
    } else {
        let mut command_builder = Command::new("sh");
        command_builder.arg("-c").arg(command);
        command_builder
    };
    cmd.current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = match cmd.output() {
        Ok(output) => output,
        Err(err) => {
            return Err(VerifyError {
                code: 2,
                message: format!("failed to run verify-cmd: {err}"),
            });
        }
    };
    if output.status.success() {
        return Ok(());
    }
    let status = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".into());
    let mut detail = output_message(&output);
    if detail == "git command failed" {
        detail = "command produced no output".into();
    }
    Err(VerifyError {
        code: 1,
        message: format!("verify-cmd failed (exit {status}): {detail}"),
    })
}

fn collect_blockers(merge_text: &str, porcelain: &[u8], scans: &[FileScan]) -> Vec<String> {
    let mut reasons = Vec::new();
    for line in merge_text.lines() {
        if unsupported_merge_line(line) {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                reasons.push(trimmed.to_string());
            }
        }
    }
    for (xy, path) in parse_porcelain_z(porcelain) {
        if unmerged_code(&xy) && !content_code(&xy) {
            reasons.push(format!(
                "{xy} {path}: delete/modify conflict is not supported"
            ));
        }
    }
    for scan in scans {
        if scan.status == FileStatus::Conflicted && !scan.hunks.is_empty() {
            continue;
        }
        let why = scan
            .error
            .clone()
            .unwrap_or_else(|| scan.status.label().to_string());
        reasons.push(format!("{}: {why}", display_path(&scan.rel_path)));
    }
    reasons
}

fn blocker_code(scans: &[FileScan]) -> i32 {
    if scans
        .iter()
        .any(|scan| scan.status == FileStatus::ParseError)
    {
        2
    } else {
        1
    }
}

fn unsupported_merge_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    const KINDS: &[&str] = &[
        "conflict (modify/delete)",
        "conflict (rename/delete)",
        "conflict (rename/rename)",
        "conflict (directory/file)",
        "conflict (file/directory)",
        "conflict (submodule)",
        "cannot merge binary",
    ];
    KINDS.iter().any(|kind| lower.contains(kind))
}

fn unmerged_code(xy: &str) -> bool {
    matches!(xy, "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU")
}

fn content_code(xy: &str) -> bool {
    matches!(xy, "UU" | "AA")
}

fn parse_porcelain_z(bytes: &[u8]) -> Vec<(String, String)> {
    let parts: Vec<&[u8]> = bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < parts.len() {
        let record = parts[index];
        if record.len() < 3 {
            index += 1;
            continue;
        }
        let xy = String::from_utf8_lossy(&record[..2]).into_owned();
        let path = if record.get(2) == Some(&b' ') {
            String::from_utf8_lossy(&record[3..]).into_owned()
        } else {
            String::from_utf8_lossy(&record[2..]).into_owned()
        };
        let rename = xy.starts_with('R') || xy.starts_with('C');
        if rename && index + 1 < parts.len() {
            index += 1;
            let new_path = String::from_utf8_lossy(parts[index]).into_owned();
            out.push((xy, new_path));
        } else {
            out.push((xy, path));
        }
        index += 1;
    }
    out
}

fn conflicted_paths(scans: &[FileScan]) -> Vec<PathBuf> {
    scans
        .iter()
        .filter(|scan| scan.status == FileStatus::Conflicted && !scan.hunks.is_empty())
        .map(|scan| scan.rel_path.clone())
        .collect()
}

fn same_paths(written: &[PathBuf], expected: &[PathBuf]) -> bool {
    let mut left = written.to_vec();
    let mut right = expected.to_vec();
    left.sort();
    right.sort();
    left == right
}

fn markers_remaining(
    root: &Path,
    paths: &[PathBuf],
) -> std::result::Result<Option<String>, String> {
    for path in paths {
        let text = fs::read_to_string(root.join(path))
            .map_err(|err| format!("failed to read {}: {err}", display_path(path)))?;
        if has_conflict_markers(&text) {
            return Ok(Some(display_path(path)));
        }
    }
    Ok(None)
}

fn resolved_entries(scans: &[FileScan]) -> Vec<ResolvedEntry> {
    let mut out = Vec::new();
    for scan in scans {
        let path = display_path(&scan.rel_path);
        for (index, hunk) in scan.hunks.iter().enumerate() {
            let Some(resolution) = scan
                .outcomes
                .get(index)
                .and_then(|outcome| outcome.resolution())
            else {
                continue;
            };
            let resolver = scan
                .attempts
                .get(index)
                .and_then(|attempt| attempt.resolver)
                .map(|resolver| resolver.to_string())
                .unwrap_or_else(|| "decision".into());
            out.push(ResolvedEntry {
                path: path.clone(),
                hunk: hunk.id,
                resolver,
                confidence: resolution.confidence,
                choice: resolution.choice.to_string(),
            });
        }
    }
    out.sort_by(|left, right| (&left.path, left.hunk).cmp(&(&right.path, right.hunk)));
    out
}

fn summarize_failures(scans: &[FileScan]) -> String {
    let mut parts = Vec::new();
    for scan in scans {
        let path = display_path(&scan.rel_path);
        if scan.status != FileStatus::Conflicted {
            if let Some(err) = &scan.error {
                parts.push(format!("{path}: {err}"));
            }
        }
        for (index, outcome) in scan.outcomes.iter().enumerate() {
            let id = scan
                .hunks
                .get(index)
                .map(|hunk| hunk.id)
                .unwrap_or(index + 1);
            if let Some(reason) = outcome.rejection() {
                parts.push(format!("{path} hunk {id}: {reason}"));
            } else if outcome.resolution().is_none() && scan.status == FileStatus::Conflicted {
                parts.push(format!("{path} hunk {id}: unresolved"));
            }
        }
    }
    if parts.is_empty() {
        "unresolved conflicts remain".into()
    } else {
        truncate_join(&parts)
    }
}

fn truncate_join(parts: &[String]) -> String {
    const MAX: usize = 2_000;
    let text = parts.join("; ");
    if text.len() <= MAX {
        return text;
    }
    let mut end = MAX;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn commit_message(base: &str, hunks: &[ResolvedEntry]) -> String {
    let mut message = format!(
        "truce: sync with {}\n\nResolved conflicts:\n",
        one_line(base)
    );
    for hunk in hunks {
        message.push_str(&format!(
            "- {} hunk {} ({}, confidence {:.2}, choice {})\n",
            one_line(&hunk.path),
            hunk.hunk,
            one_line(&hunk.resolver),
            hunk.confidence,
            one_line(&hunk.choice)
        ));
    }
    message
}

fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_message_lists_each_hunk() {
        let message = commit_message(
            "origin/main",
            &[ResolvedEntry {
                path: "letter.txt".into(),
                hunk: 1,
                resolver: "decision".into(),
                confidence: 0.95,
                choice: "ours".into(),
            }],
        );
        assert!(message.starts_with("truce: sync with origin/main\n"));
        assert!(message.contains("- letter.txt hunk 1 (decision, confidence 0.95, choice ours)"));
    }

    #[test]
    fn tracking_ref_splits_on_known_remote() {
        let remotes = vec!["origin".into(), "upstream".into()];
        assert_eq!(
            split_tracking("origin/main", &remotes),
            Some(("origin".into(), "main".into()))
        );
        assert_eq!(
            split_tracking("origin/feature/foo", &remotes),
            Some(("origin".into(), "feature/foo".into()))
        );
        assert_eq!(split_tracking("main", &remotes), None);
    }

    #[test]
    fn porcelain_marks_delete_conflicts() {
        let parsed = parse_porcelain_z(b"UU letter.txt\0DU gone.txt\0M  other.txt\0");
        assert_eq!(
            parsed,
            vec![
                ("UU".into(), "letter.txt".into()),
                ("DU".into(), "gone.txt".into()),
                ("M ".into(), "other.txt".into()),
            ]
        );
        let reasons = collect_blockers("", b"DU gone.txt\0", &[]);
        assert!(
            reasons.iter().any(|reason| reason.contains("gone.txt")),
            "{reasons:?}"
        );
    }

    #[test]
    fn binary_and_rename_lines_are_unsupported() {
        let text = "\
CONFLICT (content): Merge conflict in letter.txt
warning: Cannot merge binary files: logo.bin (HEAD vs. origin/main)
CONFLICT (modify/delete): gone deleted in HEAD and modified in origin/main.
CONFLICT (rename/rename): old renamed to a in HEAD and to b in origin/main.
";
        let hits: Vec<_> = text
            .lines()
            .filter(|line| unsupported_merge_line(line))
            .collect();
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert!(!unsupported_merge_line(
            "CONFLICT (content): Merge conflict in letter.txt"
        ));
    }

    #[test]
    fn clean_fast_forward_is_not_a_merge_commit() {
        assert_eq!(
            classify_clean("aaa", "bbb", "bbb", Strategy::Merge),
            Outcome::FastForward
        );
        assert_eq!(
            classify_clean("aaa", "ccc", "bbb", Strategy::Merge),
            Outcome::CleanMerge
        );
        assert_eq!(
            classify_clean("aaa", "aaa", "bbb", Strategy::Merge),
            Outcome::UpToDate
        );
    }
}
