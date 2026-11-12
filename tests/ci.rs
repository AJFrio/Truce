//! CI-mode and composite-action script tests. Mock HTTP only.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{
    git, git_allow_fail, single_conflict_repo, truce, two_branch_clean_repo, two_branch_repo,
};
use serde_json::{json, Value};
use tempfile::{NamedTempFile, TempDir};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "test-key";

fn completion(resolved: &str, choice: &str, confidence: f64) -> Value {
    let payload = json!({
        "resolved": resolved,
        "choice": choice,
        "rationale": "mocked",
        "confidence": confidence
    });
    json!({
        "id": "mock",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": payload.to_string() }
        }]
    })
}

fn ok_template(resolved: &str) -> ResponseTemplate {
    json_template(resolved, 0.95)
}

fn json_template(resolved: &str, confidence: f64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(completion(resolved, "merged", confidence))
}

fn summary_file() -> NamedTempFile {
    NamedTempFile::new().unwrap()
}

#[cfg(unix)]
fn action_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/truce-action.sh")
}

#[cfg(unix)]
fn script_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg(action_script())
        .current_dir(dir)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("TRUCE_SKIP_FETCH", "1")
        .env("TRUCE_SKIP_PUSH", "1")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("TRUCE_MODEL")
        .env_remove("TRUCE_CODE_MODEL")
        .env_remove("TRUCE_MIN_CONFIDENCE");
    cmd
}

#[cfg(unix)]
fn isolate_git_identity(cmd: &mut Command, home: &Path) {
    let global = home.join(".gitconfig");
    fs::write(&global, "").unwrap();
    cmd.env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env_remove("EMAIL");
}

#[cfg(unix)]
fn unset_local_identity(root: &Path) {
    let _ = git_allow_fail(root, &["config", "--unset", "user.name"]);
    let _ = git_allow_fail(root, &["config", "--unset", "user.email"]);
}

#[tokio::test]
async fn ci_flag_writes_annotations_and_step_summary() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_template("gamma"))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let summary = summary_file();

    let output = truce(dir.path())
        .env("OPENROUTER_API_KEY", KEY)
        .env("GITHUB_STEP_SUMMARY", summary.path())
        .args([
            "--ci",
            "--json",
            "--base-url",
            &server.uri(),
            "--retries",
            "1",
            "--timeout",
            "5",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("::error") && !stdout.contains("::warning"),
        "annotations must stay off JSON stdout:\n{stdout}"
    );
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["summary"]["resolved_hunks"], 1);

    let md = fs::read_to_string(summary.path()).unwrap();
    assert!(md.contains("## Truce"), "{md}");
    assert!(md.contains("`letter.txt`"), "{md}");
    assert!(md.contains("RESOLVED") || md.contains("CONFLICTED"), "{md}");
    // Successful resolution: no error annotation for that hunk.
    assert!(
        !stderr.contains("::error file=letter.txt"),
        "unexpected error annotation:\n{stderr}"
    );
}

#[tokio::test]
async fn github_actions_env_auto_detects_ci_mode() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_template("gamma"))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let summary = summary_file();

    let output = truce(dir.path())
        .env("OPENROUTER_API_KEY", KEY)
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", summary.path())
        .args([
            "--json",
            "--base-url",
            &server.uri(),
            "--retries",
            "1",
            "--timeout",
            "5",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let md = fs::read_to_string(summary.path()).unwrap();
    assert!(md.contains("## Truce"), "{md}");
    assert!(md.contains("| File | Status |"), "{md}");
}

#[test]
fn ci_mode_without_key_annotates_unresolved_hunk() {
    let dir = single_conflict_repo();
    let summary = summary_file();

    let output = truce(dir.path())
        .env("GITHUB_STEP_SUMMARY", summary.path())
        .args(["--ci", "--json"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("::error file=letter.txt,line="),
        "missing file+line annotation:\n{stderr}"
    );
    assert!(
        stderr.contains("Unresolved merge conflict") || stderr.contains("title=Unresolved"),
        "{stderr}"
    );
    let md = fs::read_to_string(summary.path()).unwrap();
    assert!(md.contains("## Truce"), "{md}");
    assert!(md.contains("`letter.txt`"), "{md}");
    assert!(md.contains("Exit: `1`"), "{md}");
}

#[cfg(unix)]
#[tokio::test]
async fn action_script_merges_and_runs_truce_in_ci_mode() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_template("gamma"))
        .mount(&server)
        .await;

    let dir = two_branch_repo();
    let summary = summary_file();
    let bin = assert_cmd::cargo::cargo_bin("truce");

    let output = script_cmd(dir.path())
        .env("GIT_AUTHOR_NAME", "Truce Test")
        .env("GIT_AUTHOR_EMAIL", "truce@example.com")
        .env("GIT_COMMITTER_NAME", "Truce Test")
        .env("GIT_COMMITTER_EMAIL", "truce@example.com")
        .env("OPENROUTER_API_KEY", KEY)
        .env("TRUCE_BIN", &bin)
        .env("TRUCE_MODE", "commit")
        .env("TRUCE_MERGE_REF", "main")
        .env("TRUCE_BASE_URL", server.uri())
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", summary.path())
        .env("GITHUB_HEAD_REF", "feature")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "script failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let body = fs::read_to_string(dir.path().join("letter.txt")).unwrap();
    assert_eq!(body, "hello\ngamma\nbye\n");
    assert!(!body.contains("<<<<<<<"));

    let md = fs::read_to_string(summary.path()).unwrap();
    assert!(md.contains("## Truce"), "{md}");

    let log = git(dir.path(), &["log", "-1", "--pretty=%s"]);
    let subject = String::from_utf8_lossy(&log.stdout);
    assert!(
        subject.contains("truce: resolve merge conflicts"),
        "{subject}"
    );

    let status = git_allow_fail(dir.path(), &["status", "--porcelain"]);
    let porcelain = String::from_utf8_lossy(&status.stdout);
    assert!(porcelain.trim().is_empty(), "dirty tree:\n{porcelain}");
}

#[cfg(unix)]
#[test]
fn action_script_rejects_whitespace_in_api_key() {
    let dir = two_branch_repo();
    let output = script_cmd(dir.path())
        .env("OPENROUTER_API_KEY", "sk or key")
        .env("TRUCE_SKIP_MERGE", "1")
        .env("TRUCE_BIN", "true")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("whitespace"), "{stderr}");
    assert!(!stderr.contains("sk or key"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn action_script_clean_merge_exits_0_without_identity_env_or_push() {
    let dir = two_branch_clean_repo();
    unset_local_identity(dir.path());
    let home = TempDir::new().unwrap();
    let summary = summary_file();
    let before = git(dir.path(), &["rev-parse", "HEAD"]);
    let before_sha = String::from_utf8_lossy(&before.stdout).trim().to_string();

    let mut cmd = script_cmd(dir.path());
    isolate_git_identity(&mut cmd, home.path());
    let output = cmd
        .env("TRUCE_MODE", "commit")
        .env("TRUCE_MERGE_REF", "main")
        .env("TRUCE_BIN", "false")
        .env("GITHUB_STEP_SUMMARY", summary.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Merged cleanly; nothing to resolve"),
        "{stdout}"
    );
    let md = fs::read_to_string(summary.path()).unwrap();
    assert!(md.contains("Merged cleanly; nothing to resolve"), "{md}");

    let after = git(dir.path(), &["rev-parse", "HEAD"]);
    let after_sha = String::from_utf8_lossy(&after.stdout).trim().to_string();
    assert_ne!(before_sha, after_sha, "clean merge should create a commit");
    let subject = git(dir.path(), &["log", "-1", "--pretty=%s"]);
    let subject = String::from_utf8_lossy(&subject.stdout);
    assert!(
        !subject.contains("truce: resolve merge conflicts"),
        "{subject}"
    );
    assert!(dir.path().join("main.txt").is_file());
    assert!(dir.path().join("feature.txt").is_file());
}

#[cfg(unix)]
#[test]
fn action_script_merge_error_exits_2_and_does_not_run_truce() {
    let dir = two_branch_clean_repo();
    let output = script_cmd(dir.path())
        .env("TRUCE_MODE", "commit")
        .env("TRUCE_MERGE_REF", "definitely-not-a-ref")
        .env("TRUCE_BIN", "false")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(err.contains("git merge failed without conflicts"), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn action_script_unresolved_skips_commit_and_push() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(json_template("gamma", 0.1))
        .mount(&server)
        .await;

    let dir = two_branch_repo();
    let summary = summary_file();
    let bin = assert_cmd::cargo::cargo_bin("truce");
    let before = git(dir.path(), &["log", "-1", "--pretty=%H"]);
    let before_sha = String::from_utf8_lossy(&before.stdout).trim().to_string();

    let output = script_cmd(dir.path())
        .env("GIT_AUTHOR_NAME", "Truce Test")
        .env("GIT_AUTHOR_EMAIL", "truce@example.com")
        .env("OPENROUTER_API_KEY", KEY)
        .env("TRUCE_BIN", &bin)
        .env("TRUCE_MODE", "commit")
        .env("TRUCE_MERGE_REF", "main")
        .env("TRUCE_BASE_URL", server.uri())
        .env("GITHUB_STEP_SUMMARY", summary.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("Skipping commit and push"), "{combined}");
    let md = fs::read_to_string(summary.path()).unwrap();
    assert!(md.contains("Skipping commit and push"), "{md}");
    assert!(md.contains("unresolved conflicts remain"), "{md}");

    let after = git(dir.path(), &["log", "-1", "--pretty=%H"]);
    let after_sha = String::from_utf8_lossy(&after.stdout).trim().to_string();
    assert_eq!(before_sha, after_sha, "must not commit a broken merge");
    let merge_head = git_allow_fail(dir.path(), &["rev-parse", "-q", "--verify", "MERGE_HEAD"]);
    assert!(
        merge_head.status.success(),
        "should still be in a conflicted merge"
    );
    let body = fs::read_to_string(dir.path().join("letter.txt")).unwrap();
    assert!(body.contains("<<<<<<<"), "{body}");
}

#[cfg(unix)]
#[test]
fn action_script_passes_code_model_and_min_confidence() {
    let dir = two_branch_repo();
    let bin = assert_cmd::cargo::cargo_bin("truce");
    let output = script_cmd(dir.path())
        .env("TRUCE_BIN", &bin)
        .env("TRUCE_MODE", "report")
        .env("TRUCE_SKIP_MERGE", "1")
        .env("TRUCE_MODEL", "custom/decision")
        .env("TRUCE_CODE_MODEL", "custom/code")
        .env("TRUCE_MIN_CONFIDENCE", "0.50")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("decision_model=custom/decision"),
        "{stdout}"
    );
    assert!(stdout.contains("code_model=custom/code"), "{stdout}");
    assert!(stdout.contains("min_confidence=0.50"), "{stdout}");
}
