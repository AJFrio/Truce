//! System One decision path and code-model escalation. Wiremock only.

mod common;

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::{single_conflict_repo, truce};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEY: &str = "test-key";
const DECISION_MODEL: &str = "liquid/d1-ultra";
const CODE_MODEL: &str = "~deepseek/deepseek-flash-latest";

fn decision_response(choice: &str, confidence: f64) -> Value {
    json!({
        "id": "dec-mock",
        "model": DECISION_MODEL,
        "provider": "Liquid",
        "answers": {
            "resolution": {
                "type": "choice",
                "choice": choice,
                "confidence": confidence,
                "probabilities": {
                    "ours": 0.5,
                    "theirs": 0.25,
                    "both": 0.25,
                    "needs_code": 0.0
                }
            }
        },
        "usage": { "cost": 0.0, "input_tokens": 20, "output_tokens": 4 }
    })
}

fn decision_template(choice: &str, confidence: f64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(decision_response(choice, confidence))
}

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
            "message": { "role": "assistant", "content": payload.to_string() }
        }]
    })
}

fn code_template(resolved: &str, choice: &str, confidence: f64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(completion(resolved, choice, confidence))
}

fn truce_at(dir: &std::path::Path, base: &str) -> assert_cmd::Command {
    let mut cmd = truce(dir);
    cmd.env("OPENROUTER_API_KEY", KEY).args([
        "--base-url",
        base,
        "--model",
        DECISION_MODEL,
        "--code-model",
        CODE_MODEL,
        "--timeout",
        "5",
    ]);
    cmd
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn report_of(output: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!(
            "{err}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_no_key(output: &std::process::Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains(KEY), "stdout contained the API key");
    assert!(!stderr.contains(KEY), "stderr contained the API key");
}

fn paths_of(requests: &[Request]) -> Vec<String> {
    requests
        .iter()
        .map(|request| request.url.path().to_string())
        .collect()
}

async fn mount_decision(server: &MockServer, choice: &str, confidence: f64) {
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(decision_template(choice, confidence))
        .mount(server)
        .await;
}

#[tokio::test]
async fn decision_path_picks_ours() {
    let server = MockServer::start().await;
    mount_decision(&server, "ours", 0.875).await;

    let dir = single_conflict_repo();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--apply", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    assert_no_key(&output);
    let body = normalize(&fs::read_to_string(dir.path().join("letter.txt")).unwrap());
    assert_eq!(body, "hello\nomega\nbye\n");

    let report = report_of(&output);
    let hunk = &report["files"][0]["hunks"][0];
    assert_eq!(hunk["resolver"], "decision");
    assert!(hunk.get("escalation_reason").is_none(), "{hunk}");
    assert_eq!(hunk["decision"]["model"], DECISION_MODEL);
    assert_eq!(hunk["decision"]["choice"], "ours");
    assert!((hunk["decision"]["confidence"].as_f64().unwrap() - 0.875).abs() < 1e-9);
    assert_eq!(hunk["decision"]["probabilities"]["ours"], 0.5);
    assert_eq!(hunk["decision"]["probabilities"]["theirs"], 0.25);
    assert_eq!(hunk["decision"]["probabilities"]["both"], 0.25);
    assert_eq!(hunk["decision"]["probabilities"]["needs_code"], 0.0);
    assert_eq!(hunk["resolution"]["choice"], "ours");
    assert_eq!(hunk["resolution"]["text"], "omega");
    assert_eq!(
        hunk["resolution"]["rationale"],
        "decision model selected ours"
    );
    assert_eq!(report["model"], DECISION_MODEL);
    assert_eq!(report["code_model"], CODE_MODEL);
    assert_eq!(report["summary"]["resolved_hunks"], 1);
    assert_eq!(report["summary"]["files_written"], 1);

    let requests = server.received_requests().await.unwrap();
    let paths = paths_of(&requests);
    assert_eq!(paths, vec!["/systemone".to_string()], "{paths:?}");
    assert!(!std::str::from_utf8(&requests[0].body)
        .unwrap()
        .contains(KEY));
}

#[tokio::test]
async fn decision_path_picks_theirs() {
    let server = MockServer::start().await;
    mount_decision(&server, "theirs", 1.0).await;
    let dir = single_conflict_repo();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--apply", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let body = normalize(&fs::read_to_string(dir.path().join("letter.txt")).unwrap());
    assert_eq!(body, "hello\nbeta\nbye\n");
    let hunk = &report_of(&output)["files"][0]["hunks"][0];
    assert_eq!(hunk["resolver"], "decision");
    assert_eq!(hunk["decision"]["choice"], "theirs");
    assert_eq!(hunk["resolution"]["choice"], "theirs");
    assert_eq!(hunk["resolution"]["text"], "beta");
    let paths = paths_of(&server.received_requests().await.unwrap());
    assert!(paths.iter().all(|path| path == "/systemone"), "{paths:?}");
}

#[tokio::test]
async fn decision_path_picks_both_and_drops_identical_seam() {
    let server = MockServer::start().await;
    mount_decision(&server, "both", 0.875).await;
    let dir = single_conflict_repo();
    fs::write(
        dir.path().join("letter.txt"),
        "keep\n<<<<<<< HEAD\nalpha\nshared\n=======\nshared\nbeta\n>>>>>>> feature\nend\n",
    )
    .unwrap();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--apply", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let body = normalize(&fs::read_to_string(dir.path().join("letter.txt")).unwrap());
    assert_eq!(body, "keep\nalpha\nshared\nbeta\nend\n");
    let hunk = &report_of(&output)["files"][0]["hunks"][0];
    assert_eq!(hunk["resolver"], "decision");
    assert_eq!(hunk["decision"]["choice"], "both");
    assert_eq!(hunk["resolution"]["choice"], "merged");
    assert_eq!(hunk["resolution"]["text"], "alpha\nshared\nbeta");
    assert!(
        hunk["resolution"]["rationale"]
            .as_str()
            .unwrap()
            .contains("seam"),
        "{hunk}"
    );
    let paths = paths_of(&server.received_requests().await.unwrap());
    assert!(paths.iter().all(|path| path == "/systemone"), "{paths:?}");
}

#[tokio::test]
async fn low_confidence_escalates_to_the_code_model() {
    let server = MockServer::start().await;
    mount_decision(&server, "ours", 0.25).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(code_template("gamma", "merged", 0.875))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--apply", "--json", "--min-confidence", "0.7"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let body = normalize(&fs::read_to_string(dir.path().join("letter.txt")).unwrap());
    assert_eq!(body, "hello\ngamma\nbye\n");
    let hunk = &report_of(&output)["files"][0]["hunks"][0];
    assert_eq!(hunk["resolver"], "code");
    assert_eq!(hunk["escalation_reason"], "low_confidence");
    assert_eq!(hunk["decision"]["choice"], "ours");
    assert!((hunk["decision"]["confidence"].as_f64().unwrap() - 0.25).abs() < 1e-9);
    assert_eq!(hunk["resolution"]["text"], "gamma");
    assert_eq!(hunk["resolution"]["choice"], "merged");

    let requests = server.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path().ends_with("/chat/completions"))
        .expect("code request");
    let chat_body: Value = serde_json::from_slice(&chat.body).unwrap();
    assert_eq!(chat_body["model"], CODE_MODEL);
    assert!(chat_body.get("response_format").is_some());
    assert!(chat_body.get("messages").is_some());
    assert!(chat_body.get("questions").is_none());
}

#[tokio::test]
async fn needs_code_escalates_to_the_code_model() {
    let server = MockServer::start().await;
    mount_decision(&server, "needs_code", 0.875).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(code_template("gamma", "merged", 0.875))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--apply", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let hunk = &report_of(&output)["files"][0]["hunks"][0];
    assert_eq!(hunk["resolver"], "code");
    assert_eq!(hunk["escalation_reason"], "needs_code");
    assert_eq!(hunk["decision"]["choice"], "needs_code");
    assert_eq!(hunk["resolution"]["text"], "gamma");
    let body = normalize(&fs::read_to_string(dir.path().join("letter.txt")).unwrap());
    assert!(body.contains("gamma"), "{body}");
    assert!(!body.contains("<<<<<<<"), "{body}");
}

#[tokio::test]
async fn unknown_choice_escalates() {
    let server = MockServer::start().await;
    mount_decision(&server, "rewrite", 0.99).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(code_template("gamma", "ours", 0.875))
        .mount(&server)
        .await;
    let dir = single_conflict_repo();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let hunk = &report_of(&output)["files"][0]["hunks"][0];
    assert_eq!(hunk["escalation_reason"], "unknown_choice");
    assert_eq!(hunk["resolver"], "code");
    assert_eq!(hunk["decision"]["choice"], "rewrite");
    assert_eq!(hunk["resolution"]["text"], "gamma");
}

#[tokio::test]
async fn decision_4xx_is_not_retried_and_escalates() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(move |_request: &Request| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(400).set_body_string(format!("bad {KEY}"))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(code_template("gamma", "theirs", 0.875))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--apply", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "400 must not be retried");
    assert_no_key(&output);
    let hunk = &report_of(&output)["files"][0]["hunks"][0];
    assert_eq!(hunk["escalation_reason"], "decision_error");
    assert_eq!(hunk["resolver"], "code");
    assert!(hunk.get("decision").is_none(), "{hunk}");
    assert_eq!(hunk["resolution"]["text"], "gamma");
}

#[tokio::test]
async fn decision_5xx_retries_then_escalates() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(move |_request: &Request| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(500)
                .insert_header("retry-after", "0")
                .set_body_string("busy")
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(code_template("gamma", "merged", 0.875))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--apply", "--json", "--retries", "1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "retries=1 means two attempts"
    );
    let hunk = &report_of(&output)["files"][0]["hunks"][0];
    assert_eq!(hunk["escalation_reason"], "decision_error");
    assert_eq!(hunk["resolver"], "code");
    let body = normalize(&fs::read_to_string(dir.path().join("letter.txt")).unwrap());
    assert!(body.contains("gamma"), "{body}");
}

#[tokio::test]
async fn decision_auth_failure_exits_2_without_calling_the_code_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(ResponseTemplate::new(401).set_body_string(format!("nope {KEY}")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(code_template("gamma", "merged", 0.9))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let before = fs::read_to_string(dir.path().join("letter.txt")).unwrap();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--apply"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{:?}", output);
    assert_no_key(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("authentication") || stderr.contains("OpenRouter"),
        "{stderr}"
    );
    let after = fs::read_to_string(dir.path().join("letter.txt")).unwrap();
    assert_eq!(before, after);
    let paths = paths_of(&server.received_requests().await.unwrap());
    assert!(
        paths.iter().all(|path| path.ends_with("/systemone")),
        "auth must not escalate: {paths:?}"
    );
}

#[tokio::test]
async fn malformed_decision_body_escalates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(code_template("gamma", "merged", 0.875))
        .mount(&server)
        .await;
    let dir = single_conflict_repo();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let hunk = &report_of(&output)["files"][0]["hunks"][0];
    assert_eq!(hunk["escalation_reason"], "decision_error");
    assert_eq!(hunk["resolver"], "code");
    assert!(hunk.get("decision").is_none(), "{hunk}");
}

#[tokio::test]
async fn code_model_error_redacts_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(ResponseTemplate::new(400).set_body_string(format!("decision {KEY}")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string(format!("code {KEY}")))
        .mount(&server)
        .await;
    let dir = single_conflict_repo();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--apply", "--json", "--retries", "0"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    assert_no_key(&output);
    let hunk = &report_of(&output)["files"][0]["hunks"][0];
    let rejection = hunk["rejection"].as_str().unwrap();
    assert!(rejection.contains("***"), "{rejection}");
    assert!(!rejection.contains(KEY), "{rejection}");
    assert_eq!(hunk["escalation_reason"], "decision_error");
    assert_eq!(hunk["resolver"], "code");
}

#[tokio::test]
async fn oversize_hunk_skips_the_decision_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(decision_template("ours", 1.0))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(code_template("short", "merged", 0.875))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let huge = "H".repeat(150_000);
    fs::write(
        dir.path().join("letter.txt"),
        format!("keep\n<<<<<<< HEAD\n{huge}\n=======\nother\n>>>>>>> feature\nend\n"),
    )
    .unwrap();
    let output = truce_at(dir.path(), &server.uri())
        .args(["--apply", "--json", "--retries", "0"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let body = normalize(&fs::read_to_string(dir.path().join("letter.txt")).unwrap());
    assert_eq!(body, "keep\nshort\nend\n");
    let hunk = &report_of(&output)["files"][0]["hunks"][0];
    assert_eq!(hunk["escalation_reason"], "too_large");
    assert_eq!(hunk["resolver"], "code");
    assert!(hunk.get("decision").is_none(), "{hunk}");
    let paths = paths_of(&server.received_requests().await.unwrap());
    assert!(
        !paths.iter().any(|path| path.contains("systemone")),
        "{paths:?}"
    );
    assert!(
        paths.iter().any(|path| path.ends_with("/chat/completions")),
        "{paths:?}"
    );
}

#[tokio::test]
async fn systemone_request_body_and_path_match_the_schema() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/systemone"))
        .respond_with(decision_template("ours", 1.0))
        .mount(&server)
        .await;

    let dir = single_conflict_repo();
    let base = format!("{}/api/v1", server.uri());
    let output = truce_at(dir.path(), &base)
        .args(["--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "{:?}", paths_of(&requests));
    assert_eq!(requests[0].method.as_str(), "POST");
    assert_eq!(requests[0].url.path(), "/api/v1/systemone");
    let auth = requests[0]
        .headers
        .get("authorization")
        .map(|value| value.to_str().unwrap_or(""))
        .unwrap_or("");
    assert!(auth.starts_with("Bearer "), "missing bearer scheme");
    assert_eq!(auth.len(), "Bearer ".len() + KEY.len());

    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let expected = json!({
        "model": DECISION_MODEL,
        "questions": {
            "resolution": {
                "criteria": {
                    "ours": "Keep the OURS side unchanged and drop THEIRS. OURS is already a complete, correct resolution.",
                    "theirs": "Keep the THEIRS side unchanged and drop OURS. THEIRS is already a complete, correct resolution.",
                    "both": "Keep both sides by placing every OURS line first and every THEIRS line second, without rewriting either side.",
                    "needs_code": "Neither side alone, nor a plain concatenation of both, is a correct resolution. The hunk needs edited or new code."
                },
                "instructions": "Choose how to resolve this single git merge conflict. Prefer ours or theirs when that side can be kept verbatim. Choose both only when both sides must be retained and concatenating them is safe. Choose needs_code when the fix requires a rewrite.",
                "type": "choice"
            }
        },
        "state": {
            "file": "letter.txt",
            "language": "text",
            "ours": "omega",
            "theirs": "beta",
            "context": "ours_label: HEAD\ntheirs_label: feature\nbefore:\nhello\nafter:\nbye\n"
        }
    });
    assert_eq!(body, expected, "request body:\n{body}");
    assert!(body.get("messages").is_none());
    assert!(body.get("temperature").is_none());
    assert!(body.get("response_format").is_none());
    assert!(body["state"].get("base").is_none());
    let raw = std::str::from_utf8(&requests[0].body).unwrap();
    assert!(!raw.contains(KEY));
}

#[tokio::test]
async fn diff3_state_includes_base() {
    let server = MockServer::start().await;
    mount_decision(&server, "ours", 1.0).await;
    let dir = common::init_repo();
    let root = dir.path();
    common::git(root, &["config", "merge.conflictStyle", "diff3"]);
    common::write(root, "letter.txt", "alpha\n");
    common::git(root, &["add", "."]);
    common::git(root, &["commit", "-m", "base"]);
    common::git(root, &["checkout", "-b", "other"]);
    common::write(root, "letter.txt", "beta\n");
    common::git(root, &["add", "."]);
    common::git(root, &["commit", "-m", "other"]);
    common::git(root, &["checkout", "main"]);
    common::write(root, "letter.txt", "omega\n");
    common::git(root, &["add", "."]);
    common::git(root, &["commit", "-m", "main"]);
    let merge = common::git_allow_fail(root, &["merge", "other", "--no-edit"]);
    assert!(!merge.status.success(), "expected a diff3 conflict");

    let output = truce_at(root, &server.uri())
        .args(["--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(requests[0].url.path(), "/systemone");
    assert_eq!(body["state"]["ours"], "omega");
    assert_eq!(body["state"]["theirs"], "beta");
    assert_eq!(body["state"]["base"], "alpha");
    assert!(body["state"]["context"]
        .as_str()
        .unwrap()
        .contains("base_label:"));
}
