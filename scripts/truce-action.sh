#!/usr/bin/env bash
# Core of the Truce GitHub Action. Invoked by action.yml.
# Secrets (OPENROUTER_API_KEY) must arrive via the environment, never argv.
set -euo pipefail

# Do not `set -x`: command traces can expand secrets.

mode="${TRUCE_MODE:-report}"
target="${TRUCE_TARGET_BRANCH:-}"
bin="${TRUCE_BIN:-truce}"
min_conf="${TRUCE_MIN_CONFIDENCE:-}"
model="${TRUCE_MODEL:-}"

if [[ "${OPENROUTER_API_KEY:-}" == *[[:space:]]* ]]; then
  echo "error: OPENROUTER_API_KEY must not contain whitespace" >&2
  exit 2
fi

# A GitHub runner has no commit identity. Set it before any merge/commit
# so a clean merge is not rejected, then hidden by `|| true`.
git config user.name "${GIT_AUTHOR_NAME:-github-actions[bot]}"
git config user.email "${GIT_AUTHOR_EMAIL:-41898282+github-actions[bot]@users.noreply.github.com}"

note() {
  echo "$1"
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    printf '%s\n' "$1" >> "${GITHUB_STEP_SUMMARY}"
  fi
}

if [[ "${TRUCE_SKIP_FETCH:-}" != "1" ]]; then
  if [[ -z "$target" ]]; then
    target="${GITHUB_BASE_REF:-main}"
  fi
  git fetch --no-tags origin "$target"
fi

if [[ "${TRUCE_SKIP_MERGE:-}" != "1" ]]; then
  if [[ -z "${TRUCE_MERGE_REF:-}" ]]; then
    if [[ -z "$target" ]]; then
      target="${GITHUB_BASE_REF:-main}"
    fi
    TRUCE_MERGE_REF="origin/${target}"
  fi
  echo "Merging ${TRUCE_MERGE_REF} into $(git rev-parse --abbrev-ref HEAD)"
  set +e
  merge_out="$(git merge --no-edit --no-stat "${TRUCE_MERGE_REF}" 2>&1)"
  merge_code=$?
  set -e
  if [[ -n "$merge_out" ]]; then
    printf '%s\n' "$merge_out"
  fi

  if [[ "$merge_code" -eq 0 ]]; then
    note "Merged cleanly; nothing to resolve"
    exit 0
  fi

  unmerged="$(git ls-files -u)"
  if [[ -n "$unmerged" ]]; then
    echo "Merge produced conflicts; continuing with truce"
  else
    echo "error: git merge failed without conflicts (exit ${merge_code})" >&2
    if [[ -n "$merge_out" ]]; then
      printf '%s\n' "$merge_out" >&2
    fi
    exit 2
  fi
fi

args=(--ci --no-color)
if [[ -n "$model" ]]; then
  args+=(--model "$model")
fi
if [[ -n "$min_conf" ]]; then
  args+=(--min-confidence "$min_conf")
fi

if [[ "$mode" == "commit" ]]; then
  args+=(--apply --stage)
elif [[ "$mode" != "report" ]]; then
  echo "error: TRUCE_MODE must be 'report' or 'commit' (got ${mode})" >&2
  exit 2
fi

echo "Running truce (mode=${mode}; key_set=${OPENROUTER_API_KEY:+yes})"
set +e
"${bin}" "${args[@]}"
code=$?
set -e

if [[ "$mode" == "commit" ]]; then
  unmerged="$(git ls-files -u)"
  if [[ "$code" -ne 0 || -n "$unmerged" ]]; then
    note "Skipping commit and push: truce exited ${code}; unresolved conflicts remain."
    if [[ "$code" -eq 0 ]]; then
      code=1
    fi
  elif [[ -n "$(git diff --cached --name-only)" ]]; then
    git commit -m "truce: resolve merge conflicts"
    if [[ "${TRUCE_SKIP_PUSH:-}" != "1" ]]; then
      branch="truce/resolve-${GITHUB_HEAD_REF:-${GITHUB_SHA:-local}}"
      branch="${branch// /_}"
      git checkout -B "$branch"
      git push -u origin "HEAD:refs/heads/${branch}"
      echo "Pushed resolutions to ${branch}"
    else
      echo "Committed resolutions (push skipped)"
    fi
  else
    echo "No staged resolutions to commit"
  fi
fi

exit "$code"
