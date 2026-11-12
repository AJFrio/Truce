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
| `mode` | `report` | `report` previews. `commit` applies, stages, commits, and pushes a `truce/resolve-*` branch only when every conflict resolved (exit 0). A clean merge exits 0 and does not push. Unresolved hunks skip commit and push. |
| `model` | `liquid/d1-ultra` | System One decision model. |
| `code-model` | `~deepseek/deepseek-flash-latest` | Chat model for escalation. |
| `min-confidence` | `0.7` | Escalate a side choice below this. |
| `target-branch` | `github.base_ref` | Branch merged into the PR head before Truce runs. |
| `github-token` | `github.token` | Checkout, and push in commit mode. |

Store the key as the repository secret `OPENROUTER_API_KEY`. Report mode needs `contents: read`. Commit mode needs `contents: write`.
