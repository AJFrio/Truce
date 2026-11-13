//! Shared git fixtures for CLI integration tests.
#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use assert_cmd::Command as Truce;
use tempfile::TempDir;

pub fn git(dir: &Path, args: &[&str]) -> Output {
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

pub fn git_allow_fail(dir: &Path, args: &[&str]) -> Output {
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

pub fn init_repo() -> TempDir {
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

pub fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Never inherit a real OpenRouter key or model from the environment.
pub fn truce(dir: &Path) -> Truce {
    let mut cmd = Truce::cargo_bin("truce").unwrap();
    cmd.current_dir(dir)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("TRUCE_MODEL")
        .env_remove("TRUCE_CODE_MODEL")
        .env_remove("TRUCE_MIN_CONFIDENCE")
        .env_remove("TRUCE_BASE_URL")
        .env_remove("OPENROUTER_BASE_URL")
        .env(
            "XDG_CONFIG_HOME",
            std::env::temp_dir().join(format!("truce-xdg-none-{}", std::process::id())),
        )
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_STEP_SUMMARY")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("TRUCE_UPDATE_API")
        .env_remove("TRUCE_UPDATE_TARGET");
    cmd
}

pub fn mid_merge_repo() -> TempDir {
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

/// Two branches that merge cleanly, currently on `feature`.
pub fn two_branch_clean_repo() -> TempDir {
    let dir = init_repo();
    let root = dir.path();
    write(root, "base.txt", "base\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "base"]);

    git(root, &["checkout", "-b", "feature"]);
    write(root, "feature.txt", "feature\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "feature"]);

    git(root, &["checkout", "main"]);
    write(root, "main.txt", "main\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "main"]);

    git(root, &["checkout", "feature"]);
    dir
}

/// Two branches that conflict, currently on `feature` with a clean tree.
/// The action script fetches/merges `main` (or `TRUCE_MERGE_REF`) itself.
pub fn two_branch_repo() -> TempDir {
    let dir = init_repo();
    let root = dir.path();
    write(root, "letter.txt", "hello\nalpha\nbye\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "base"]);

    git(root, &["checkout", "-b", "feature"]);
    write(root, "letter.txt", "hello\nbeta\nbye\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "feature"]);

    git(root, &["checkout", "main"]);
    write(root, "letter.txt", "hello\nomega\nbye\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "main"]);

    git(root, &["checkout", "feature"]);
    dir
}

pub fn single_conflict_repo() -> TempDir {
    let dir = init_repo();
    let root = dir.path();
    write(root, "letter.txt", "hello\nalpha\nbye\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "base"]);

    git(root, &["checkout", "-b", "feature"]);
    write(root, "letter.txt", "hello\nbeta\nbye\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "feature"]);

    git(root, &["checkout", "main"]);
    write(root, "letter.txt", "hello\nomega\nbye\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "main"]);

    let merge = git_allow_fail(root, &["merge", "feature", "--no-edit"]);
    assert!(!merge.status.success());
    dir
}
