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
#   personal/mac.sh install              swap the Mac's release build into /Applications and start it
#
# TCC: the dev build runs inside Terminal.app, so Screen Recording, Accessibility and
# Microphone are granted to Terminal once and survive rebuilds (verified 2026-10-22);
# no signed .app needed. Granting Screen Recording quits Terminal and the dev build:
# rerun `run` afterwards. Sign in with "Sign in inside screenpipe instead"; the
# browser's deep link goes to the installed app, not this unbundled binary.
set -euo pipefail

MAC_HOST="${MAC_HOST:-mac}"
MAC_REPO="${MAC_REPO:-Workspace/myScreenpipe}"
# Self-signed code-signing certificate in the Mac's login keychain (PERSONAL.md).
SIGN_IDENTITY="${SIGN_IDENTITY:-screenpipe personal}"
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
    [ -n "$branch" ] || { echo "error: detached HEAD; pass a branch" >&2; exit 1; }
    # Also rejects a leading '-', which fetch/switch would parse as an option.
    git check-ref-format --branch "$branch" >/dev/null || { echo "error: invalid branch name '$branch'" >&2; exit 1; }
    qb=$(printf '%q' "$branch")
    git fetch -q origin "$branch" || { echo "error: origin/$branch not found; push it first" >&2; exit 1; }
    if [ -n "$(git rev-list "origin/$branch..$branch" 2>/dev/null)" ]; then
      echo "error: $branch has commits not on origin; push them first (the Mac builds origin/$branch)" >&2
      exit 1
    fi
    onmac "
      # A failed fetch, switch or ff-only merge must not fall through to building stale code.
      setopt err_exit
      # Native builds rewrite tracked gen/schemas (AGENTS.md); drop only that noise.
      git checkout -- apps/screenpipe-app-tauri/src-tauri/gen/schemas
      git diff --quiet && git diff --cached --quiet || { echo 'error: Mac checkout has local changes' >&2; exit 1; }
      git fetch -q origin $qb
      git switch -q $qb 2>/dev/null || git switch -q -c $qb --track origin/$qb
      git merge -q --ff-only origin/$qb
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
    # (N) makes an empty glob expand to nothing instead of zsh's "no matches found".
    onmac "
      logs=(~/.screenpipe-dev/screenpipe-app.*.log(N))
      [ \${#logs} -gt 0 ] || { echo 'no dev logs yet' >&2; exit 1; }
      tail -n $(printf '%q' "$lines") $follow \"\$(ls -t \$logs | head -1)\"
    "
    ;;
  install)
    # The Owner builds the release bundle on the Mac (`bun tauri build`); this only installs it.
    # Quit from the menu keeps the app alive in the tray, and the running app only hands over
    # to a copy with the same signing identity (manual_handoff/signing.rs), so: stop it by
    # SIGTERM, swap the bundle, sign with the stable certificate if there is one, then open.
    onmac "
      setopt err_exit
      new='apps/screenpipe-app-tauri/src-tauri/target/release/bundle/macos/screenpipe.app'
      ent='apps/screenpipe-app-tauri/src-tauri/entitlements.plist'
      [ -d \"\$new\" ] || { echo \"error: no release build at \$new; run bun tauri build first\" >&2; exit 1; }
      # Same identity every build keeps the handoff working and TCC grants across rebuilds.
      id='$SIGN_IDENTITY'
      if ! security find-identity -v -p codesigning | grep -qF \"\\\"\$id\\\"\"; then
        if security find-identity -p codesigning | grep -qF \"\\\"\$id\\\"\"; then
          echo \"warning: '\$id' exists but is not trusted for code signing (PERSONAL.md, step 3); signing ad hoc\" >&2
        else
          echo \"warning: no '\$id' code-signing certificate (see PERSONAL.md); signing ad hoc, so macOS will ask for permissions again\" >&2
        fi
        id=-
      fi
      # Recording is the critical path: sign and verify a staged copy BEFORE stopping the app,
      # so a signing failure never pauses capture.
      staged=/tmp/screenpipe-staged.app
      rm -rf \"\$staged\"
      ditto \"\$new\" \"\$staged\"
      if [ \"\$id\" = - ]; then
        codesign --force --deep --sign - --entitlements \"\$PWD/\$ent\" \"\$staged\"
      else
        # An ssh session cannot use the login keychain's private key (errSecInternalComponent),
        # so sign inside the desktop session. First use asks to let codesign use the key:
        # Always Allow makes that one-time.
        st=/tmp/screenpipe-sign.status; rm -f \"\$st\"
        printf '%s\n' '#!/bin/zsh' \"codesign --force --deep --sign '\$id' --entitlements '\$PWD/\$ent' '\$staged'\" \"echo \\\$? > '\$st'\" 'exit' > /tmp/screenpipe-sign.command
        chmod 700 /tmp/screenpipe-sign.command
        open -a Terminal /tmp/screenpipe-sign.command
        echo 'signing in the desktop session (allow codesign to use the key if macOS asks)...'
        for i in {1..90}; do [ -s \"\$st\" ] && break; sleep 2; done
        [ \"\$(cat \"\$st\" 2>/dev/null)\" = 0 ] || { echo 'error: signing with the certificate failed or timed out; nothing changed, screenpipe still running' >&2; exit 1; }
      fi
      codesign --verify --deep --strict \"\$staged\"
      if pgrep -f '$PROD_PAT' >/dev/null; then
        echo 'stopping screenpipe (recording pauses until the new build starts)'
        pkill -TERM -f '$PROD_PAT'
        for i in {1..30}; do pgrep -f '$PROD_PAT' >/dev/null || break; sleep 1; done
        if pgrep -f '$PROD_PAT' >/dev/null; then echo 'error: screenpipe did not exit; nothing changed' >&2; exit 1; fi
      fi
      # Keep the previous copy outside /Applications until the new one is up; on any failure,
      # put it back and reopen it so the recorder does not stay down.
      prev=/tmp/screenpipe-previous.app
      rm -rf \"\$prev\"
      [ -d /Applications/screenpipe.app ] && mv /Applications/screenpipe.app \"\$prev\"
      mv \"\$staged\" /Applications/screenpipe.app
      open /Applications/screenpipe.app
      for i in {1..60}; do curl -s http://127.0.0.1:3030/health | grep -q '\"status\":\"healthy\"' && break; sleep 2; done
      if ! curl -s http://127.0.0.1:3030/health | grep -q '\"status\":\"healthy\"'; then
        echo 'error: new build not healthy after 2 minutes (macOS may be asking for permissions)' >&2
        if [ -d \"\$prev\" ] && ! pgrep -f '$PROD_PAT' >/dev/null; then
          rm -rf /Applications/screenpipe.app && mv \"\$prev\" /Applications/screenpipe.app && open /Applications/screenpipe.app
          echo 'restored and reopened the previous build' >&2
        else
          echo \"new build left running; previous copy kept at \$prev\" >&2
        fi
        exit 1
      fi
      echo \"installed \$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' /Applications/screenpipe.app/Contents/Info.plist), signed with: \$id\"
      rm -rf \"\$prev\"
    "
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
    sed -n -e '/^# Edit on the/s/^# //p' -e '/^#   personal\/mac\.sh/s/^# //p' "$0"; exit 1
    ;;
esac
