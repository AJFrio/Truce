# Truce

Resolve git merge conflicts with a fast decision model.

`truce` runs inside a git work tree. It finds unmerged files, parses each conflict hunk (including an optional diff3 / zdiff3 base), asks an OpenRouter decision model, and prints a preview or writes the resolution.

## Install

```bash
cargo install --path .   # from a clone
cargo install --git https://github.com/AJFrio/Truce --locked
```

Requires Rust 1.80+ (`rust-version` in `Cargo.toml`) and `git` on `PATH`. Prebuilt Linux, macOS, and Windows binaries are on [GitHub Releases](https://github.com/AJFrio/Truce/releases), with checksums in `SHA256SUMS`.

## Usage

```bash
export OPENROUTER_API_KEY=sk-or-...   # never printed

truce                      # preview (default)
truce --json
truce --apply              # write files whose hunks all resolved
truce --apply --stage      # write, then git add those files
truce --model liquid/d1-ultra --code-model "~deepseek/deepseek-flash-latest" --min-confidence 0.8
truce --context 5 src/

truce sync                       # fetch origin/main, merge, resolve, commit
truce sync --push                # also push (never a force-push)
truce sync --rebase              # experimental per-commit rebase
truce sync --verify-cmd "cargo test"
```

Run during a merge, rebase, cherry-pick, or revert. `--dry-run` is the default: with a key, the model runs and files stay unchanged. A file is rewritten only when every hunk in it resolved. `git add` runs only with `--stage`. With no key, `truce` lists hunks and skips the model; `--apply` then exits 2.

```
truce [OPTIONS] [PATH]...

  --json                 JSON report (schema_version 1)
  --dry-run              Preview only (default)
  --apply                Write accepted resolutions
  --stage                git add after a successful --apply
  --model <MODEL>        Decision model (default: liquid/d1-ultra)
  --code-model <MODEL>   Chat model for escalation
                         (default: ~deepseek/deepseek-flash-latest)
  --min-confidence <N>   Escalate a side choice below this (default: 0.7)
  --concurrency <N>      Max in-flight hunks (default: 8)
  --timeout <SECS>       Per-request timeout (default: 30)
  --retries <N>          Retries on 429/5xx/timeouts (default: 3)
  --base-url <URL>       API base (tests / proxy)
  --context <N>          Context lines per hunk (default: 3)
  --no-color             Disable ANSI color
  --ci                   GitHub Actions mode (also auto-detected)
  PATH                   Limit scanning to these paths
```

Color is on when stdout is a TTY, and off with `--no-color`, `--ci`, `GITHUB_ACTIONS=true`, `NO_COLOR`, or `TERM=dumb`. Status words (`OURS`, `THEIRS`, `BASE`, `CONFLICTED`, `SKIPPED`, `RESOLVED`, `REJECTED`) are always printed.

Binary and non-UTF-8 files are skipped and left unchanged. Malformed markers are parse errors. A rejected hunk — leftover markers, empty or garbage output, or code-model confidence below `--min-confidence` — stays unresolved, including when a confident decision fails those checks.

## Configuration

First match wins, separately for the decision model, code model, and `min_confidence`: CLI flag, then `TRUCE_MODEL` / `TRUCE_CODE_MODEL` / `TRUCE_MIN_CONFIDENCE`, then `.truce.toml` in the repo root, then `$XDG_CONFIG_HOME/truce/config.toml` (`~/.config/truce/config.toml`, or `%APPDATA%\truce\config.toml` on Windows), then the built-in defaults. `min_confidence` must be in `0..=1`.

```toml
model = "liquid/d1-ultra"
code_model = "~deepseek/deepseek-flash-latest"
min_confidence = 0.7
concurrency = 8
timeout_secs = 30
retries = 3
```

The API base defaults to `https://openrouter.ai/api/v1` (`--base-url`, `TRUCE_BASE_URL`, or `OPENROUTER_BASE_URL`). Decisions post to `{base}/systemone`; escalation posts to `{base}/chat/completions`. The key is a bearer token and is never printed, logged, or included in request bodies.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | No conflicts, or every conflict was resolved. `--help` and `--version` also exit 0. |
| 1 | Unresolved conflicts remain, including skipped binaries |
| 2 | Usage or runtime failure (not a git repo, parse error, missing key on `--apply`, auth failure, IO) |

`truce sync` uses the same numbers. See [truce sync](#truce-sync) for abort, verify, and push.

## JSON (`schema_version` 1)

`--json` prints one object. Optional fields are omitted when null or empty: `model`, `code_model`, `base`, `base_label`, `error`, `resolution`, `rejection`, `decision`, `resolver`, `escalation_reason`, `probabilities`. Field names are the contract. `schema_version` stays `1`.

```json
{
  "schema_version": 1,
  "tool": "truce",
  "repo_root": "/abs/path/to/repo",
  "merge_in_progress": true,
  "dry_run": true,
  "model": "liquid/d1-ultra",
  "code_model": "~deepseek/deepseek-flash-latest",
  "min_confidence": 0.7,
  "files": [
    {
      "path": "src/app.rs",
      "status": "conflicted",
      "line_ending": "lf",
      "trailing_newline": true,
      "hunks": [
        {
          "id": 1,
          "start_line": 2,
          "end_line": 8,
          "ours_label": "HEAD",
          "theirs_label": "feature",
          "ours": ["boot_v1();"],
          "theirs": ["boot_v2();"],
          "context_before": ["fn start() {"],
          "context_after": ["}"],
          "resolution": {
            "text": "boot_v1();",
            "choice": "ours",
            "rationale": "decision model selected ours",
            "confidence": 0.91
          },
          "decision": {
            "model": "liquid/d1-ultra",
            "choice": "ours",
            "confidence": 0.91,
            "probabilities": {"both": 0.02, "needs_code": 0.01, "ours": 0.91, "theirs": 0.06}
          },
          "resolver": "decision"
        }
      ]
    }
  ],
  "summary": {
    "conflicted_files": 1,
    "hunks": 1,
    "skipped_files": 0,
    "resolved_hunks": 1,
    "unresolved_hunks": 0,
    "parse_errors": 0,
    "files_written": 0,
    "files_staged": 0
  }
}
```

- `path` is repo-relative and uses `/`. `status` is `conflicted`, `binary`, `non_utf8`, `missing`, `parse_error`, or `unmerged_without_markers`.
- Hunk line arrays omit EOL characters. `line_ending` is `lf` or `crlf`.
- `resolution.choice` is `ours`, `theirs`, or `merged`. A decision of `both` is stored as `merged`.
- `decision` is the System One answer (`choice`: `ours`, `theirs`, `both`, or `needs_code`) when that call returned one.
- `resolver` is `decision` or `code`. `escalation_reason` is `needs_code`, `low_confidence`, `unknown_choice`, `decision_error`, or `too_large`, and is omitted when the decision path applied the hunk.
- `min_confidence` is always present.

## Resolution

1. Find the work tree and list unmerged paths (`git ls-files -u`, `git diff --diff-filter=U`), NUL-separated.
2. Parse `<<<<<<<`, an optional `|||||||` base, `=======`, and `>>>>>>>`.
3. POST each hunk to `{base}/systemone` with `model`, `state` (file, language, both sides, optional base, context), and a `resolution` choice of `ours`, `theirs`, `both`, or `needs_code`. The body has no `messages`, `temperature`, or `response_format`.
4. `ours`, `theirs`, or `both` at or above `--min-confidence` is applied in Rust: that side unchanged, or for `both` every OURS line then every THEIRS line. One identical seam line is kept once; other duplicates stay. Markers, empty output, or garbage reject the hunk and skip escalation.
5. Other hunks go to `{base}/chat/completions` on `--code-model`: `needs_code`, `low_confidence`, `unknown_choice`, `decision_error` (non-auth 4xx, 5xx after retries, timeout, or a malformed body), or `too_large`. The decision budget is 32,768 tokens. Context is dropped from the outside in; if the sides still do not fit, the decision call is skipped.
6. 401/403 abort the run (exit 2). 429, 5xx, and timeouts retry with jittered backoff and honor `Retry-After`. Up to `--concurrency` hunks run at once; a hunk may call both APIs.
7. `--apply` writes with a temp file and rename, keeps the file's line endings, and writes a file only when every hunk in it validated.

## truce sync

`truce sync` keeps the current branch current with a base branch. Run it before opening a pull request so CI is not the first thing that discovers the branch has fallen behind.

```bash
truce sync
  --base origin/main          # default; fetched, then merged
  --rebase                    # experimental; merge is the default
  --push                      # push only when this is set
  --verify-cmd "cargo test"   # after resolution, before the commit
  --json
  --model liquid/d1-ultra
  --code-model "~deepseek/deepseek-flash-latest"
  --min-confidence 0.8
  --ci
```

Model, code-model, min-confidence, concurrency, timeout, retries, and base-url are the same flags as `truce`, with the same config and environment precedence.

### Flow

1. Refuse (exit 2) on a detached HEAD, a dirty work tree (including untracked files), a merge, rebase, cherry-pick, or revert already in progress, or unmerged index entries. An in-progress merge is left as it is.
2. Fetch `--base` (default `origin/main`). If that ref does not exist on the remote, use the remote's default branch (`git remote set-head --auto`, then `origin/HEAD`).
3. If the base is already an ancestor of HEAD, exit 0 and do not create a commit. `--push` still pushes when you asked for it.
4. Require a git identity (`user.name` and `user.email`, or `GIT_AUTHOR_*` / `GIT_COMMITTER_*`) before merging or rebasing. Truce does not write git config. The GitHub Action sets `github-actions[bot]` first.
5. Merge the base (default) or rebase onto it (`--rebase`).
6. When there are conflicts, run the ENG-58 resolver: System One `liquid/d1-ultra`, escalating to `--code-model`. Stage and commit only when every hunk is accepted.
7. `--verify-cmd` runs after that resolution is staged and before the commit, through `sh -c` (or `cmd /C` on Windows).
8. Push with `git push origin HEAD` only when `--push` is set. Never a force-push.

### Clean merge

A clean integration does not call the model and does not add a Truce commit on top of git's.

| Situation | What git does | Truce commit message |
| --- | --- | --- |
| Already up to date | nothing | none (no-op, exit 0) |
| Fast-forward | HEAD moves to the base; no new commit | none |
| Diverged, no conflicts | git's ordinary merge commit (`Merge …`) | none |
| Conflicts resolved | one merge commit that records the resolutions | `truce: sync with <base>`, listing each file, hunk, resolver (`decision` or `code`), confidence, and choice |

`--rebase` replays each commit. Conflicted commits are resolved one at a time, then `git rebase --continue` keeps the original message. The JSON report lists the hunks. Rebase is experimental. While a commit is replaying, OURS is the branch you are rebasing onto and THEIRS is the commit being replayed. A failure runs `git rebase --abort`.

### Abort

These restore the original HEAD with `git merge --abort` or `git rebase --abort` (then `git reset --hard` if HEAD is still moved). Nothing is left half-resolved, and conflict markers are not committed.

| Cause | Exit |
| --- | --- |
| A hunk is unresolved, or still below `--min-confidence` after escalation | 1 |
| Escalation or the decision API fails (including auth and a missing key once conflicts exist) | 2 |
| A conflicted file is binary or not UTF-8, or the conflict is modify/delete, rename/delete, rename/rename, directory/file, or a submodule | 1 (parse errors in markers are 2) |
| `--verify-cmd` exits non-zero | 1 |
| The shell that should run `--verify-cmd` cannot be started | 2 |

A failed `--push` does **not** roll back the commit that already succeeded. Exit 2, and the report's `error` says the push failed. Every other failure restores the original HEAD.

Dirty tree, detached HEAD, and "already in progress" are refusals (exit 2) before fetch or merge.

### Sync JSON (`schema_version` 1)

`--json` prints one object. `command` is `sync`, which distinguishes it from the resolve report. `schema_version` stays `1`. `commit` and `error` are omitted when they do not apply. `branch` is omitted on a detached HEAD (sync refuses before then).

```json
{
  "schema_version": 1,
  "tool": "truce",
  "command": "sync",
  "repo_root": "/abs/path/to/repo",
  "branch": "feature",
  "requested_base": "origin/main",
  "base": "origin/main",
  "strategy": "merge",
  "outcome": "resolved",
  "exit_code": 0,
  "aborted": false,
  "pushed": false,
  "merge_in_progress": false,
  "head": "0123abcd",
  "commit": "0123abcd",
  "model": "liquid/d1-ultra",
  "code_model": "~deepseek/deepseek-flash-latest",
  "min_confidence": 0.7,
  "resolved": [
    {
      "path": "letter.txt",
      "hunk": 1,
      "resolver": "decision",
      "confidence": 0.95,
      "choice": "ours"
    }
  ],
  "files": [],
  "summary": {
    "conflicted_files": 1,
    "hunks": 1,
    "skipped_files": 0,
    "resolved_hunks": 1,
    "unresolved_hunks": 0,
    "parse_errors": 0,
    "files_written": 1,
    "files_staged": 1
  }
}
```

`outcome` is `up_to_date`, `fast_forward`, `clean_merge`, `clean_rebase`, `resolved`, `aborted`, or `refused`. `strategy` is `merge` or `rebase`. `resolved[].resolver` is `decision` or `code`. `files` uses the same file and hunk objects as the resolve report. On an abort, `summary.files_written` is 0: the tree was restored. `requested_base` differs from `base` when the requested ref was missing and Truce used the remote's default branch.

`--ci` (or `GITHUB_ACTIONS=true`) emits `::error` / `::warning` annotations and appends a `## Truce sync` table to `$GITHUB_STEP_SUMMARY`, then the usual Truce file table.

### Agents and a pre-push hook

Before opening a PR, on the feature branch:

```bash
truce sync --base origin/main
git push -u origin HEAD
```

[`examples/pre-push`](examples/pre-push) does the sync at push time and does not pass `--push` (the push is already running). If sync creates a commit, that commit is not part of the push that invoked the hook, so the hook exits 1 and you push again.

```bash
cp examples/pre-push .git/hooks/pre-push
chmod +x .git/hooks/pre-push
```

`TRUCE_SYNC_BASE` overrides the hook's base (default `origin/main`).

### Sync in pull request CI

Copy [`examples/truce-sync.yml`](examples/truce-sync.yml). It runs `truce sync` as the first step, then the tests, in the same job. `push` defaults to false, so the sync commit exists only in that checkout: the tests see it, the PR branch does not move, and the workflow does not retrigger. A later step that checks the repo out again will not see the sync. Set `push: true` and `contents: write` only when the branch itself must move, and expect the push to start another `pull_request` run.

The composite action accepts `mode: sync`. Pushing stays opt-in via the `push` input.

## GitHub Actions

`--ci`, or `GITHUB_ACTIONS=true`, turns off color, emits `::error` / `::warning` annotations, and appends a Markdown summary to `$GITHUB_STEP_SUMMARY`. Exit codes stay 0 / 1 / 2.

The composite action is `action.yml`. Copy [`examples/truce-pr.yml`](examples/truce-pr.yml) into the consuming repo; that example is not active here.

```yaml
- uses: AJFrio/Truce@v1
  with:
    openrouter-api-key: ${{ secrets.OPENROUTER_API_KEY }}
    mode: report   # or commit
```

| Input | Default | Notes |
| --- | --- | --- |
| `openrouter-api-key` | required | `secrets.OPENROUTER_API_KEY`. Never echoed. |
| `mode` | `report` | `report` previews. `commit` applies, stages, commits, and pushes a `truce/resolve-*` branch only when every conflict resolved (exit 0). `sync` runs `truce sync` (fetch, merge, resolve, commit) and does not push unless `push` is true. |
| `push` | `false` | Sync mode only. `true` passes `--push`. |
| `verify-cmd` | empty | Sync mode only. Shell command after resolution and before the sync commit. |
| `model` | `liquid/d1-ultra` | System One decision model. |
| `code-model` | `~deepseek/deepseek-flash-latest` | Chat model for escalation. |
| `min-confidence` | `0.7` | Escalate a side choice below this. |
| `target-branch` | `github.base_ref` | Branch merged into the PR head. For `merge_group`, set this from `github.event.merge_group.base_ref` (the action strips `refs/heads/`). |
| `github-token` | `github.token` | Checkout, and push in commit mode or when `push` is true. |

Store the key as the repository secret `OPENROUTER_API_KEY`. Report mode and sync without push need `contents: read`. Commit mode, and sync with `push: true`, need `contents: write`.

### Merge queue

Copy [`examples/truce-merge-queue.yml`](examples/truce-merge-queue.yml). The workflow runs on `pull_request` and `merge_group`. The job name is `truce`. It syncs (push off), then runs the tests. That job is the required check.

Enable the queue on the base branch (public repositories, GitHub Team, and Enterprise Cloud):

1. Settings → Rules → Rulesets (or Branch protection rules).
2. Require a pull request before merging.
3. Require status checks to pass, and select the check named `truce` (the job name, not the workflow name).
4. Require merge queue. Pull requests then enter the queue from the merge button instead of merging immediately.

The workflow has to listen for `merge_group`. A required check that never reports on that event stalls the queue.

Limitations, plainly:

- GitHub creates the merge group on the server. If the PR conflicts with the latest base, GitHub drops it from the queue and does not hand the Actions runner a tree full of conflict markers. Truce cannot fix that server-side conflict.
- In the runner, `truce sync` merges the base into the queue checkout and resolves conflicts that merge produces, then the tests run. The queue branch is `gh-readonly-queue/…`. Pushing it is not supported. Leave `push` false. The queue merges when the required check passes.
- Truce still will not commit a binary, modify/delete, rename/delete, or rename/rename conflict. Those fail the check and GitHub removes the group.
- Sync without push changes only this job's checkout. Do not `actions/checkout` again before the tests if they need the synced tree.
- The action's checkout ref is `github.head_ref` or, for `merge_group` (where `head_ref` is empty), `github.ref`. Fetch depth is 0 so the merge has history.
