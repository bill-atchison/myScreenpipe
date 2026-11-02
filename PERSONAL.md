# PERSONAL.md

Personal build profile for this fork (`myScreenpipe`). Read this before building
the app for everyday use.

## License basis

LICENSE.md section 2 allows personal, non-commercial use of the source and of
binaries you build from it. This profile is for that use only. It does not cover
commercial use (section 4), and it does not apply to official prebuilt builds
(section 3).

## What the personal build flag does

Set `NEXT_PUBLIC_SCREENPIPE_PERSONAL_BUILD=1` when you build. Rust reads it at
compile time with `option_env!`, and Next.js inlines it into the frontend. That
one variable changes the following:

| Area | Effect |
| --- | --- |
| Telemetry | The process starts with `SCREENPIPE_DISABLE_TELEMETRY=1`, unless you set that variable yourself. That turns off Rust Sentry, app and engine PostHog, and resource telemetry, and the frontend picks it up too. Child processes (engine, Pi) inherit it. |
| Frontend PostHog | `posthog.init` never runs, so there are no capture or feature-flag requests. |
| New settings store only | Analytics off, remote support logs off. AI presets: **ChatGPT / Codex** (`openai-chatgpt`, `gpt-5.6-terra`, the default) and **Ollama** (`native-ollama`, `http://localhost:11434/v1`, model left for you to pick). No `screenpipe-cloud` preset is seeded; you can still add one yourself. |
| Cloud sync and archive | Nothing to change. Upstream already turns them off at every launch: the log lines are `cloud sync: disabled because the Storage sync backend was removed` and `cloud archive: disabled because the Storage archive backend was removed`. |

## What it does not do

- It never rewrites an existing settings store. The defaults apply only when the
  app creates a new store, and you can change all of them in Settings afterwards.
- It does not change sign-in, plan, entitlement, trial, history (24h on the free
  plan) or pipe-count checks, and it does not force rollout flags. You sign in
  once with a free Screenpipe account; local recording, search, timeline and
  chat with ChatGPT or Ollama then work under the free plan.
- It does not change how the app calls hosted services. Without the flag, the
  build behaves exactly as upstream.

## First run

1. Sign in with a free Screenpipe account.
2. Connect ChatGPT under Settings > AI. This needs a ChatGPT Plus or Pro
   subscription and an OAuth login in your browser. Until you connect, the
   Codex preset asks you to sign in.
3. If you want Ollama instead, install Ollama, `ollama pull <model>`, then pick
   that model on the Ollama preset and make it the default.

## Release build for everyday use, debug build for development only

Use a **release** build day to day: it runs the real plan checks, uses
`~/.screenpipe` and port 3030, and sets up the AI tools normally. Upstream debug
builds skip plan verification (`cfg!(debug_assertions)` in
`src-tauri/src/recording.rs`), keep their data in `~/.screenpipe-dev`, and use
port 3130, so use them only for development.

A release build shares `~/.screenpipe`, port 3030 and the single-instance
identity with an installed screenpipe. Run one at a time: quit the installed app
first.

### Release build: open item

AGENTS.md lets agents build `src-tauri` only through the queued scripts. None of
those scripts builds the consumer app with the release profile:

- `build:tauri:dev`, `dev:tauri`, `test:tauri` and `tauri:build`
  (`scripts/build_macos.sh`) all use `debug-dev`.
- `bundle:enterprise-test-installer` and `build:persistence-installer` are
  release builds, but they compile `--features enterprise-build` for
  `x86_64-pc-windows-msvc`, which is not this profile.
- The human path in CONTRIBUTING.md ("build the Tauri installer") is a raw
  `bun tauri build`. An agent may not run it, and it bypasses the queue.

Until the Owner either runs that human path or approves a queued release mode, a
personal release build is **not yet documented as an agent-runnable step**. The
command a person would run (Windows, PowerShell, from
`apps/screenpipe-app-tauri`) is upstream's:

```powershell
$env:NEXT_PUBLIC_SCREENPIPE_PERSONAL_BUILD = "1"
bun install --frozen-lockfile
bun tauri build      # CONTRIBUTING.md "build the Tauri installer"; installer in src-tauri\target\release\bundle\nsis\
```

### Debug build (development)

Windows (PowerShell, from `apps/screenpipe-app-tauri`; needs the machine-wide
sccache and native build queue):

```powershell
$env:NEXT_PUBLIC_SCREENPIPE_PERSONAL_BUILD = "1"
bun install --frozen-lockfile
bun run build:tauri:dev   # binary: src-tauri\target\debug-dev\screenpipe-app.exe
```

Mac, driven from the Windows VM over SSH with `personal/mac.sh`. It builds with
`build:tauri:dev`, so it is a development build:

```bash
personal/mac.sh build [branch]   # fast-forward the Mac clone, then build:tauri:dev
personal/mac.sh run              # port 3130, ~/.screenpipe-dev
personal/mac.sh logs -f
personal/mac.sh stop
```

To build the flagged version, export `NEXT_PUBLIC_SCREENPIPE_PERSONAL_BUILD=1`
in the Mac login shell; `mac.sh` does not set it.

If you change only the flag, check that the binary was actually rebuilt. Cargo
tracks `option_env!`, but confirm that sccache did not reuse a stale result.

## Tests

From `apps/screenpipe-app-tauri`:

```bash
bun run test:tauri personal:: -- store::   # native unit tests (queued, debug-dev)
bun run test                               # frontend (vitest + bun tests)
```

## Change log: upstream files touched

Keep this list complete so upstream merges stay easy to review.

| File | Change |
| --- | --- |
| `apps/screenpipe-app-tauri/src-tauri/src/main.rs` | `mod personal;` plus `personal::apply_process_env()` right after `dev_isolation::apply()` (+3 lines) |
| `apps/screenpipe-app-tauri/src-tauri/src/store.rs` | `init_store`: call `personal::apply_new_profile_defaults(&mut store, is_new_store)` (+5 lines) |
| `apps/screenpipe-app-tauri/app/providers.tsx` | skip `posthog.init` when the flag is set (+5/-1 lines) |

Files added by this fork (not upstream): `PERSONAL.md`,
`apps/screenpipe-app-tauri/src-tauri/src/personal.rs`, `personal/mac.sh`.
