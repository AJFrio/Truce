//! CLI integration tests. These shell out to a real `git` and the `truce` binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use assert_cmd::Command as Truce;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Truce Test")
        .env("GIT_AUTHOR_EMAIL", "truce@example.com")
        .env("GIT_COMMITTER_NAME", "Truce Test")
        .env("GIT_COMMITTER_EMAIL", "truce@example.com")
        .output()
        .unwrap_or_else(|err| panic!("git {:?} failed to spawn: {err}", args));
    if !output.status.success() {
        panic!(
            "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output
}

fn git_allow_fail(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Truce Test")
        .env("GIT_AUTHOR_EMAIL", "truce@example.com")
        .env("GIT_COMMITTER_NAME", "Truce Test")
        .env("GIT_COMMITTER_EMAIL", "truce@example.com")
        .output()
        .unwrap_or_else(|err| panic!("git {:?} failed to spawn: {err}", args))
}

fn init_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let init = git_allow_fail(dir.path(), &["init", "--initial-branch=main"]);
    if !init.status.success() {
        git(dir.path(), &["init"]);
        let _ = git_allow_fail(dir.path(), &["checkout", "-b", "main"]);
    }
    git(dir.path(), &["config", "user.name", "Truce Test"]);
    git(dir.path(), &["config", "user.email", "truce@example.com"]);
    git(dir.path(), &["config", "core.autocrlf", "false"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    git(dir.path(), &["config", "advice.detachedHead", "false"]);
    dir
}

fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn truce(dir: &Path) -> Truce {
    let mut cmd = Truce::cargo_bin("truce").unwrap();
    cmd.current_dir(dir)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");
    cmd
}

/// Two text files (one with a space in the name) plus a binary, all conflicted.
fn mid_merge_repo() -> TempDir {
    let dir = init_repo();
    let root = dir.path();

    write(
        root,
        "src/app.rs",
        "fn start() {\n    boot();\n}\n\nfn helper() {\n    1\n    2\n    3\n    4\n    5\n    6\n    7\n    8\n}\n\nfn stop() {\n    halt();\n}\n",
    );
    write(root, "notes/my file.txt", "title\nshared line\nfooter\n");
    fs::create_dir_all(root.join("assets")).unwrap();
    fs::write(root.join("assets/logo.bin"), b"aaa\0bbb").unwrap();

    git(root, &["add", "."]);
    git(root, &["commit", "-m", "base"]);

    git(root, &["checkout", "-b", "feature"]);
    write(
        root,
        "src/app.rs",
        "fn start() {\n    boot_v2();\n}\n\nfn helper() {\n    1\n    2\n    3\n    4\n    5\n    6\n    7\n    8\n}\n\nfn stop() {\n    halt_v2();\n}\n",
    );
    write(
        root,
        "notes/my file.txt",
        "title\nshared line from feature\nfooter\n",
    );
    fs::write(root.join("assets/logo.bin"), b"ccc\0ddd").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "feature changes"]);

    git(root, &["checkout", "main"]);
    write(
        root,
        "src/app.rs",
        "fn start() {\n    boot_v1();\n}\n\nfn helper() {\n    1\n    2\n    3\n    4\n    5\n    6\n    7\n    8\n}\n\nfn stop() {\n    halt_v1();\n}\n",
    );
    write(
        root,
        "notes/my file.txt",
        "title\nshared line from main\nfooter\n",
    );
    fs::write(root.join("assets/logo.bin"), b"eee\0fff").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "main changes"]);

    let merge = git_allow_fail(root, &["merge", "feature", "--no-edit", "--no-ff"]);
    assert!(
        !merge.status.success(),
        "expected a conflict, merge succeeded:\n{}",
        String::from_utf8_lossy(&merge.stdout)
    );
    dir
}

#[test]
fn help_lists_exit_codes() {
    truce(Path::new("."))
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Exit codes:"))
        .stdout(predicate::str::contains("--json"))
        .stdout(predicate::str::contains("--apply"));
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
fn apply_without_resolver_writes_nothing() {
    let dir = mid_merge_repo();
    let before = fs::read_to_string(dir.path().join("src/app.rs")).unwrap();
    truce(dir.path())
        .args(["--apply", "--no-color"])
        .assert()
        .code(1);
    let after = fs::read_to_string(dir.path().join("src/app.rs")).unwrap();
    assert_eq!(
        before, after,
        "--apply must not rewrite files without resolutions"
    );
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
    // Leave the index unmerged but smash the working-tree markers.
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

/// Touch a path so the compiler keeps PathBuf import used on all platforms.
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
