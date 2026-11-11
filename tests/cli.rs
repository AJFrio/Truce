//! CLI integration tests. These shell out to a real `git` and the `truce` binary.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{git, git_allow_fail, init_repo, mid_merge_repo, truce, write};
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

#[test]
fn help_lists_exit_codes() {
    truce(Path::new("."))
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Exit codes:"))
        .stdout(predicate::str::contains("--json"))
        .stdout(predicate::str::contains("--apply"))
        .stdout(predicate::str::contains("--model"))
        .stdout(predicate::str::contains("--stage"));
}

#[test]
fn outside_a_repo_exits_2() {
    let dir = TempDir::new().unwrap();
    truce(dir.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not a git repository"));
}

#[test]
fn clean_repo_exits_0() {
    let dir = init_repo();
    write(dir.path(), "README.md", "hello\n");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "init"]);

    truce(dir.path())
        .assert()
        .code(0)
        .stdout(predicate::str::contains("No merge in progress"));
}

#[test]
fn mid_merge_lists_every_hunk_json() {
    let dir = mid_merge_repo();
    let output = truce(dir.path())
        .args(["--json", "--no-color"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value = serde_json::from_slice(&output.stdout).expect("json report");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["tool"], "truce");
    assert_eq!(report["merge_in_progress"], true);
    assert_eq!(report["dry_run"], true);

    let files = report["files"].as_array().expect("files array");
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert!(
        paths.contains(&"src/app.rs"),
        "missing src/app.rs in {paths:?}"
    );
    assert!(
        paths.contains(&"notes/my file.txt"),
        "missing spaced path in {paths:?}"
    );
    assert!(
        paths.contains(&"assets/logo.bin"),
        "missing binary in {paths:?}"
    );

    let app = files.iter().find(|f| f["path"] == "src/app.rs").unwrap();
    assert_eq!(app["status"], "conflicted");
    let hunks = app["hunks"].as_array().unwrap();
    assert_eq!(
        hunks.len(),
        2,
        "src/app.rs should have two hunks: {hunks:?}"
    );
    assert_eq!(hunks[0]["id"], 1);
    assert_eq!(hunks[1]["id"], 2);
    assert!(hunks[0]["ours"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l.as_str().unwrap().contains("boot_v1")));
    assert!(hunks[0]["theirs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l.as_str().unwrap().contains("boot_v2")));
    assert!(hunks[1]["ours"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l.as_str().unwrap().contains("halt_v1")));
    assert!(hunks[1]["theirs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l.as_str().unwrap().contains("halt_v2")));
    assert!(hunks[0]["start_line"].as_u64().unwrap() >= 1);
    assert!(hunks[0]["end_line"].as_u64().unwrap() > hunks[0]["start_line"].as_u64().unwrap());

    let notes = files
        .iter()
        .find(|f| f["path"] == "notes/my file.txt")
        .unwrap();
    assert_eq!(notes["status"], "conflicted");
    assert_eq!(notes["hunks"].as_array().unwrap().len(), 1);

    let blob = files
        .iter()
        .find(|f| f["path"] == "assets/logo.bin")
        .unwrap();
    assert_eq!(blob["status"], "binary");
    assert_eq!(blob["hunks"].as_array().unwrap().len(), 0);

    assert_eq!(report["summary"]["hunks"], 3);
    assert!(report["summary"]["skipped_files"].as_u64().unwrap() >= 1);
}

#[test]
fn mid_merge_human_output_uses_labels() {
    let dir = mid_merge_repo();
    truce(dir.path())
        .arg("--no-color")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("OURS"))
        .stdout(predicate::str::contains("THEIRS"))
        .stdout(predicate::str::contains("CONFLICTED"))
        .stdout(predicate::str::contains("SKIPPED"))
        .stdout(predicate::str::contains("src/app.rs"))
        .stdout(predicate::str::contains("notes/my file.txt"));
}

#[test]
fn apply_without_api_key_exits_2() {
    let dir = mid_merge_repo();
    let before = fs::read_to_string(dir.path().join("src/app.rs")).unwrap();
    truce(dir.path())
        .args(["--apply", "--no-color"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("OPENROUTER_API_KEY"));
    let after = fs::read_to_string(dir.path().join("src/app.rs")).unwrap();
    assert_eq!(before, after);
    assert!(before.contains("<<<<<<<"));
}

#[test]
fn path_filter_limits_report() {
    let dir = mid_merge_repo();
    let output = truce(dir.path())
        .args(["--json", "src/app.rs"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let files = report["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "src/app.rs");
}

#[test]
fn diff3_style_includes_base() {
    let dir = init_repo();
    let root = dir.path();
    git(root, &["config", "merge.conflictStyle", "diff3"]);
    write(root, "letter.txt", "alpha\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "base"]);

    git(root, &["checkout", "-b", "other"]);
    write(root, "letter.txt", "beta\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "other"]);

    git(root, &["checkout", "main"]);
    write(root, "letter.txt", "omega\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "main"]);

    let merge = git_allow_fail(root, &["merge", "other", "--no-edit"]);
    assert!(!merge.status.success());

    let output = truce(root).arg("--json").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let hunk = &report["files"][0]["hunks"][0];
    assert!(hunk.get("base").is_some(), "expected diff3 base: {hunk}");
    assert!(hunk.get("base_label").is_some());
}

#[test]
fn parse_error_on_unmerged_file_exits_2() {
    let dir = mid_merge_repo();
    fs::write(
        dir.path().join("src/app.rs"),
        "<<<<<<< HEAD\nthis is never closed\n",
    )
    .unwrap();

    let output = truce(dir.path()).arg("--json").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let app = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "src/app.rs")
        .unwrap();
    assert_eq!(app["status"], "parse_error");
    assert!(app["error"].as_str().unwrap().contains("unterminated"));
}

#[test]
fn no_color_env_strips_ansi() {
    let dir = mid_merge_repo();
    let output = truce(dir.path()).env("NO_COLOR", "1").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "ANSI leaked with NO_COLOR: {stdout}"
    );
}

#[test]
fn repo_root_is_absolute() {
    let dir = init_repo();
    write(dir.path(), "f.txt", "x\n");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "x"]);
    let output = truce(dir.path()).arg("--json").output().unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let root = PathBuf::from(report["repo_root"].as_str().unwrap());
    assert!(root.is_absolute());
}
