//! `truce update` against a local GitHub API. No real release is downloaded.

mod common;

use std::path::Path;

use common::truce;
use predicates::prelude::*;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn installed_version() -> String {
    let output = truce(Path::new(".")).arg("--version").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    stdout
        .split_whitespace()
        .nth(1)
        .expect("truce --version")
        .trim()
        .to_string()
}

fn release(tag: &str, download: &str) -> serde_json::Value {
    json!({
        "tag_name": tag,
        "assets": [{
            "name": "SHA256SUMS",
            "browser_download_url": download
        }]
    })
}

#[test]
fn update_help_lists_check_version_and_checksum() {
    truce(Path::new("."))
        .args(["update", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--check"))
        .stdout(predicate::str::contains("--version"))
        .stdout(predicate::str::contains("SHA256SUMS"))
        .stdout(predicate::str::contains("sudo"))
        .stdout(predicate::str::contains("without downloading"));
}

#[tokio::test]
async fn already_up_to_date_exits_0_without_downloading() {
    let server = MockServer::start().await;
    let version = installed_version();
    let tag = format!("v{version}");
    Mock::given(method("GET"))
        .and(path("/repos/AJFrio/Truce/releases/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(release(&tag, &format!("{}/do-not-fetch", server.uri()))),
        )
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let output = truce(dir.path())
        .env("TRUCE_UPDATE_API", server.uri())
        .arg("update")
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
        stdout.contains(&format!("truce {version} is up to date")),
        "{stdout}"
    );
    assert!(!stderr.contains("downloading"), "{stderr}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn check_reports_a_newer_release_without_downloading() {
    let server = MockServer::start().await;
    let version = installed_version();
    Mock::given(method("GET"))
        .and(path("/repos/AJFrio/Truce/releases/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(release(
            "v99.0.0",
            &format!("{}/do-not-fetch", server.uri()),
        )))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let output = truce(dir.path())
        .env("TRUCE_UPDATE_API", server.uri())
        .args(["update", "--check"])
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
        stdout.contains(&format!(
            "truce {version} is installed; v99.0.0 is available"
        )),
        "{stdout}"
    );
    assert!(!stderr.contains("downloading"), "{stderr}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn version_flag_matching_this_binary_does_not_download() {
    let server = MockServer::start().await;
    let version = installed_version();
    let tag_path = format!("/repos/AJFrio/Truce/releases/tags/v{version}");
    Mock::given(method("GET"))
        .and(path(tag_path.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(release(
            &format!("v{version}"),
            &format!("{}/do-not-fetch", server.uri()),
        )))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let output = truce(dir.path())
        .env("TRUCE_UPDATE_API", server.uri())
        .args(["update", "--version", &version])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("up to date"), "{stdout}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].url.path(), tag_path);
}
