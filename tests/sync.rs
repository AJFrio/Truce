//! `truce sync` against local bare origins. Wiremock only; no real API key.

mod common;

use std::fs;
use std::path::Path;
use std::process::Output;

use common::{git, git_allow_fail, init_repo, truce, two_branch_repo, write};
use serde_json::{json, Value};
use tempfile::{NamedTempFile, TempDir};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "test-key";

struct Pair {
    _bare: TempDir,
    work: TempDir,
}

fn path_arg(path: &Path) -> String {
    path.to_str().expect("utf-8 temp path").to_string()
}

fn init_bare(path: &Path) {
    let init = git_allow_fail(path, &["init", "--bare", "--initial-branch=main"]);
    if !init.status.success() {
        git(path, &["init", "--bare"]);
        git(path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }
}

fn pair() -> Pair {
    let bare = TempDir::new().unwrap();
    init_bare(bare.path());
    let work = init_repo();
    git(
        work.path(),
        &["remote", "add", "origin", &path_arg(bare.path())],
    );
    Pair { _bare: bare, work }
}

fn bare_of(pair: &Pair) -> &Path {
    pair._bare.path()
}

fn sha(dir: &Path, rev: &str) -> String {
    let output = git(dir, &["rev-parse", rev]);
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .replace('\r', "")
        .to_string()
}

fn sha_maybe(dir: &Path, rev: &str) -> Option<String> {
    let output = git_allow_fail(dir, &["rev-parse", "--verify", "--quiet", rev]);
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout)
        .trim()
        .replace('\r', "")
        .to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn porcelain(dir: &Path) -> String {
    let output = git(dir, &["status", "--porcelain"]);
    String::from_utf8_lossy(&output.stdout)
        .replace("\r\n", "\n")
        .to_string()
}

fn commit_message(dir: &Path) -> String {
    let output = git(dir, &["log", "-1", "--pretty=%B"]);
    String::from_utf8_lossy(&output.stdout)
        .replace("\r\n", "\n")
        .to_string()
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn report_of(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!(
            "{err}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_no_key(output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains(KEY), "stdout contained the API key");
    assert!(!stderr.contains(KEY), "stderr contained the API key");
}

fn sync(dir: &Path) -> assert_cmd::Command {
    let mut cmd = truce(dir);
    cmd.arg("sync").args(["--json", "--no-color"]);
    cmd
}

fn decision_body(choice: &str, confidence: f64) -> Value {
    json!({
        "id": "dec-mock",
        "model": "liquid/d1-ultra",
        "provider": "Liquid",
        "answers": {
            "resolution": {
                "type": "choice",
                "choice": choice,
                "confidence": confidence,
                "probabilities": {"ours": confidence, "theirs": 0.0, "both": 0.0, "needs_code": 0.0}
            }
        }
    })
}

fn code_body(resolved: &str, confidence: f64) -> Value {
    let payload = json!({
        "resolved": resolved,
        "choice": "ours",
        "rationale": "mocked",
        "confidence": confidence
    });
    json!({
        "id": "mock",
        "choices": [{
            "message": { "role": "assistant", "content": payload.to_string() }
        }]
    })
}

async fn mount_decision(server: &MockServer, choice: &str, confidence: f64) {
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(decision_body(choice, confidence)))
        .mount(server)
        .await;
}

async fn mount_code(server: &MockServer, resolved: &str, confidence: f64) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(code_body(resolved, confidence)))
        .mount(server)
        .await;
}

fn seed_main(work: &Path, rel: &str, contents: &str) {
    write(work, rel, contents);
    git(work, &["add", "."]);
    git(work, &["commit", "-m", "base"]);
    git(work, &["push", "-u", "origin", "main"]);
}

/// Feature and main both edit `letter.txt`. Work tree is on `feature`.
fn diverged_letter() -> Pair {
    let pair = pair();
    let work = pair.work.path();
    seed_main(work, "letter.txt", "hello\nalpha\nbye\n");
    git(work, &["checkout", "-b", "feature"]);
    write(work, "letter.txt", "hello\nbeta\nbye\n");
    git(work, &["add", "."]);
    git(work, &["commit", "-m", "feature"]);
    git(work, &["checkout", "main"]);
    write(work, "letter.txt", "hello\nomega\nbye\n");
    git(work, &["add", "."]);
    git(work, &["commit", "-m", "main"]);
    git(work, &["push", "origin", "main"]);
    git(work, &["checkout", "feature"]);
    pair
}

fn diverged_clean() -> Pair {
    let pair = pair();
    let work = pair.work.path();
    seed_main(work, "base.txt", "base\n");
    git(work, &["checkout", "-b", "feature"]);
    write(work, "feature.txt", "feature\n");
    git(work, &["add", "."]);
    git(work, &["commit", "-m", "feature"]);
    git(work, &["checkout", "main"]);
    write(work, "main.txt", "main\n");
    git(work, &["add", "."]);
    git(work, &["commit", "-m", "main"]);
    git(work, &["push", "origin", "main"]);
    git(work, &["checkout", "feature"]);
    pair
}

fn behind_main() -> Pair {
    let pair = pair();
    let work = pair.work.path();
    seed_main(work, "base.txt", "base\n");
    git(work, &["checkout", "-b", "feature"]);
    git(work, &["checkout", "main"]);
    write(work, "main.txt", "main\n");
    git(work, &["add", "."]);
    git(work, &["commit", "-m", "main moves"]);
    git(work, &["push", "origin", "main"]);
    git(work, &["checkout", "feature"]);
    pair
}

fn binary_conflict() -> Pair {
    let pair = pair();
    let work = pair.work.path();
    fs::write(work.join("logo.bin"), b"aaa\0bbb").unwrap();
    git(work, &["add", "."]);
    git(work, &["commit", "-m", "base"]);
    git(work, &["push", "-u", "origin", "main"]);
    git(work, &["checkout", "-b", "feature"]);
    fs::write(work.join("logo.bin"), b"ccc\0ddd").unwrap();
    git(work, &["add", "."]);
    git(work, &["commit", "-m", "feature"]);
    git(work, &["checkout", "main"]);
    fs::write(work.join("logo.bin"), b"eee\0fff").unwrap();
    git(work, &["add", "."]);
    git(work, &["commit", "-m", "main"]);
    git(work, &["push", "origin", "main"]);
    git(work, &["checkout", "feature"]);
    pair
}

fn model_args(server: &MockServer) -> Vec<String> {
    vec![
        "--base-url".into(),
        server.uri(),
        "--timeout".into(),
        "5".into(),
        "--retries".into(),
        "1".into(),
    ]
}

#[test]
fn sync_help_lists_flags_and_exit_codes() {
    truce(Path::new("."))
        .args(["sync", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--base"))
        .stdout(predicates::str::contains("--push"))
        .stdout(predicates::str::contains("--verify-cmd"))
        .stdout(predicates::str::contains("--rebase"))
        .stdout(predicates::str::contains("--min-confidence"))
        .stdout(predicates::str::contains("Exit codes:"))
        .stdout(predicates::str::contains("origin/main"));
}

#[test]
fn already_up_to_date_is_a_no_op() {
    let pair = pair();
    let work = pair.work.path();
    seed_main(work, "base.txt", "base\n");
    git(work, &["checkout", "-b", "feature"]);
    write(work, "feature.txt", "feature\n");
    git(work, &["add", "."]);
    git(work, &["commit", "-m", "feature"]);
    let before = sha(work, "HEAD");

    let output = sync(work).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report = report_of(&output);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["command"], "sync");
    assert_eq!(report["outcome"], "up_to_date");
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["aborted"], false);
    assert_eq!(report["pushed"], false);
    assert!(report.get("commit").is_none(), "{report}");
    assert!(report.get("error").is_none(), "{report}");
    assert_eq!(sha(work, "HEAD"), before);
    assert!(porcelain(work).trim().is_empty(), "{}", porcelain(work));
    assert!(sha_maybe(bare_of(&pair), "refs/heads/feature").is_none());
}

#[test]
fn clean_merge_creates_a_normal_merge_commit() {
    let pair = diverged_clean();
    let work = pair.work.path();
    let before = sha(work, "HEAD");

    let output = sync(work).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report = report_of(&output);
    assert_eq!(report["outcome"], "clean_merge", "{report}");
    assert_eq!(report["strategy"], "merge");
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["aborted"], false);
    let after = sha(work, "HEAD");
    assert_ne!(after, before);
    assert_eq!(report["commit"], after);
    assert_eq!(report["head"], after);
    let parents = git(work, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    let parent_line = String::from_utf8_lossy(&parents.stdout);
    assert_eq!(
        parent_line.split_whitespace().count(),
        3,
        "expected a merge commit: {parent_line}"
    );
    let subject = commit_message(work);
    assert!(
        !subject.contains("truce: sync"),
        "clean merge should keep git's message: {subject}"
    );
    assert!(work.join("main.txt").is_file());
    assert!(work.join("feature.txt").is_file());
    assert!(porcelain(work).trim().is_empty(), "{}", porcelain(work));
    assert!(sha_maybe(bare_of(&pair), "refs/heads/feature").is_none());
}

#[test]
fn fast_forward_moves_head_without_a_new_commit() {
    let pair = behind_main();
    let work = pair.work.path();
    let base = sha(work, "origin/main");
    let before = sha(work, "HEAD");

    let output = sync(work).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report = report_of(&output);
    assert_eq!(report["outcome"], "fast_forward", "{report}");
    assert!(report.get("commit").is_none(), "{report}");
    assert_eq!(report["head"], base);
    assert_ne!(sha(work, "HEAD"), before);
    assert_eq!(sha(work, "HEAD"), base);
    assert!(work.join("main.txt").is_file());
}

#[test]
fn missing_base_falls_back_to_the_remote_default_branch() {
    let pair = behind_main();
    let work = pair.work.path();
    let base = sha(work, "origin/main");

    let output = sync(work)
        .args(["--base", "origin/does-not-exist"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report = report_of(&output);
    assert_eq!(report["outcome"], "fast_forward", "{report}");
    assert_eq!(report["requested_base"], "origin/does-not-exist");
    assert_eq!(report["base"], "origin/main");
    assert_eq!(sha(work, "HEAD"), base);
}

#[test]
fn push_is_opt_in_and_updates_the_local_origin() {
    let pair = behind_main();
    let work = pair.work.path();

    let output = sync(work).arg("--push").output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report = report_of(&output);
    assert_eq!(report["pushed"], true, "{report}");
    assert_eq!(report["outcome"], "fast_forward");
    let local = sha(work, "HEAD");
    assert_eq!(sha(bare_of(&pair), "refs/heads/feature"), local);
}

#[test]
fn dirty_tree_is_refused() {
    let pair = pair();
    let work = pair.work.path();
    seed_main(work, "base.txt", "base\n");
    let before = sha(work, "HEAD");
    write(work, "base.txt", "base\ndirty\n");

    let output = sync(work).output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let report = report_of(&output);
    assert_eq!(report["outcome"], "refused");
    assert_eq!(report["exit_code"], 2);
    assert_eq!(report["aborted"], false);
    let error = report["error"].as_str().unwrap_or("");
    assert!(error.contains("dirty"), "{error}");
    assert_eq!(sha(work, "HEAD"), before);
    assert!(
        porcelain(work).contains("base.txt"),
        "dirty file should remain:\n{}",
        porcelain(work)
    );
    assert!(sha_maybe(work, "MERGE_HEAD").is_none());
}

#[test]
fn detached_head_is_refused() {
    let pair = pair();
    let work = pair.work.path();
    seed_main(work, "base.txt", "base\n");
    let before = sha(work, "HEAD");
    git(work, &["checkout", "--detach"]);

    let output = sync(work).output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let report = report_of(&output);
    assert_eq!(report["outcome"], "refused");
    assert!(
        report["error"].as_str().unwrap_or("").contains("detached"),
        "{report}"
    );
    assert_eq!(sha(work, "HEAD"), before);
}

#[test]
fn merge_already_in_progress_is_left_alone() {
    let dir = two_branch_repo();
    let work = dir.path();
    let merge = git_allow_fail(work, &["merge", "main", "--no-edit"]);
    assert!(!merge.status.success(), "expected a conflict");
    let before = sha(work, "HEAD");
    assert!(sha_maybe(work, "MERGE_HEAD").is_some());

    let output = sync(work).output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let report = report_of(&output);
    assert_eq!(report["outcome"], "refused");
    assert_eq!(report["merge_in_progress"], true);
    assert!(
        report["error"]
            .as_str()
            .unwrap_or("")
            .contains("already in progress"),
        "{report}"
    );
    assert_eq!(sha(work, "HEAD"), before);
    assert!(sha_maybe(work, "MERGE_HEAD").is_some());
    let body = fs::read_to_string(work.join("letter.txt")).unwrap();
    assert!(body.contains("<<<<<<<"), "{body}");
}

#[test]
fn binary_conflict_aborts_and_restores_head() {
    let pair = binary_conflict();
    let work = pair.work.path();
    let before = sha(work, "HEAD");
    let original = fs::read(work.join("logo.bin")).unwrap();

    let output = sync(work).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = report_of(&output);
    assert_eq!(report["outcome"], "aborted", "{report}");
    assert_eq!(report["exit_code"], 1);
    assert_eq!(report["aborted"], true);
    assert!(report.get("commit").is_none(), "{report}");
    let error = report["error"].as_str().unwrap_or("").to_ascii_lowercase();
    assert!(error.contains("binary") || error.contains("nul"), "{error}");
    assert_eq!(sha(work, "HEAD"), before);
    assert!(sha_maybe(work, "MERGE_HEAD").is_none());
    assert_eq!(fs::read(work.join("logo.bin")).unwrap(), original);
    assert!(porcelain(work).trim().is_empty(), "{}", porcelain(work));
    assert_eq!(report["summary"]["files_written"], 0);
}

#[tokio::test]
async fn resolved_conflict_is_committed_with_resolver_and_confidence() {
    let server = MockServer::start().await;
    mount_decision(&server, "ours", 0.95).await;

    let pair = diverged_letter();
    let work = pair.work.path();
    let before = sha(work, "HEAD");
    let summary = NamedTempFile::new().unwrap();

    let mut args = model_args(&server);
    args.push("--ci".into());
    let output = sync(work)
        .env("OPENROUTER_API_KEY", KEY)
        .env("GITHUB_STEP_SUMMARY", summary.path())
        .args(&args)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_no_key(&output);
    let report = report_of(&output);
    assert_eq!(report["outcome"], "resolved", "{report}");
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["pushed"], false);
    assert_eq!(report["aborted"], false);
    assert_eq!(report["summary"]["resolved_hunks"], 1);
    assert_eq!(report["summary"]["files_written"], 1);
    let hunk = &report["resolved"][0];
    assert_eq!(hunk["path"], "letter.txt");
    assert_eq!(hunk["hunk"], 1);
    assert_eq!(hunk["resolver"], "decision");
    assert_eq!(hunk["choice"], "ours");
    assert!((hunk["confidence"].as_f64().unwrap() - 0.95).abs() < 1e-9);

    let body = normalize(&fs::read_to_string(work.join("letter.txt")).unwrap());
    assert_eq!(body, "hello\nbeta\nbye\n");
    assert!(!body.contains("<<<<<<<"));

    let after = sha(work, "HEAD");
    assert_ne!(after, before);
    assert_eq!(report["commit"], after);
    let message = commit_message(work);
    assert!(
        message.contains("truce: sync with origin/main"),
        "{message}"
    );
    assert!(
        message.contains("letter.txt hunk 1 (decision, confidence 0.95, choice ours)"),
        "{message}"
    );
    let parents = git(work, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert_eq!(
        String::from_utf8_lossy(&parents.stdout)
            .split_whitespace()
            .count(),
        3
    );
    assert!(porcelain(work).trim().is_empty(), "{}", porcelain(work));
    assert!(sha_maybe(bare_of(&pair), "refs/heads/feature").is_none());
    let shown = git(work, &["show", "HEAD:letter.txt"]);
    let shown = normalize(&String::from_utf8_lossy(&shown.stdout));
    assert!(!shown.contains("<<<<<<<"), "{shown}");

    let md = fs::read_to_string(summary.path()).unwrap();
    assert!(md.contains("## Truce sync"), "{md}");
    assert!(md.contains("letter.txt"), "{md}");
    assert!(md.contains("decision"), "{md}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("::error"),
        "successful sync should not emit an error annotation:\n{stderr}"
    );
}

#[tokio::test]
async fn low_confidence_aborts_and_restores_the_tree() {
    let server = MockServer::start().await;
    mount_decision(&server, "ours", 0.2).await;
    mount_code(&server, "beta", 0.1).await;

    let pair = diverged_letter();
    let work = pair.work.path();
    let before = sha(work, "HEAD");
    let original = fs::read(work.join("letter.txt")).unwrap();

    let mut args = model_args(&server);
    args.extend(["--min-confidence".into(), "0.8".into(), "--push".into()]);
    let output = sync(work)
        .env("OPENROUTER_API_KEY", KEY)
        .args(&args)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_no_key(&output);
    let report = report_of(&output);
    assert_eq!(report["outcome"], "aborted", "{report}");
    assert_eq!(report["exit_code"], 1);
    assert_eq!(report["aborted"], true);
    assert_eq!(report["pushed"], false);
    assert!(report.get("commit").is_none(), "{report}");
    let error = report["error"].as_str().unwrap_or("");
    assert!(
        error.contains("confidence") || error.contains("unresolved"),
        "{error}"
    );
    assert_eq!(sha(work, "HEAD"), before);
    assert!(sha_maybe(work, "MERGE_HEAD").is_none());
    assert_eq!(fs::read(work.join("letter.txt")).unwrap(), original);
    assert!(!String::from_utf8_lossy(&original).contains("<<<<<<<"));
    assert!(porcelain(work).trim().is_empty(), "{}", porcelain(work));
    assert!(sha_maybe(bare_of(&pair), "refs/heads/feature").is_none());
    assert!(!commit_message(work).contains("truce: sync"));
    assert_eq!(report["summary"]["files_written"], 0);
}

#[tokio::test]
async fn verify_cmd_failure_aborts_before_commit() {
    let server = MockServer::start().await;
    mount_decision(&server, "ours", 0.95).await;

    let pair = diverged_letter();
    let work = pair.work.path();
    let before = sha(work, "HEAD");
    let original = fs::read(work.join("letter.txt")).unwrap();

    let mut args = model_args(&server);
    args.extend(["--verify-cmd".into(), "truce-verify-should-fail".into()]);
    let output = sync(work)
        .env("OPENROUTER_API_KEY", KEY)
        .args(&args)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_no_key(&output);
    let report = report_of(&output);
    assert_eq!(report["outcome"], "aborted", "{report}");
    assert_eq!(report["exit_code"], 1);
    assert_eq!(report["aborted"], true);
    let error = report["error"].as_str().unwrap_or("");
    assert!(error.contains("verify-cmd"), "{error}");
    assert!(report.get("commit").is_none(), "{report}");
    assert_eq!(sha(work, "HEAD"), before);
    assert!(sha_maybe(work, "MERGE_HEAD").is_none());
    assert_eq!(fs::read(work.join("letter.txt")).unwrap(), original);
    assert!(porcelain(work).trim().is_empty(), "{}", porcelain(work));
    assert!(!commit_message(work).contains("truce: sync"));
}

#[tokio::test]
async fn rebase_resolves_a_conflict_and_finishes() {
    let server = MockServer::start().await;
    mount_decision(&server, "ours", 0.95).await;

    let pair = diverged_letter();
    let work = pair.work.path();
    let before = sha(work, "HEAD");

    let mut args = model_args(&server);
    args.push("--rebase".into());
    let output = sync(work)
        .env("OPENROUTER_API_KEY", KEY)
        .args(&args)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_no_key(&output);
    let report = report_of(&output);
    assert_eq!(report["strategy"], "rebase");
    assert_eq!(report["outcome"], "resolved", "{report}");
    assert_eq!(report["resolved"][0]["resolver"], "decision");
    let body = normalize(&fs::read_to_string(work.join("letter.txt")).unwrap());
    assert!(!body.contains("<<<<<<<"), "{body}");
    assert!(body.contains("omega") || body.contains("beta"), "{body}");
    assert_ne!(sha(work, "HEAD"), before);
    assert!(sha_maybe(work, "MERGE_HEAD").is_none());
    assert!(porcelain(work).trim().is_empty(), "{}", porcelain(work));
    let _ = pair;
}

#[tokio::test]
async fn rebase_low_confidence_restores_head() {
    let server = MockServer::start().await;
    mount_decision(&server, "ours", 0.2).await;
    mount_code(&server, "nope", 0.1).await;

    let pair = diverged_letter();
    let work = pair.work.path();
    let before = sha(work, "HEAD");
    let original = fs::read(work.join("letter.txt")).unwrap();

    let mut args = model_args(&server);
    args.extend(["--rebase".into(), "--min-confidence".into(), "0.8".into()]);
    let output = sync(work)
        .env("OPENROUTER_API_KEY", KEY)
        .args(&args)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = report_of(&output);
    assert_eq!(report["outcome"], "aborted", "{report}");
    assert_eq!(report["aborted"], true);
    assert_eq!(sha(work, "HEAD"), before);
    assert_eq!(fs::read(work.join("letter.txt")).unwrap(), original);
    assert!(porcelain(work).trim().is_empty(), "{}", porcelain(work));
    assert!(sha_maybe(work, "MERGE_HEAD").is_none());
}
