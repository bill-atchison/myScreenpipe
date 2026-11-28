#!/usr/bin/env bash
# Sync this worktree's changes (vs origin/myScreenpipe) to the Mac test worktree, then run a command there.
#   bash .superpowers/tagging/mac-test.sh 'cargo test -p screenpipe-db --lib project_tags'
# Never touches ~/Workspace/myScreenpipe (the installed app's clone).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
REMOTE=Workspace/wt-e7e1f54d
files=$( { git diff --name-only origin/myScreenpipe; git ls-files --others --exclude-standard; } | grep -v '^\.superpowers/\|^\.dashboard/' | sort -u)
# Deleted files: remove on the Mac too.
for f in $files; do [ -e "$f" ] || ssh -o BatchMode=yes mac "rm -f ~/$REMOTE/$f"; done
present=$(for f in $files; do [ -e "$f" ] && echo "$f"; done)
if [ -n "$present" ]; then
  # -m gives fresh mtimes so cargo never runs a stale build; LF normalisation for Windows checkouts.
  tar -cf - $present | ssh -o BatchMode=yes mac "cd ~/$REMOTE && tar -xmf - && for f in $(echo $present); do perl -pi -e 's/\r\n/\n/' \"\$f\" 2>/dev/null || true; done"
fi
ssh -o BatchMode=yes mac "zsh -lc $(printf '%q' "cd ~/$REMOTE && ulimit -n 10240 && $1")"
