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

### Updates, and installing a build on the Mac

Personal builds never auto-update: the updater is disabled in Rust whatever the
remote policy says (log line "updates disabled (personal build)"). To update,
rebuild from `myScreenpipe`.

Build on the Mac, then install from the Windows VM:

```bash
# Mac, in ~/Workspace/myScreenpipe
git pull --ff-only origin myScreenpipe
cd apps/screenpipe-app-tauri
export NEXT_PUBLIC_SCREENPIPE_PERSONAL_BUILD=1
bun install --frozen-lockfile
bun tauri build
```

```bash
# Windows VM
personal/mac.sh install
```

`install` stops the running app with SIGTERM, keeps the old copy in
`/tmp/screenpipe-previous.app`, copies the new bundle into `/Applications`,
signs it, opens it, and waits up to two minutes for `/health`. It deletes the
old copy only once the new one is healthy.

Don't just copy the app and open it. **Quit screenpipe** only stops recording
and keeps the app in the tray, and that running copy refuses to hand over to a
build signed with a different identity ("Screenpipe couldn't switch versions …
code requirement"). An ad hoc signature (`--sign -`) is different on every
build, and macOS also asks for Screen Recording, Accessibility and Microphone
again each time.

#### One-time: a stable signing certificate

With a self-signed code-signing certificate every build has the same identity,
so the handoff works and macOS keeps your permissions across rebuilds. No Apple
developer account is needed. On the Mac:

1. Open **Keychain Access**, then **Keychain Access > Certificate Assistant >
   Create a Certificate…**
2. Name: `screenpipe personal` (exactly; `install` looks for this name).
   Identity Type: **Self-Signed Root**. Certificate Type: **Code Signing**.
   Click **Create**, then **Done**. It goes into your login keychain.
3. Trust it for code signing: in Keychain Access, double-click
   **screenpipe personal** (login keychain, My Certificates), expand
   **Trust**, set **Code Signing** to **Always Trust**, close the window and
   enter your password. Certificate Assistant does not do this for you.
4. Check it: `security find-identity -v -p codesigning` lists
   `"screenpipe personal"`. Without step 3 it only appears without `-v`,
   marked `CSSMERR_TP_NOT_TRUSTED`.

`install` signs with it automatically (override with `SIGN_IDENTITY=...`) and
falls back to ad hoc, with a warning, when it isn't there. The first build
signed with it still asks for permissions once, because the identity changed.
After that they stay.

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
bun run test:tauri personal::   # native unit tests (queued, debug-dev)
bun run test                               # frontend (vitest + bun tests)
```

## VM context helper

When the Mac records a Parallels Windows VM, every VM frame is just "Parallels
Desktop". A small helper inside the VM reports the VM's foreground app name and
window title to the Mac, which stores them as the frame's app and window labels.
The host's own values still drive every admission decision (ignore list,
incognito, pause); the guest labels only change what is stored and what the live
hot cache shows. Labels fall back to "Parallels Desktop" when they are older
than 5 s, while the VM session is locked, or while the VM's foreground cannot be
identified (for example a UAC prompt). After the Mac wakes they fall back only
until the helper's next heartbeat (at most 2 s).

Requirements:

- Mac API auth must be ON. `POST /context/external` returns 403 otherwise.
- `ssh mac` must work from the VM with key auth and no prompts (the tunnel runs
  with `BatchMode=yes`).
- The VM's own screenpipe should be stopped while you rely on this.

Install, inside the VM, as the user who is logged in:

1. Get the key on the Mac: `screenpipe auth token`.
2. Save it, alone on one line, to `%APPDATA%\vm-context-helper\api-key`.
3. From `tools/vm-context-helper`, publish and register the logon task:

```powershell
dotnet publish -c Release -o $env:LOCALAPPDATA\vm-context-helper\bin
.\install-task.ps1
```

The task `VmContextHelper` runs non-elevated at logon with `RestartCount 3` (one
minute apart). After 3 crashes it stays down until the next logon. Check what the
helper sees with `VmContextHelper.exe --once`.

The helper opens `ssh -N -L 127.0.0.1:3031:127.0.0.1:3030 mac` itself (retry
backoff 1 s to 60 s) and sends only when that ssh child owns port 3031. Log:
`%LOCALAPPDATA%\vm-context-helper\helper.log` (rotates at 1 MB). It fails
closed, logging one line, and sends nothing when:

- another process owns port 3031,
- the key file is missing, or
- the Job Object that kills ssh with the helper cannot be set up, or session
  lock notifications cannot be registered (the helper exits).

If the VM clock steps backwards and the helper then restarts, the Mac silently
drops every post as stale (HTTP 204) until the Mac screenpipe restarts; restart
it to recover.

Privacy: the VM sends only the foreground app name and window title. Window
titles are not written to the helper log (app names are). The Mac's ignore list
and incognito check also apply to the guest app.

Checks: `dotnet test tools/vm-context-helper.Tests` (VM, 10 tests);
`cargo test -p screenpipe-engine --lib external_context` (Mac).

## Semantic memory search

Search your history by meaning, so "bill is past due" finds the text "invoice
overdue". It is **off by default**. Capture is unaffected either way.

Turn it on with any of these (the saved choice is kept unless a flag or the
environment variable overrides it; CLI flags win):

- Settings > Screen > System > Semantic search (a switch).
- `screenpipe record --enable-semantic-search` (or `=false` to force it off).
- Environment variable `SCREENPIPE_ENABLE_SEMANTIC_SEARCH=true|false`.

It takes effect on the next server restart (Settings lists it as a
restart-required setting).

**Model.** all-MiniLM-L6-v2 (ONNX) from the Hugging Face repo
`Xenova/all-MiniLM-L6-v2` at a pinned revision, so every install shares one
vector space. The first run downloads about 90 MB into
`<OS cache dir>/screenpipe/models`; after that it loads from disk. It runs on
the CPU on this device only (one ONNX thread); nothing is sent anywhere except
that download.

**What is embedded.** Screen text (`frames.full_text`) and audio transcripts.
Identical texts are embedded once. A text is split into at most 8 chunks of
about 256 tokens, and anything past the eighth chunk is not embedded. A
background worker does the work: new rows first, newest first, then it backfills
older history. Each batch takes the shared background-CPU lane with an adaptive
slice (at most 32 rows) and a 50 ms to 60 s cooldown, and it pauses while
background work is suspended. If the model cannot load, the worker logs one line
and stops.

**Privacy.**

- With text privacy release (redaction) on, only released text is embedded, from
  the released (redacted) copy.
- When the release setting or its policy changes (the `receipt_key` changes), all
  vectors are deleted and rebuilt.
- Deleting a frame or transcript deletes its vectors (database triggers).
- A transcript rewritten by overlap cleanup loses its vector at once (database
  trigger) and the worker embeds it again from the new text. Rewriting it with
  the same text changes nothing.
- The route re-reads every hit through the release gate and drops a vector hit
  whose current text no longer hashes to the text it was embedded from, and any
  row that is gone or unreleased.
- Pipes with data restrictions or the privacy filter get 403.
- The history-access cutoff (for example 24 h on the free plan) applies.

### `GET /search/semantic`

Hybrid: nearest vectors and keyword (FTS) matches are fused by reciprocal rank.

| Param | Meaning |
| --- | --- |
| `q` | Required, 1 to 1000 characters. |
| `limit` | Default 20, max 100. |
| `content_type` | `frame`, `audio` or `all` (default). |
| `start_time`, `end_time` | Optional RFC 3339 timestamps. |

Response: `{"data": [...]}`; each item has `source_type` (`frame` or `audio`),
`id`, `timestamp`, `text` (first 300 characters), `app_name`, `window_name`,
`device`, `speaker`, `score` and `matched_by` (`vector`, `keyword` or `both`).

Errors: 503 when semantic search is off (including right after turning the
setting off, once the server restarts), the worker has stopped, or the model is
not loaded yet; 409 in
search-only mode (Screenpipe quit, serving saved history); 403 for restricted
pipes; 400 for a bad `q`.

```bash
curl -G "http://localhost:3030/search/semantic" \
  -H "Authorization: Bearer $(screenpipe auth token)" \
  --data-urlencode "q=invoice overdue payment" --data-urlencode "limit=5"
```

Eval (needs the real model; downloads it once). On a 30-frame, 4-transcript
paraphrase fixture, hybrid recall@5 was 11/11 against 0/11 for keyword only.
That gap is partly the fixture's design: the keyword side requires every query
word to match. Run it from the Mac worktree:

```bash
cargo test -p screenpipe-engine --lib recall_eval -- --ignored --nocapture
```

### Known limits

- The keyword side scores only the newest 5000 matches (`KEYWORD_SCAN`), so older
  keyword matches drop out; the vector side can still find them.
- With release off, an edited frame is protected only by the route's hash
  check and is never re-embedded (a frames trigger would fire when sealing
  moves `full_text` into payloads and re-embed all history). Edited transcripts
  are re-embedded (see Privacy).
- A time range, the history cutoff and `content_type` apply after the global
  nearest-neighbour read, so with any of them set it reads 10 chunks per
  requested result (at most 1000) instead of 4 (at most 200). A narrow range whose rows all rank below that can still miss
  vector hits; the keyword side still finds them.
- Turning the setting off stops the worker but keeps the vectors on disk.
  Deleting a frame or transcript still deletes its vectors.
- An external `sqlite3` session that deletes frames or transcripts, or edits a
  transcript's text, fails with `no such module: vec0`: the embedding triggers
  touch the vector table, and
  sqlite-vec is loaded only inside Screenpipe.
- Only the first chunk (about 254 tokens) of the query is embedded.
- Texts under 4 words (`MIN_EMBED_WORDS`, e.g. a "Every turn." transcript) get
  no vectors, because a small model ranks them close to every query; they stay
  findable by keyword only.
- Any `receipt_key` bump deletes and rebuilds every vector, which takes a while
  on a long history.

## Change log: upstream files touched

Keep this list complete so upstream merges stay easy to review.

| File | Change |
| --- | --- |
| `apps/screenpipe-app-tauri/src-tauri/src/main.rs` | `mod personal;` plus `personal::apply_process_env()` right after `dev_isolation::apply()` (+3 lines) |
| `apps/screenpipe-app-tauri/src-tauri/src/store.rs` | `init_store`: call `personal::apply_new_profile_defaults(&mut store, is_new_store)` after the settings migrations, just before the save (+7 lines) |
| `apps/screenpipe-app-tauri/app/providers.tsx` | skip `posthog.init` when the flag is set (+5/-1 lines) |
| `crates/screenpipe-engine/src/event_driven_capture.rs` | capture hook: guest labels override stored and hot-cache app/window only; host values still drive admission |
| `crates/screenpipe-engine/src/server.rs` | register `POST /context/external` and `GET /search/semantic` |
| `crates/screenpipe-engine/src/routes/mod.rs` | `pub mod external_context;`, `pub mod semantic_search;` |
| `crates/screenpipe-engine/src/lib.rs` | `pub mod external_context;`, `pub mod embedding_worker;` |
| `crates/screenpipe-engine/src/sleep_monitor.rs` | clear the external context on wake |
| `crates/screenpipe-capture/src/paired_capture.rs` | optional stored labels for the frame's app/window |
| `packages/sdk/recorder-core/src/platform/recorder.rs` | `stored_labels: None` for the new field |
| `crates/screenpipe-engine/src/routes/search.rs` | `apply_search_history_access` made `pub(crate)`; `/search/semantic` joins the storage-snapshot read paths |
| `crates/screenpipe-engine/src/cli/mod.rs` | `--enable-semantic-search` / `SCREENPIPE_ENABLE_SEMANTIC_SEARCH` flag, override tracking, tests |
| `crates/screenpipe-engine/src/recording_config.rs` | `enable_semantic_search` field and tests |
| `crates/screenpipe-engine/src/bin/screenpipe-engine.rs` | spawn the embedding worker when enabled; abort it at shutdown (+11 lines) |
| `crates/screenpipe-engine/Cargo.toml` | `screenpipe-embed` dependency |
| `crates/screenpipe-config/src/recording.rs` | `enable_semantic_search` setting (`enableSemanticSearch`, default off) and test |
| `crates/screenpipe-db/src/db/mod.rs` | `mod embeddings;` and its re-exports |
| `crates/screenpipe-db/src/lib.rs` | re-export the embedding types and `EMBEDDING_DIMS` |
| `apps/screenpipe-app-tauri/src-tauri/src/server_core.rs` | spawn the embedding worker when enabled (+6 lines) |
| `apps/screenpipe-app-tauri/components/settings/recording-settings.tsx` | Semantic search switch, search index entry, restart-required entry (+23 lines) |
| `apps/screenpipe-app-tauri/lib/utils/tauri.ts` | `enableSemanticSearch` in the generated settings type (+6 lines) |
| `Cargo.lock`, `apps/screenpipe-app-tauri/src-tauri/Cargo.lock` | new dependencies of `screenpipe-embed` |
| `docs/COVERAGE.md`, `docs/coverage/CORE.md`, `docs/coverage/core-engine-map.json` | coverage entries for the embedding files |

Files added by this fork (not upstream): `PERSONAL.md`,
`apps/screenpipe-app-tauri/src-tauri/src/personal.rs`, `personal/mac.sh`,
`crates/screenpipe-engine/src/external_context.rs`,
`crates/screenpipe-engine/src/routes/external_context.rs`, `tools/.gitignore`,
`tools/vm-context-helper/`, `tools/vm-context-helper.Tests/`,
`crates/screenpipe-embed/`, `crates/screenpipe-engine/src/embedding_worker.rs`,
`crates/screenpipe-engine/src/routes/semantic_search.rs`,
`crates/screenpipe-db/src/db/embeddings.rs`,
`crates/screenpipe-db/src/migrations/20261126120000_text_embeddings.sql`.
