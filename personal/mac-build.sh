#!/usr/bin/env bash
# screenpipe — AI that knows everything you've seen, said, or heard
# https://screenpipe.com
#
# Run ON THE MAC to make the personal release build. It always sets the personal
# flag, so a forgotten export can never produce an upstream build (auto-update and
# telemetry back on). Afterwards install from the VM with: personal/mac.sh install
#   personal/mac-build.sh [branch]   default branch: myScreenpipe
set -euo pipefail

branch="${1:-myScreenpipe}"
cd "$(dirname "$0")/.."

git checkout -- apps/screenpipe-app-tauri/src-tauri/gen/schemas 2>/dev/null || true
git diff --quiet && git diff --cached --quiet || { echo "error: checkout has local changes" >&2; exit 1; }
git fetch -q origin "$branch"
git switch -q "$branch"
git merge -q --ff-only "origin/$branch"
echo "building personal release from $(git log -1 --oneline)"

export NEXT_PUBLIC_SCREENPIPE_PERSONAL_BUILD=1
cd apps/screenpipe-app-tauri
bun install --frozen-lockfile
bun tauri build
echo "done; install from the VM with: personal/mac.sh install"
