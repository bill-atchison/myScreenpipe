#!/usr/bin/env bash
# screenpipe — AI that knows everything you've seen, said, or heard
# https://screenpipe.com
#
# Edit on the Windows VM, build and run on the Mac host over SSH.
#   personal/mac.sh build [branch]       fast-forward the Mac clone to origin/<branch>, then build:tauri:dev
#   personal/mac.sh run [--stop-installed] launch the dev build (port 3130, ~/.screenpipe-dev)
#   personal/mac.sh stop                 quit the dev build and reopen the installed app
#   personal/mac.sh logs [-f] [lines]    tail the dev build's newest log
#   personal/mac.sh status               Mac checkout, installed + dev instances, build queue
#
# TCC: the dev build runs inside Terminal.app, so Screen Recording, Accessibility and
# Microphone are granted to Terminal once and survive rebuilds (verified 2026-10-22);
# no signed .app needed. Granting Screen Recording quits Terminal and the dev build:
# rerun `run` afterwards. Sign in with "Sign in inside screenpipe instead"; the
# browser's deep link goes to the installed app, not this unbundled binary.
set -euo pipefail

MAC_HOST="${MAC_HOST:-mac}"
MAC_REPO="${MAC_REPO:-Workspace/myScreenpipe}"
DEV_BIN="apps/screenpipe-app-tauri/src-tauri/target/debug-dev/screenpipe-app"
# Brackets keep pgrep/pkill from matching the `zsh -lc` that carries these patterns.
DEV_PAT="target/debug-dev/[s]creenpipe-app"
# Main installed process only, not its --screenpipe-acp-* children.
PROD_PAT="screenpipe.app/Contents/MacOS/[s]creenpipe-app( --autostart)?$"
# Recording is the critical path: reopen the installed recorder and confirm it stayed up,
# since a launch that loses the single-instance handoff exits within seconds.
REOPEN_PROD="
  if ! pgrep -f '$PROD_PAT' >/dev/null && [ -d /Applications/screenpipe.app ]; then
    open /Applications/screenpipe.app
    for i in {1..20}; do pgrep -f '$PROD_PAT' >/dev/null && break; sleep 1; done
    sleep 5
    pgrep -f '$PROD_PAT' >/dev/null || { echo 'error: installed screenpipe did not stay up; open it manually' >&2; exit 1; }
    echo 'reopened installed screenpipe'
  fi"

# Login shell: a plain `ssh mac cmd` lacks the Homebrew PATH (git-lfs, bun, cargo).
onmac() {
  ssh -o BatchMode=yes "$MAC_HOST" "zsh -lc $(printf '%q' "cd ~/$MAC_REPO && $1")"
}

cmd="${1:-}"; shift || true
case "$cmd" in
  build)
    branch="${1:-$(git branch --show-current)}"
    git fetch -q origin "$branch" || { echo "error: origin/$branch not found; push it first" >&2; exit 1; }
    if [ -n "$(git rev-list "origin/$branch..$branch" 2>/dev/null)" ]; then
      echo "error: $branch has commits not on origin; push them first (the Mac builds origin/$branch)" >&2
      exit 1
    fi
    onmac "
      # Native builds rewrite tracked gen/schemas (AGENTS.md); drop only that noise.
      git checkout -- apps/screenpipe-app-tauri/src-tauri/gen/schemas
      git diff --quiet && git diff --cached --quiet || { echo 'error: Mac checkout has local changes' >&2; exit 1; }
      git fetch -q origin $(printf '%q' "$branch")
      git switch -q $(printf '%q' "$branch") 2>/dev/null || git switch -q -c $(printf '%q' "$branch") --track origin/$(printf '%q' "$branch")
      git merge -q --ff-only origin/$(printf '%q' "$branch")
      git log -1 --oneline
      cd apps/screenpipe-app-tauri && bun install --frozen-lockfile && bun run build:tauri:dev
    "
    ;;
  run)
    stop_installed=0
    [ "${1:-}" = "--stop-installed" ] && stop_installed=1
    onmac "
      if pgrep -f '$DEV_PAT' >/dev/null; then echo 'error: dev build already running; use stop first' >&2; exit 1; fi
      [ -x '$DEV_BIN' ] || { echo 'error: no dev build; run build first' >&2; exit 1; }
      # Dev isolation gives the debug build its own port and data dir, but both share the
      # single-instance identifier, so a running installed app swallows the dev launch.
      if pgrep -f '$PROD_PAT' >/dev/null; then
        [ $stop_installed = 1 ] || { echo 'error: installed screenpipe is running (it blocks the dev build); rerun with --stop-installed' >&2; exit 1; }
        echo 'stopping installed screenpipe (recording pauses until: personal/mac.sh stop)'
        # SIGTERM, as the app's own port-conflict reclaim does; AppleScript quit opens a confirm dialog.
        pkill -TERM -f '$PROD_PAT'
        for i in {1..30}; do pgrep -f '$PROD_PAT' >/dev/null || break; sleep 1; done
        if pgrep -f '$PROD_PAT' >/dev/null; then echo 'error: installed screenpipe did not exit' >&2; exit 1; fi
      fi
      # Terminal.app launches it in the GUI session; an sshd child has no desktop or TCC grants.
      open -a Terminal \"\$PWD/$DEV_BIN\"
      for i in {1..20}; do pgrep -f '$DEV_PAT' >/dev/null && break; sleep 1; done
      # Stay-up check: a startup crash appears briefly and would otherwise read as success.
      sleep 5
      if ! pgrep -f '$DEV_PAT' >/dev/null; then
        echo 'error: dev build did not start or exited during startup' >&2
        $REOPEN_PROD
        exit 1
      fi
      echo 'launched; follow with: personal/mac.sh logs -f'
    "
    ;;
  stop)
    onmac "
      pkill -f '$DEV_PAT' && echo 'dev build stopped' || echo 'dev build not running'
      # SIGTERM lets it flush capture and its DB; wait rather than SIGKILL a writer.
      for i in {1..60}; do pgrep -f '$DEV_PAT' >/dev/null || break; sleep 1; done
      # Reopening now would hand the launch to the still-alive dev build (shared single-instance id).
      if pgrep -f '$DEV_PAT' >/dev/null; then
        echo 'error: dev build still running after 60s; installed screenpipe NOT reopened, rerun stop' >&2; exit 1
      fi
      $REOPEN_PROD
    "
    ;;
  logs)
    follow=""; [ "${1:-}" = "-f" ] && { follow="-F"; shift; }
    lines="${1:-100}"
    onmac "tail -n $(printf '%q' "$lines") $follow \"\$(ls -t ~/.screenpipe-dev/screenpipe-app.*.log | head -1)\""
    ;;
  status)
    onmac "
      echo '== checkout'; git branch --show-current; git log -1 --oneline
      echo '== installed (3030)'; pgrep -fl '$PROD_PAT' || echo 'not running'
      echo '== dev build (3130)'; pgrep -fl '$DEV_PAT' || echo 'not running'
      echo '== build queue'; cd apps/screenpipe-app-tauri && bun run build:tauri:status
    "
    ;;
  *)
    sed -n '5,10s/^# \{0,1\}//p' "$0"; exit 1
    ;;
esac
