// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

//! Opt-in code snippet capture (`enableSnippetCapture`). Every minute the
//! worker reads clipboard copies and cuts recorded in `ui_events` since its
//! high water mark, keeps the ones that look like code, and saves them to
//! `snippets` (served by `/snippets`). Progress lives in a sidecar
//! `snippets.state.json`, not in the database. The first run (no state file)
//! starts at the newest event, so only copies made after opting in are
//! saved. Capture is never touched: the clipboard rows are already durable
//! when this reads them.
//!
//! Privacy: `ui_events` is the `input` surface of the text privacy release.
//! While release is on, a copy is saved only after its input receipt is
//! certified, and then with the released text. While async PII redaction is
//! on, a copy also waits until the redaction worker has processed it. The
//! scan stops at the first copy still waiting, so nothing is skipped; a copy
//! still waiting after a day is passed over and never saved. Text that looks
//! like a secret (keys, tokens, passwords) is never saved. Snippets outlive
//! clipboard retention; a user's time-range delete removes the snippets
//! copied in that range. Snippets saved before release was switched on are
//! not revisited, but the API hides them until their event is released.
//!
//! With snippet capture on, capture keeps up to
//! `SNIPPET_CLIPBOARD_CONTENT_MAX_BYTES` (20,000) of a copy or cut (pastes
//! stay at 1,000) and marks a cut copy with a trailing "..."; those are not
//! saved, so a snippet is never a fragment.

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use screenpipe_a11y::events::SNIPPET_CLIPBOARD_CONTENT_MAX_BYTES;
use screenpipe_db::{ClipboardCopy, DatabaseManager, NewSnippet};
use screenpipe_resource::ResourceGovernor;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const STATE_FILE: &str = "snippets.state.json";
const INTERVAL: Duration = Duration::from_secs(60);
/// Clipboard rows read per batch. They are human copy events, so a fixed
/// batch stays small; time-sized batches are not needed.
const BATCH: i64 = 500;
/// Snippets whose clipboard event may still be linked to a frame.
const LINK_WINDOW: chrono::Duration = chrono::Duration::hours(1);
const LINK_BATCH: i64 = 500;
const MIN_COOLDOWN: Duration = Duration::from_millis(50);
const MAX_COOLDOWN: Duration = Duration::from_secs(10);
/// Longer text is not a snippet.
pub const MAX_SNIPPET_CHARS: usize = 20_000;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub high_water: i64,
}

/// `None`: no (readable) state file, i.e. the first run since opting in.
pub fn read_state(path: &Path) -> Option<State> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            info!(
                "snippets: cannot read {} ({e}); treating as a first run",
                path.display()
            );
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(state) => Some(state),
        Err(e) => {
            info!(
                "snippets: cannot parse {} ({e}); treating as a first run",
                path.display()
            );
            None
        }
    }
}

/// Snippet capture is off: forget progress, so the next opt-in is a first
/// run and copies made while it was off are never saved.
pub fn forget_progress(data_dir: &Path) -> Result<()> {
    let path = data_dir.join(STATE_FILE);
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

fn write_state(path: &Path, state: &State) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(state)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

// ---------------------------------------------------------------- detection

const KEYWORDS: &[&str] = &[
    "fn",
    "pub",
    "let",
    "const",
    "var",
    "def",
    "class",
    "import",
    "from",
    "return",
    "use",
    "struct",
    "impl",
    "enum",
    "interface",
    "function",
    "async",
    "await",
    "if",
    "elif",
    "else",
    "for",
    "while",
    "match",
    "package",
    "func",
    "public",
    "private",
    "protected",
    "static",
    "void",
    "int",
    "try",
    "catch",
    "except",
    "with",
    "#include",
    "#define",
    "using",
    "namespace",
    "export",
    "module",
    "mod",
    "type",
    "switch",
    "case",
    "new",
    "throw",
    "raise",
    "yield",
];

/// Commands whose name alone, with an argument, marks a shell line.
const COMMANDS: &[&str] = &[
    "git",
    "cargo",
    "npm",
    "npx",
    "bun",
    "bunx",
    "yarn",
    "pnpm",
    "docker",
    "kubectl",
    "sudo",
    "brew",
    "pip",
    "pip3",
    "gh",
    "rustup",
    "terraform",
    "psql",
    "sqlite3",
    "ssh",
    "scp",
    "curl",
    "wget",
    "make",
    "go",
    "az",
    "aws",
    "gcloud",
    "dotnet",
    "msbuild",
    "powershell",
    "pwsh",
    "chmod",
    "chown",
    "mkdir",
    "rm",
    "mv",
    "cp",
    "ls",
    "cd",
    "grep",
    "rg",
    "sed",
    "awk",
    "tar",
    "python",
    "python3",
    "node",
    "deno",
    "export",
    "echo",
    "cat",
    "tail",
    "head",
    "find",
    "ps",
    "kill",
    "systemctl",
    "launchctl",
    "defaults",
    "apt",
    "apt-get",
    "yum",
    "choco",
    "winget",
];

const OPERATORS: &[&str] = &[
    "=>", "->", "::", ":=", "==", "!=", "&&", "||", "+=", "-=", "</", "/>", "<<", ">>", "|>",
];

/// A credential written out: a quoted literal assigned to a secret-named key,
/// an env-style secret assignment, or a bearer token.
static SECRET_ASSIGNMENTS: Lazy<[Regex; 4]> = Lazy::new(|| {
    [
        // Quoted after `:` or `=`, or a bare token after `=` that is not an
        // expression or type (no `(`, `.`, `[`, `:`).
        r#"(?i)(pass(word|wd)?|pwd|secret|api[_-]?key|access[_-]?key|auth[_-]?token|access[_-]?token)\w*["']?\s*(?:[:=]\s*["'][^"'\s]{6,}["']|=\s*[^\s"'(\[.:;,]{6,}(?:[\s;,]|$))"#,
        r"(?m)^\s*(export\s+)?[A-Z0-9_]*(PASSWORD|PASSWD|SECRET|API_KEY|TOKEN|PRIVATE_KEY)[A-Z0-9_]*\s*=\s*\S{6,}",
        r"(?i)\bbearer\s+[A-Za-z0-9._~+/-]{16,}",
        // Credentials in a URL: scheme://user:pass@host.
        r"[A-Za-z][A-Za-z0-9+.-]*://[^/\s:@]+:[^/\s@]+@",
    ]
    .map(|p| Regex::new(p).expect("secret pattern compiles"))
});

static SQL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?is)^\s*(select\s.+\sfrom\s|with\s+\w+\s+as\s*\(|insert\s+into\s|update\s+\w+\s+set\s|delete\s+from\s|create\s+(unique\s+)?(table|index|view|trigger)\s|alter\s+table\s|drop\s+(table|index|view)\s)",
    )
    .expect("sql pattern compiles")
});

static CALL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[A-Za-z_$][\w$]*(\.[A-Za-z_$][\w$]*)*\([^()]*\)").unwrap());

static ASSIGNMENT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z_$][\w$.\[\]]*\s*(=|\+=|-=|:=)\s*\S").unwrap());

static WHOLE_CALL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z_$][\w$]*(\.[A-Za-z_$][\w$]*)*\(.*\);?$").unwrap());

/// A token that looks generated: long, with letters and digits alternating
/// often (hex digests, base64 keys, UUIDs), or carrying a known key prefix.
// ponytail: a literal hash or UUID inside real code also trips this, so that
// snippet is not saved; loosen it if those turn out to matter.
fn secret_token(token: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "pk_live_",
        "ghp_",
        "gho_",
        "ghs_",
        "ghu_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xapp-",
        "AKIA",
        "ASIA",
        "AIza",
        "ya29.",
        "eyJ",
        "npm_",
        "pypi-",
    ];
    if token.len() >= 16 && PREFIXES.iter().any(|p| token.starts_with(p)) {
        return true;
    }
    if token.len() < 20 || token.contains('/') {
        return false;
    }
    let mut changes = 0;
    let mut prev: Option<bool> = None;
    for c in token.chars() {
        let digit = if c.is_ascii_digit() {
            Some(true)
        } else if c.is_ascii_alphabetic() {
            Some(false)
        } else {
            None
        };
        if let (Some(p), Some(d)) = (prev, digit) {
            if p != d {
                changes += 1;
            }
        }
        prev = digit;
    }
    changes >= 4
}

fn contains_secret(text: &str) -> bool {
    text.contains("PRIVATE KEY-----")
        || SECRET_ASSIGNMENTS.iter().any(|r| r.is_match(text))
        || text
            .split(|c: char| c.is_whitespace() || "=:\"'`,;()[]{}<>".contains(c))
            .any(secret_token)
}

fn shell_line(line: &str) -> bool {
    let line = line.trim();
    let line = line
        .strip_prefix("$ ")
        .or_else(|| line.strip_prefix("PS> "))
        .or_else(|| line.strip_prefix("% "))
        .map(str::trim)
        .unwrap_or(line);
    let mut words = line.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    let args: Vec<&str> = words.collect();
    COMMANDS.contains(&first)
        && !args.is_empty()
        && args
            .iter()
            .any(|a| a.starts_with('-') || a.contains('/') || a.contains('.') || "|>&".contains(*a))
        || (matches!(
            first,
            "git" | "cargo" | "npm" | "bun" | "docker" | "kubectl" | "sudo" | "brew" | "gh"
        ) && !args.is_empty()
            && args.iter().all(|a| !a.ends_with(['.', ',', '?', '!'])))
}

fn starts_with_keyword(line: &str) -> bool {
    let first = line
        .split(|c: char| c.is_whitespace() || c == '(' || c == '<')
        .next()
        .unwrap_or("");
    KEYWORDS.contains(&first) && line.len() > first.len()
}

/// One line on its own terms. Prose rarely ends lines with these characters
/// or uses these operators.
fn code_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let indented_python_block = t.ends_with(':') && starts_with_keyword(t);
    t.ends_with([';', '{', '}', '(', '[', ']'])
        || (t.ends_with(')') && CALL.is_match(t))
        || indented_python_block
        || t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with("#!")
        || t.starts_with("--")
        || OPERATORS.iter().any(|op| t.contains(op))
        || starts_with_keyword(t)
        || ASSIGNMENT.is_match(t)
        || shell_line(t)
        || SQL.is_match(t)
}

/// Copied text that is code: not prose, not a single token (URLs, emails,
/// keys, passwords) and not anything that looks like a secret.
pub fn is_code(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > MAX_SNIPPET_CHARS || contains_secret(text) {
        return false;
    }
    if !text.contains(char::is_whitespace) {
        // The only single tokens kept are whole calls such as `run();`.
        return text.len() >= 6 && WHOLE_CALL.is_match(text);
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if let [line] = lines.as_slice() {
        let t = line.trim();
        // One line needs a strong signal; ending in `;` or `)` alone is not.
        // "Select the best one from the list" is prose; SQL punctuation or
        // an upper-case keyword says otherwise.
        let sql = SQL.is_match(t)
            && (t.contains(['*', ';', '=', '(', ','])
                || t.split_whitespace()
                    .next()
                    .is_some_and(|w| w.len() > 1 && w.chars().all(|c| c.is_ascii_uppercase())));
        return shell_line(t)
            || sql
            || (t.ends_with([';', '{', '}'])
                && (starts_with_keyword(t)
                    || ASSIGNMENT.is_match(t)
                    || CALL.is_match(t)
                    || OPERATORS.iter().any(|op| t.contains(op))))
            || (starts_with_keyword(t) && (t.contains('(') || t.contains('=')));
    }
    let code = lines.iter().filter(|l| code_line(l)).count();
    let indented = lines
        .iter()
        .skip(1)
        .filter(|l| l.starts_with([' ', '\t']) && !l.trim_start().starts_with(['-', '*', '•']))
        .count();
    code >= 2 && (code * 2 >= lines.len() || (code + indented) * 3 >= lines.len() * 2)
}

fn extension_language(name: &str) -> Option<&'static str> {
    let (_, ext) = name.trim().rsplit_once('.')?;
    Some(match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "cs" => "csharp",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "sh" | "bash" | "zsh" => "shell",
        "ps1" | "psm1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "md" => "markdown",
        "clw" | "inc" | "equ" => "clarion",
        "xml" | "xaml" => "xml",
        _ => return None,
    })
}

fn content_language(text: &str) -> Option<&'static str> {
    let t = text.trim_start();
    let has = |s: &str| t.contains(s);
    let line_starts = |s: &str| t.lines().any(|l| l.trim_start().starts_with(s));
    if let Some(first) = t.lines().next().filter(|l| l.starts_with("#!")) {
        return Some(if first.contains("python") {
            "python"
        } else if first.contains("node") {
            "javascript"
        } else {
            "shell"
        });
    }
    if t.starts_with("<?php") {
        return Some("php");
    }
    if t.starts_with('<') && has(">") {
        return Some("html");
    }
    if t.starts_with(['{', '[']) && serde_json::from_str::<serde_json::Value>(t).is_ok() {
        return Some("json");
    }
    if SQL.is_match(t) {
        return Some("sql");
    }
    if line_starts("#include") {
        return Some(if has("std::") || has("namespace ") || has("class ") {
            "cpp"
        } else {
            "c"
        });
    }
    if has("fn ")
        && (has("let ") || has("->") || has("::") || has("pub ") || has("impl ") || has("!("))
    {
        return Some("rust");
    }
    if has("func ") && (line_starts("package ") || has(":=")) {
        return Some("go");
    }
    if line_starts("using System") || (line_starts("namespace ") && has("{")) {
        return Some("csharp");
    }
    if line_starts("import java.") || has("System.out.") {
        return Some("java");
    }
    let python = line_starts("def ")
        || line_starts("elif ")
        || t.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("from ") && l.contains(" import ")
        })
        || (line_starts("import ") && !has(" from '") && !has(" from \"") && !has(";"));
    if python && !has("{") {
        return Some("python");
    }
    if has("interface ") || has(": string") || has(": number") || has(": boolean") {
        return Some("typescript");
    }
    if line_starts("const ")
        || line_starts("let ")
        || has("function")
        || has("=>")
        || has("console.")
    {
        return Some("javascript");
    }
    if t.lines().next().is_some_and(shell_line) {
        return Some("shell");
    }
    None
}

/// The file the code came from decides first, then a file name in the window
/// title, then the code itself.
pub fn guess_language(
    text: &str,
    document_path: Option<&str>,
    window_title: Option<&str>,
) -> Option<String> {
    document_path
        .and_then(extension_language)
        .or_else(|| {
            window_title?
                .split(|c: char| c.is_whitespace() || "—–|·()[]".contains(c))
                .filter(|w| w.contains('.') && !w.ends_with('.'))
                .find_map(extension_language)
        })
        .or_else(|| content_language(text))
        .map(str::to_string)
}

// ------------------------------------------------------------------- worker

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassStats {
    pub scanned: u64,
    pub saved: u64,
    /// Copies passed over after waiting a day for privacy release/redaction.
    pub passed_over: u64,
    pub linked: u64,
}

/// Capture cuts copies at the snippet cap (on, since this worker runs only
/// with snippet capture) and marks them with a trailing "...". Shorter text
/// ending in "..." is real code and kept.
// ponytail: PII removal after the cut can shorten a cut copy below the
// threshold; it would then be saved as a fragment.
fn cut_by_capture(text: &str) -> bool {
    text.ends_with("...") && text.len() >= SNIPPET_CLIPBOARD_CONTENT_MAX_BYTES - 4
}

fn new_snippet(copy: &ClipboardCopy) -> NewSnippet {
    // Keep the first line's indentation; drop surrounding blank lines.
    let content = copy
        .content
        .trim_end()
        .trim_start_matches(['\r', '\n'])
        .to_string();
    NewSnippet {
        language: guess_language(
            &content,
            copy.document_path.as_deref(),
            copy.window_title.as_deref(),
        ),
        created_at: copy.created_at.clone(),
        content,
        app_name: copy.app_name.clone(),
        window_title: copy.window_title.clone(),
        browser_url: copy.browser_url.clone(),
        document_path: copy.document_path.clone(),
        frame_id: copy.frame_id,
        ui_event_id: copy.id,
        release_key: copy.release_key,
    }
}

/// Save the code among clipboard copies above the high water mark, then link
/// recent snippets to the frames their copies were linked to since.
pub async fn pass(
    db: &DatabaseManager,
    governor: &Arc<ResourceGovernor>,
    wait_for_redaction: bool,
    state_path: &Path,
) -> Result<(State, PassStats)> {
    let mut stats = PassStats::default();
    let max = db.max_ui_event_id().await?;
    let Some(mut state) = read_state(state_path) else {
        // First run since opting in: only copies made from now on are saved.
        let state = State { high_water: max };
        write_state(state_path, &state)?;
        info!(
            high_water = max,
            "snippets: first run, saving copies from now on"
        );
        return Ok((state, stats));
    };
    if max < state.high_water {
        // A replaced or reset database: ids restart.
        info!(
            max,
            high_water = state.high_water,
            "snippets: ui_event ids went backwards, saving copies from now on"
        );
        // Like a first run: a replaced database can hold copies from before
        // the user opted in, so nothing already in it is saved.
        state.high_water = max;
        write_state(state_path, &state)?;
    }
    loop {
        let permit = governor.acquire_background_cpu().await;
        let started = Instant::now();
        let copies = db
            .clipboard_copies_after(state.high_water, BATCH, wait_for_redaction)
            .await?;
        let mut next = state.high_water;
        let mut waiting = false;
        let mut new = Vec::new();
        for copy in &copies {
            if !copy.ready && !copy.stale {
                waiting = true;
                break;
            }
            next = copy.id;
            stats.scanned += 1;
            if !copy.ready {
                stats.passed_over += 1;
            } else if !cut_by_capture(&copy.content) && is_code(&copy.content) {
                new.push(new_snippet(copy));
            }
        }
        let (saved, hold) = db.insert_snippets(&new).await?;
        // Persisted after the insert: a crash in between re-reads the batch,
        // and the insert ignores text already saved.
        if next != state.high_water {
            state.high_water = next;
            write_state(state_path, &state)?;
        }
        stats.saved += saved;
        let worked = started.elapsed();
        let cooldown = permit.finish(worked, MIN_COOLDOWN, MAX_COOLDOWN).cooldown;
        debug!(
            high_water = state.high_water,
            read = copies.len(),
            saved,
            waiting,
            hold_ms = hold.as_millis(),
            "snippets: batch"
        );
        if waiting || copies.len() < BATCH as usize {
            break;
        }
        tokio::time::sleep(cooldown).await;
    }
    let since = (chrono::Utc::now() - LINK_WINDOW)
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    {
        let _permit = governor.acquire_background_cpu().await;
        stats.linked = db
            .link_snippet_frames(&since, LINK_BATCH, guess_language)
            .await?;
    }
    if stats.passed_over > 0 {
        warn!(
            passed_over = stats.passed_over,
            "snippets: gave up on clipboard copies still unreleased or unredacted after a day; \
             is PII redaction / privacy release processing ui_events?"
        );
    }
    if stats.scanned > 0 || stats.linked > 0 {
        info!(
            scanned = stats.scanned,
            saved = stats.saved,
            passed_over = stats.passed_over,
            linked = stats.linked,
            high_water = state.high_water,
            "snippets: pass done"
        );
    }
    Ok((state, stats))
}

/// Save snippets for the engine's lifetime; aborted on shutdown. Spawned
/// only while snippet capture is enabled.
pub fn spawn(
    db: Arc<DatabaseManager>,
    data_dir: PathBuf,
    wait_for_redaction: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let governor = ResourceGovernor::global();
        let state_path = data_dir.join(STATE_FILE);
        info!(wait_for_redaction, "snippets: worker started");
        loop {
            if let Err(e) = pass(&db, &governor, wait_for_redaction, &state_path).await {
                warn!("snippets: pass failed, retrying: {e:#}");
            }
            tokio::time::sleep(INTERVAL).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &[&str] = &[
        "fn main() {\n    println!(\"hi\");\n}",
        "def add(a, b):\n    return a + b",
        "SELECT id, name FROM users WHERE active = 1;",
        "select * from frames limit 10",
        "git commit -m \"fix: thing\"",
        "$ cargo test -p screenpipe-db --lib snippets",
        "const x = await fetch(url);",
        "if (a && b) {\n  run();\n}",
        "{\n  \"name\": \"screenpipe\",\n  \"private\": true\n}",
        "docker run --rm -it ubuntu bash",
        "let total = items.iter().map(|i| i.price).sum::<u64>();",
        "run();",
        "#include <stdio.h>\nint main(void) { return 0; }",
        // Secret-named code that holds no literal secret.
        "interface Login {\n  user: string;\n  password: string;\n}",
        "import os\napi_key = os.environ[\"X\"]",
        "const password = bcrypt.hash(pw);",
    ];

    const NOT_CODE: &[&str] = &[
        // Single tokens: a fake API key, a password, a URL, an email, a word.
        "sk-proj-4f9aB2cD8eF1gH7iJ3kL6mN0pQ5rS9tU",
        "Tr0ub4dor&3xyz!",
        "https://github.com/screenpipe/screenpipe/pull/123",
        "someone@example.com",
        "hello",
        "",
        // Prose.
        "Thanks, I'll take a look at this tomorrow morning.",
        "Can we meet at 3pm? I think the design (v2) works better.",
        "The quick brown fox jumps over the lazy dog.\nIt was a sunny day.\nWe went home.",
        "If you want, we can go for a walk later.",
        "Select the best option from the list",
        // Code that carries a secret.
        "export OPENAI_API_KEY=sk-proj-4f9aB2cD8eF1gH7iJ3kL6mN0pQ5rS9tU",
        "const client = new Client({\n  apiKey: \"a8f3k2j9d0s7l1q4w6e8r2t5\",\n});",
        "password = \"hunter2hunter2\";",
        "DB_PASSWORD=supersecretvalue\nDB_HOST=localhost",
        "password = hunter2hunter2\nlet user = admin;",
        "DATABASE_URL=postgres://user:p4ss@host/db\nPORT=5432",
        "const db = {\n  password: \"hunter2hunter2\",\n};",
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----",
        "curl -H \"Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc\" https://x.io",
    ];

    #[test]
    fn detects_code_and_rejects_prose_tokens_and_secrets() {
        for text in CODE {
            assert!(is_code(text), "should be code: {text:?}");
        }
        for text in NOT_CODE {
            assert!(!is_code(text), "should not be code: {text:?}");
        }
        assert!(!is_code(&"let x = 1;\n".repeat(MAX_SNIPPET_CHARS / 10)));
    }

    #[test]
    fn guesses_language_from_path_title_then_content() {
        let g = |t: &str, p: Option<&str>, w: Option<&str>| guess_language(t, p, w);
        assert_eq!(
            g("x = 1", Some("/src/main.rs"), None).as_deref(),
            Some("rust")
        );
        assert_eq!(
            g("x = 1", None, Some("snippet_worker.rs — screenpipe")).as_deref(),
            Some("rust")
        );
        assert_eq!(
            g("x = 1", None, Some("TAX.CLW")).as_deref(),
            Some("clarion")
        );
        // The file beats what the content alone suggests (javascript).
        assert_eq!(
            g("const x = 1;", Some("/web/app.ts"), None).as_deref(),
            Some("typescript")
        );
        assert_eq!(g(CODE[0], None, None).as_deref(), Some("rust"));
        assert_eq!(g(CODE[1], None, None).as_deref(), Some("python"));
        assert_eq!(g(CODE[2], None, None).as_deref(), Some("sql"));
        assert_eq!(g(CODE[4], None, None).as_deref(), Some("shell"));
        assert_eq!(g(CODE[6], None, None).as_deref(), Some("javascript"));
        assert_eq!(g(CODE[8], None, None).as_deref(), Some("json"));
        assert_eq!(g(CODE[12], None, None).as_deref(), Some("c"));
        assert_eq!(
            g("interface A { b: string }", None, None).as_deref(),
            Some("typescript")
        );
        assert_eq!(g("plain words here", None, Some("Slack")), None);
    }

    fn governor() -> Arc<ResourceGovernor> {
        ResourceGovernor::new(Default::default())
    }

    async fn db() -> DatabaseManager {
        let db = DatabaseManager::new("sqlite::memory:", Default::default())
            .await
            .unwrap();
        // 1: code; 2: prose; 3: a key; 4: the same code again; 5: a paste.
        db.execute_raw_sql_write(
            "INSERT INTO frames(id,timestamp,document_path) VALUES (1,'2026-11-28T10:00:00Z','/src/lib.rs'); \
             INSERT INTO ui_events(id,timestamp,event_type,modifiers,text_content,app_name,window_title,frame_id) VALUES \
             (1,'2026-11-28T10:00:00Z','clipboard',99,'\n    let a = b();\n\n','Code','lib.rs',1), \
             (2,'2026-11-28T10:00:01Z','clipboard',99,'see you tomorrow','Slack',NULL,NULL), \
             (3,'2026-11-28T10:00:02Z','clipboard',99,'ghp_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3','Chrome',NULL,NULL), \
             (4,'2026-11-28T10:00:03Z','clipboard',120,'    let a = b();','Code','lib.rs',NULL), \
             (5,'2026-11-28T10:00:04Z','clipboard',118,'fn pasted() {}','Code',NULL,NULL);",
        )
        .await
        .unwrap();
        db
    }

    async fn contents(db: &DatabaseManager) -> Vec<(String, Option<String>)> {
        db.list_snippets(&screenpipe_db::SnippetFilter {
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|s| (s.content, s.language))
        .collect()
    }

    #[tokio::test]
    async fn saves_only_code_once_and_resumes_from_the_high_water_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        let db = db().await;
        // Opted in before these copies were made.
        write_state(&path, &State { high_water: 0 }).unwrap();
        let (state, stats) = pass(&db, &governor(), false, &path).await.unwrap();
        assert_eq!(state.high_water, 4, "the paste is not read");
        assert_eq!((stats.scanned, stats.saved), (4, 1));
        assert_eq!(read_state(&path), Some(state));
        assert_eq!(
            contents(&db).await,
            [("    let a = b();".to_string(), Some("rust".to_string()))]
        );

        // Nothing new: nothing read.
        let (_, again) = pass(&db, &governor(), false, &path).await.unwrap();
        assert_eq!((again.scanned, again.saved), (0, 0));
        // 7: code cut at the cap by capture; 8: short code that ends in "...".
        let cut = screenpipe_a11y::events::cap_clipboard_text(
            &"let a = 1;\n".repeat(2_500),
            SNIPPET_CLIPBOARD_CONTENT_MAX_BYTES,
        );
        assert!(cut.ends_with("..."));
        db.execute_raw_sql_write(&format!(
            "INSERT INTO ui_events(id,timestamp,event_type,modifiers,text_content) VALUES \
             (6,'2026-11-28T10:00:05Z','clipboard',99,'SELECT 1 FROM t;'), \
             (7,'2026-11-28T10:00:06Z','clipboard',99,'{cut}'), \
             (8,'2026-11-28T10:00:07Z','clipboard',99,'fn spread() {{\n    call(...args)\n}}...')"
        ))
        .await
        .unwrap();
        let (state, more) = pass(&db, &governor(), false, &path).await.unwrap();
        assert_eq!(
            (state.high_water, more.scanned, more.saved),
            (8, 3, 2),
            "code cut short by capture is not saved; short code ending in ... is"
        );

        // A database replaced under the same data dir restarts ids: start at
        // its newest event and save none of the copies already in it.
        db.execute_raw_sql_write("DELETE FROM snippets")
            .await
            .unwrap();
        write_state(&path, &State { high_water: 1_000 }).unwrap();
        let (state, reset) = pass(&db, &governor(), false, &path).await.unwrap();
        assert_eq!((state.high_water, reset.saved), (8, 0));
        assert!(contents(&db).await.is_empty());
        // Only a copy made after the reset is saved.
        db.execute_raw_sql_write(
            "INSERT INTO ui_events(id,timestamp,event_type,modifiers,text_content) VALUES \
             (9,'2026-11-28T10:00:08Z','clipboard',99,'let after_reset = 1;')",
        )
        .await
        .unwrap();
        let (state, after) = pass(&db, &governor(), false, &path).await.unwrap();
        assert_eq!((state.high_water, after.saved), (9, 1));
        assert_eq!(
            contents(&db).await,
            [(
                "let after_reset = 1;".to_string(),
                Some("javascript".to_string())
            )]
        );
    }

    /// Capture off at startup forgets progress: after opting back in, copies
    /// made while it was off are not saved. An unparseable state file is a
    /// first run too.
    #[tokio::test]
    async fn opting_out_forgets_progress_so_off_period_copies_are_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        let db = db().await;
        write_state(&path, &State { high_water: 0 }).unwrap();
        forget_progress(dir.path()).unwrap();
        assert!(!path.exists());
        forget_progress(dir.path()).unwrap(); // already gone: fine
        let (state, stats) = pass(&db, &governor(), false, &path).await.unwrap();
        assert_eq!((state.high_water, stats.saved), (5, 0));
        assert!(contents(&db).await.is_empty());

        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(read_state(&path), None);
    }

    /// No state file: copies already recorded are not saved; later ones are.
    #[tokio::test]
    async fn first_run_saves_only_copies_made_after_opting_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        let db = db().await;
        let (state, stats) = pass(&db, &governor(), false, &path).await.unwrap();
        assert_eq!((state.high_water, stats.saved), (5, 0));
        assert_eq!(read_state(&path), Some(state));
        assert!(contents(&db).await.is_empty());
        db.execute_raw_sql_write(
            "INSERT INTO ui_events(id,timestamp,event_type,modifiers,text_content) VALUES \
             (6,'2026-11-28T10:00:05Z','clipboard',99,'SELECT 1 FROM t;')",
        )
        .await
        .unwrap();
        let (state, stats) = pass(&db, &governor(), false, &path).await.unwrap();
        assert_eq!((state.high_water, stats.saved), (6, 1));
    }

    /// Waiting for redaction stops the scan at the first unredacted copy, so
    /// nothing after it is skipped; a copy waiting over a day is passed over.
    #[tokio::test]
    async fn waits_for_redaction_and_passes_over_stale_copies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        let db = db().await;
        // Make 1-3 a day old, 4 recent; only 4 is redacted.
        db.execute_raw_sql_write(
            "UPDATE ui_events SET timestamp = '2020-01-01T00:00:00Z' WHERE id <= 3; \
             UPDATE ui_events SET timestamp = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id >= 4; \
             UPDATE ui_events SET redacted_at = 1 WHERE id = 4; \
             INSERT INTO ui_events(id,timestamp,event_type,text_content) VALUES \
             (6,strftime('%Y-%m-%dT%H:%M:%fZ','now'),'clipboard','let waiting = true;'), \
             (7,strftime('%Y-%m-%dT%H:%M:%fZ','now'),'clipboard','let after = 1;');",
        )
        .await
        .unwrap();
        db.execute_raw_sql_write("UPDATE ui_events SET redacted_at = 1 WHERE id = 7")
            .await
            .unwrap();
        write_state(&path, &State { high_water: 0 }).unwrap();
        let (state, stats) = pass(&db, &governor(), true, &path).await.unwrap();
        assert_eq!(state.high_water, 4, "stops before waiting copy 6");
        assert_eq!((stats.passed_over, stats.saved), (3, 1));
        db.execute_raw_sql_write("UPDATE ui_events SET redacted_at = 1 WHERE id = 6")
            .await
            .unwrap();
        let (state, stats) = pass(&db, &governor(), true, &path).await.unwrap();
        assert_eq!((state.high_water, stats.saved), (7, 2));
    }
}
