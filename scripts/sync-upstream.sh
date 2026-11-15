#!/usr/bin/env bash
# Rebase this fork's branch onto raphamorim/rio.
#
# Adds the `upstream` remote when missing, saves the current tip as
# backup/<branch>-pre-rebase-<timestamp>, enables rerere so a resolution recorded
# once is replayed on the next sync, then starts the rebase. Resolve conflicts
# as usual and `git rebase --continue`; undo with
# `git reset --hard backup/<branch>-pre-rebase-<timestamp>`.
#
# Intermediate fork commits are not guaranteed to build. Check the final tree:
#   cargo +nightly clippy --workspace --all-targets && cargo +nightly test --workspace
set -Eeuo pipefail

UPSTREAM_URL=${UPSTREAM_URL:-https://github.com/raphamorim/rio.git}
UPSTREAM_REF=${UPSTREAM_REF:-upstream/main}

if [[ -n $(git status --porcelain --untracked-files=no) ]]; then
  echo "sync-upstream: commit or stash tracked changes first" >&2
  exit 1
fi

if ! git remote get-url upstream >/dev/null 2>&1; then
  git remote add upstream "$UPSTREAM_URL"
fi
git fetch upstream

branch=$(git symbolic-ref --short HEAD)
backup="backup/$branch-pre-rebase-$(date -u +%Y%m%dT%H%M%SZ)"
git branch "$backup"
git config rerere.enabled true
git config rerere.autoUpdate true

echo "sync-upstream: $branch saved as $backup;" \
  "$(git rev-list --count "$UPSTREAM_REF..HEAD") fork commits onto" \
  "$(git rev-list --count "HEAD..$UPSTREAM_REF") new upstream commits"
exec git rebase "$UPSTREAM_REF"
