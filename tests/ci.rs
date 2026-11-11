//! CI-mode and composite-action script tests. Mock HTTP only.

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use common::{git, git_allow_fail, single_conflict_repo, truce, two_branch_repo};
use serde_json::{json, Value};
use tempfile::NamedTempFile;
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
    ResponseTemplate::new(200).set_body_json(completion(resolved, "merged", 0.95))
}

fn summary_file() -> NamedTempFile {
    NamedTempFile::new().unwrap()
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
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/truce-action.sh");
    assert!(script.is_file(), "missing {}", script.display());

    let output = Command::new("bash")
        .arg(&script)
        .current_dir(dir.path())
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Truce Test")
        .env("GIT_AUTHOR_EMAIL", "truce@example.com")
        .env("GIT_COMMITTER_NAME", "Truce Test")
        .env("GIT_COMMITTER_EMAIL", "truce@example.com")
        .env("OPENROUTER_API_KEY", KEY)
        .env("TRUCE_BIN", &bin)
        .env("TRUCE_MODE", "commit")
        .env("TRUCE_SKIP_FETCH", "1")
        .env("TRUCE_SKIP_PUSH", "1")
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
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/truce-action.sh");
    let output = Command::new("bash")
        .arg(&script)
        .current_dir(dir.path())
        .env("OPENROUTER_API_KEY", "sk or key")
        .env("TRUCE_SKIP_FETCH", "1")
        .env("TRUCE_SKIP_MERGE", "1")
        .env("TRUCE_BIN", "true")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("whitespace"), "{stderr}");
    assert!(!stderr.contains("sk or key"), "{stderr}");
}
