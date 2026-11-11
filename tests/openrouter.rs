//! OpenRouter client tests against a local wiremock server. No real key, no
//! outbound network.

mod common;

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::{git, single_conflict_repo, truce};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const KEY: &str = "test-key";

fn completion(resolved: &str, choice: &str, confidence: f64, rationale: &str) -> Value {
    let payload = json!({
        "resolved": resolved,
        "choice": choice,
        "rationale": rationale,
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

fn ok_template(resolved: &str, choice: &str, confidence: f64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(completion(resolved, choice, confidence, "mocked"))
}

struct Sequence {
    calls: AtomicUsize,
    templates: Vec<ResponseTemplate>,
}

impl Respond for Sequence {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        let idx = i.min(self.templates.len() - 1);
        self.templates[idx].clone()
    }
}

fn truce_mock(dir: &std::path::Path, base: &str) -> assert_cmd::Command {
    let mut cmd = truce(dir);
    cmd.env("OPENROUTER_API_KEY", KEY).args([
        "--base-url",
        base,
        "--retries",
        "3",
        "--timeout",
        "5",
    ]);
    cmd
}

#[tokio::test]
async fn apply_writes_resolution_and_strips_markers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_template("gamma", "merged", 0.95))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let before = fs::read_to_string(dir.path().join("letter.txt")).unwrap();
    assert!(before.contains("<<<<<<<"));

    let output = truce_mock(dir.path(), &server.uri())
        .args(["--apply", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let after = fs::read_to_string(dir.path().join("letter.txt")).unwrap();
    assert!(!after.contains("<<<<<<<"), "{after}");
    assert!(after.contains("gamma"), "{after}");
    assert!(after.contains("hello"), "{after}");
    assert!(after.contains("bye"), "{after}");

    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["files_written"], 1);
    assert_eq!(report["summary"]["resolved_hunks"], 1);
    assert_eq!(
        report["files"][0]["hunks"][0]["resolution"]["choice"],
        "merged"
    );
}

#[tokio::test]
async fn dry_run_preview_does_not_write() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_template("gamma", "merged", 0.95))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let before = fs::read_to_string(dir.path().join("letter.txt")).unwrap();
    let output = truce_mock(dir.path(), &server.uri())
        .args(["--json"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = fs::read_to_string(dir.path().join("letter.txt")).unwrap();
    assert_eq!(before, after);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["summary"]["files_written"], 0);
    assert_eq!(report["summary"]["resolved_hunks"], 1);
}

#[tokio::test]
async fn low_confidence_is_rejected_and_file_untouched() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_template("gamma", "merged", 0.2))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let before = fs::read_to_string(dir.path().join("letter.txt")).unwrap();
    let output = truce_mock(dir.path(), &server.uri())
        .args(["--apply", "--min-confidence", "0.7", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let after = fs::read_to_string(dir.path().join("letter.txt")).unwrap();
    assert_eq!(before, after);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let rejection = report["files"][0]["hunks"][0]["rejection"]
        .as_str()
        .unwrap();
    assert!(rejection.contains("confidence"), "{rejection}");
    assert_eq!(report["summary"]["files_written"], 0);
}

#[tokio::test]
async fn leftover_markers_are_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_template("<<<<<<< HEAD\nstill\n", "merged", 0.99))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let output = truce_mock(dir.path(), &server.uri())
        .args(["--apply", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let rejection = report["files"][0]["hunks"][0]["rejection"]
        .as_str()
        .unwrap();
    assert!(rejection.contains("marker"), "{rejection}");
}

#[tokio::test]
async fn retries_on_429_then_succeeds() {
    let server = MockServer::start().await;
    let seq = Sequence {
        calls: AtomicUsize::new(0),
        templates: vec![
            ResponseTemplate::new(429).insert_header("Retry-After", "0"),
            ok_template("gamma", "merged", 0.9),
        ],
    };
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(seq)
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let output = truce_mock(dir.path(), &server.uri())
        .args(["--apply", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = fs::read_to_string(dir.path().join("letter.txt")).unwrap();
    assert!(after.contains("gamma"));
}

#[tokio::test]
async fn http_400_is_not_retried() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    struct Count400(Arc<AtomicUsize>);
    impl Respond for Count400 {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            self.0.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(400).set_body_string("bad request")
        }
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(Count400(calls_clone))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let output = truce_mock(dir.path(), &server.uri())
        .args(["--apply", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "400 must not be retried");
}

#[tokio::test]
async fn stage_adds_written_file() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_template("gamma", "merged", 0.95))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    truce_mock(dir.path(), &server.uri())
        .args(["--apply", "--stage"])
        .assert()
        .code(0);

    let staged = git(dir.path(), &["diff", "--cached", "--name-only"]);
    let names = String::from_utf8_lossy(&staged.stdout);
    assert!(names.contains("letter.txt"), "{names}");
    let unmerged = git(dir.path(), &["ls-files", "-u"]);
    let leftover = String::from_utf8_lossy(&unmerged.stdout);
    assert!(
        leftover.trim().is_empty(),
        "expected no unmerged stages after --stage: {leftover}"
    );
}

#[tokio::test]
async fn apply_without_stage_does_not_git_add() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_template("gamma", "merged", 0.95))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    truce_mock(dir.path(), &server.uri())
        .args(["--apply"])
        .assert()
        .code(0);
    let unmerged = git(dir.path(), &["ls-files", "-u"]);
    let leftover = String::from_utf8_lossy(&unmerged.stdout);
    assert!(
        leftover.contains("letter.txt"),
        "without --stage the path must stay unmerged: {leftover}"
    );
}

#[tokio::test]
async fn auth_failure_exits_2_without_leaking_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let output = truce_mock(dir.path(), &server.uri())
        .args(["--apply"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("authentication") || stderr.contains("OpenRouter"),
        "{stderr}"
    );
    assert!(!stderr.contains(KEY), "{stderr}");
}

#[tokio::test]
async fn prompt_mentions_language() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_template("gamma", "theirs", 0.88))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    truce_mock(dir.path(), &server.uri())
        .args(["--json"])
        .assert()
        .code(0);

    let requests = server.received_requests().await.unwrap();
    assert!(!requests.is_empty());
    let body = std::str::from_utf8(&requests[0].body).unwrap();
    assert!(body.contains("Language: text"), "{body}");
    assert!(body.contains("File: letter.txt"), "{body}");
    assert!(!body.contains(KEY), "API key leaked into request body");
}
