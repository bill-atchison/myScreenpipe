// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

//! Rules-based `project:<name>` tags on frames, so `/search?tags=project:x`
//! filters by project.
//!
//! `<data dir>/project-rules.json` (`{"projects": {"<name>": ["pattern"]}}`)
//! is the on/off switch: without it the worker idles. Every few minutes it
//! reloads the rules; when they change it clears every `project:` tag and
//! re-tags from the first frame. Progress lives in a sidecar
//! `project-rules.state.json`, not in the database.
//!
//! Each batch takes the background CPU lane, reads and matches frames
//! without the writer, then holds the shared writer only to insert the tag
//! links, so capture writes queue behind at most one short insert. Batches
//! are sized by time. Tags are taken from titles and URLs as they are when
//! tagged; redacting them afterwards does not remove the tags.

use anyhow::{Context, Result};
use screenpipe_db::DatabaseManager;
use screenpipe_resource::{ResourceGovernor, WorkSliceConfig, WorkSliceController};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

pub const RULES_FILE: &str = "project-rules.json";
const STATE_FILE: &str = "project-rules.state.json";
const INTERVAL: Duration = Duration::from_secs(300);
/// Frames read per batch, sized by time between these bounds.
const INITIAL_BATCH: u32 = 1_000;
const MAX_BATCH: u32 = 50_000;
/// Tag links removed per clearing transaction.
const CLEAR_BATCH: i64 = 20_000;
const MIN_COOLDOWN: Duration = Duration::from_millis(50);
const MAX_COOLDOWN: Duration = Duration::from_secs(60);

/// Project name to its patterns, empty ones dropped; sorted so the hash only
/// changes when the rules do.
pub type Rules = Vec<(String, Vec<String>)>;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub rules_hash: i64,
    pub high_water: i64,
}

#[derive(Deserialize)]
struct RulesFile {
    projects: BTreeMap<String, Vec<String>>,
}

/// `Ok(None)`: no rules file.
pub fn load_rules(path: &Path) -> Result<Option<Rules>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let file: RulesFile =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(
        file.projects
            .into_iter()
            .filter(|(name, _)| !name.trim().is_empty())
            .map(|(name, patterns)| {
                let mut patterns: Vec<String> = patterns
                    .into_iter()
                    .map(|p| p.trim().to_owned())
                    .filter(|p| !p.is_empty())
                    .collect();
                patterns.sort();
                patterns.dedup();
                (name, patterns)
            })
            .collect(),
    ))
}

fn rules_hash(rules: &Rules) -> i64 {
    screenpipe_embed::text_hash(&serde_json::to_string(rules).expect("rules serialize"))
}

/// A missing or unreadable state file starts over, which re-tags everything.
pub fn read_state(path: &Path) -> State {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn write_state(path: &Path, state: &State) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(state)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassStats {
    pub scanned: u64,
    pub tagged: u64,
    /// Longest single hold of the shared writer, clearing included.
    pub longest_hold: Duration,
}

/// Bring the tags up to date with `rules`: clear and restart when they
/// changed, then tag every frame above the high water mark that existed when
/// the pass began.
pub async fn tag_pass(
    db: &DatabaseManager,
    governor: &Arc<ResourceGovernor>,
    rules: &Rules,
    state_path: &Path,
) -> Result<(State, PassStats)> {
    let hash = rules_hash(rules);
    let mut state = read_state(state_path);
    let mut stats = PassStats::default();
    if state.rules_hash != hash {
        loop {
            let _permit = governor.acquire_background_cpu().await;
            let (removed, hold) = db.clear_project_tags(CLEAR_BATCH).await?;
            stats.longest_hold = stats.longest_hold.max(hold);
            if removed == 0 {
                break;
            }
        }
        // Persisted only once clearing is done: a crash before this clears again.
        state = State {
            rules_hash: hash,
            high_water: 0,
        };
        write_state(state_path, &state)?;
    }
    let max = db.max_frame_id().await?;
    if max < state.high_water {
        // A replaced or reset database: ids restart, so would the tags.
        info!(
            max,
            high_water = state.high_water,
            "project tags: frame ids went backwards, re-tagging from the start"
        );
        state.high_water = 0;
        write_state(state_path, &state)?;
    }
    let mut sizer = WorkSliceController::new(WorkSliceConfig {
        max_units: MAX_BATCH,
        initial_units: INITIAL_BATCH,
        ..WorkSliceConfig::default()
    });
    while state.high_water < max {
        let permit = governor.acquire_background_cpu().await;
        let started = Instant::now();
        let batch = db
            .tag_project_frames(rules, state.high_water, i64::from(sizer.current_units()))
            .await?;
        if batch.scanned == 0 {
            break;
        }
        state.high_water = batch.last_id;
        write_state(state_path, &state)?;
        stats.scanned += batch.scanned;
        stats.tagged += batch.tagged;
        stats.longest_hold = stats.longest_hold.max(batch.writer_hold);
        let worked = started.elapsed();
        let next = sizer.observe(worked);
        let cooldown = permit.finish(worked, MIN_COOLDOWN, MAX_COOLDOWN).cooldown;
        debug!(
            high_water = state.high_water,
            scanned = batch.scanned,
            tagged = batch.tagged,
            next,
            worked_ms = worked.as_millis(),
            hold_ms = batch.writer_hold.as_millis(),
            "project tags: batch"
        );
        // Hold the lane through the cooldown, as the embedding worker does.
        tokio::time::sleep(cooldown).await;
    }
    info!(
        projects = rules.len(),
        scanned = stats.scanned,
        tagged = stats.tagged,
        longest_writer_hold_ms = stats.longest_hold.as_millis(),
        high_water = state.high_water,
        "project tags: pass done"
    );
    Ok((state, stats))
}

/// Tag frames for the engine's lifetime; aborted on shutdown.
pub fn spawn(db: Arc<DatabaseManager>, data_dir: PathBuf) -> JoinHandle<()> {
    tokio::spawn(async move {
        let governor = ResourceGovernor::global();
        let mut rules = None;
        // Each distinct condition is logged once, not every interval.
        let mut last_note = String::new();
        let mut note = |message: String, warning: bool| {
            if message != last_note {
                if warning {
                    warn!("{message}");
                } else {
                    info!("{message}");
                }
                last_note = message;
            }
        };
        loop {
            tick(&db, &governor, &data_dir, &mut rules, &mut note).await;
            tokio::time::sleep(INTERVAL).await;
        }
    })
}

/// Reload the rules and tag with them; `None` while there are none.
/// An invalid file keeps the last good rules.
pub async fn tick(
    db: &DatabaseManager,
    governor: &Arc<ResourceGovernor>,
    data_dir: &Path,
    rules: &mut Option<Rules>,
    note: &mut impl FnMut(String, bool),
) -> Option<State> {
    let rules_path = data_dir.join(RULES_FILE);
    match load_rules(&rules_path) {
        Ok(Some(loaded)) => *rules = Some(loaded),
        Ok(None) => {
            *rules = None;
            note(
                format!("project tags: no {}, idle", rules_path.display()),
                false,
            );
        }
        Err(e) => note(
            format!("project tags: invalid rules, keeping the last good ones: {e:#}"),
            true,
        ),
    }
    let rules = rules.as_ref()?;
    match tag_pass(db, governor, rules, &data_dir.join(STATE_FILE)).await {
        Ok((state, _)) => Some(state),
        Err(e) => {
            note(format!("project tags: pass failed, retrying: {e:#}"), true);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db_with_frames() -> DatabaseManager {
        let db = DatabaseManager::new("sqlite::memory:", Default::default())
            .await
            .unwrap();
        db.execute_raw_sql_write(
            "INSERT INTO frames(id,timestamp,window_name) VALUES \
             (1,'2026-11-26T12:00:00Z','DT POS'),(2,'2026-11-26T12:00:01Z','Canopy board');",
        )
        .await
        .unwrap();
        db
    }

    async fn tags(db: &DatabaseManager) -> Vec<String> {
        let mut out = Vec::new();
        for id in [1, 2] {
            for t in db
                .get_tags(id, screenpipe_db::TagContentType::Vision)
                .await
                .unwrap()
            {
                out.push(format!("{id}:{t}"));
            }
        }
        out
    }

    fn governor() -> Arc<ResourceGovernor> {
        ResourceGovernor::new(Default::default())
    }

    #[tokio::test]
    async fn idles_without_a_rules_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = db_with_frames().await;
        let mut notes = Vec::new();
        let state = tick(&db, &governor(), dir.path(), &mut None, &mut |m, _| {
            notes.push(m)
        })
        .await;
        assert_eq!(state, None);
        assert!(tags(&db).await.is_empty());
        assert!(!dir.path().join(STATE_FILE).exists());
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn state_round_trips_and_missing_state_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        assert_eq!(read_state(&path), State::default());
        let state = State {
            rules_hash: -42,
            high_water: 20_000,
        };
        write_state(&path, &state).unwrap();
        assert_eq!(read_state(&path), state);
    }

    #[tokio::test]
    async fn tags_with_rules_retags_on_change_and_keeps_last_good() {
        let dir = tempfile::tempdir().unwrap();
        let rules_path = dir.path().join(RULES_FILE);
        let db = db_with_frames().await;
        let mut rules = None;
        let mut quiet = |_: String, _: bool| {};

        std::fs::write(&rules_path, r#"{"projects":{"dtpos":["dt pos"]}}"#).unwrap();
        let state = tick(&db, &governor(), dir.path(), &mut rules, &mut quiet)
            .await
            .unwrap();
        assert_eq!(state.high_water, 2);
        assert_eq!(read_state(&dir.path().join(STATE_FILE)), state);
        assert_eq!(tags(&db).await, ["1:project:dtpos"]);

        // Invalid file: the last good rules stay in force.
        std::fs::write(&rules_path, "{not json").unwrap();
        let mut warned = false;
        let kept = tick(&db, &governor(), dir.path(), &mut rules, &mut |_, w| {
            warned |= w
        })
        .await;
        assert!(warned);
        assert_eq!(kept, Some(state.clone()));

        std::fs::write(&rules_path, r#"{"projects":{"canopy":["CANOPY"]}}"#).unwrap();
        let changed = tick(&db, &governor(), dir.path(), &mut rules, &mut quiet)
            .await
            .unwrap();
        assert_ne!(changed.rules_hash, state.rules_hash);
        assert_eq!(tags(&db).await, ["2:project:canopy"]);
    }

    /// A database replaced under the same data dir restarts frame ids; a
    /// high water mark above them must not stall tagging forever.
    #[tokio::test]
    async fn high_water_above_the_last_frame_restarts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(RULES_FILE),
            r#"{"projects":{"dtpos":["dt pos"]}}"#,
        )
        .unwrap();
        let db = db_with_frames().await;
        let mut rules = None;
        let mut quiet = |_: String, _: bool| {};
        let state = tick(&db, &governor(), dir.path(), &mut rules, &mut quiet)
            .await
            .unwrap();
        // The old database had more frames; this one has no tags yet.
        write_state(
            &dir.path().join(STATE_FILE),
            &State {
                high_water: 1_000,
                ..state
            },
        )
        .unwrap();
        db.execute_raw_sql_write("DELETE FROM vision_tags")
            .await
            .unwrap();
        let after = tick(&db, &governor(), dir.path(), &mut rules, &mut quiet)
            .await
            .unwrap();
        assert_eq!(after.high_water, 2);
        assert_eq!(tags(&db).await, ["1:project:dtpos"]);
    }

    /// Longest writer hold and total time of a full first pass over 400k
    /// frames, in both storage modes, with the personal rules file's shape.
    #[tokio::test]
    #[ignore = "benchmark; run with --ignored --nocapture"]
    async fn full_pass_cost() {
        const FRAMES: i64 = 400_000;
        let rules = load_rules(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../personal/project-rules.json"),
        )
        .unwrap()
        .unwrap();
        for hybrid in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = if hybrid {
                DatabaseManager::new_hybrid(root.path(), Default::default(), Default::default())
                    .await
                    .unwrap()
            } else {
                let path = root.path().join("db.sqlite");
                DatabaseManager::new(path.to_str().unwrap(), Default::default())
                    .await
                    .unwrap()
            };
            // About 1 in 25 frames matches a project.
            db.execute_raw_sql_write(&format!(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<{FRAMES}) \
                 INSERT INTO frames(id,timestamp,window_name,browser_url,document_path) \
                 SELECT i, strftime('%Y-%m-%dT%H:%M:%fZ','2026-01-01','+'||i||' seconds'), \
                   CASE i % 100 WHEN 0 THEN 'DT POS - Tax ' || i WHEN 1 THEN 'Canopy board' \
                     ELSE 'Some window title number ' || i END, \
                   CASE WHEN i % 100 = 2 THEN 'https://github.com/x/screenpipe/pull/' || i \
                     ELSE 'https://example.com/page/' || i END, \
                   CASE WHEN i % 100 = 3 THEN '/Users/x/src/zpos/main.clw' END \
                 FROM n"
            ))
            .await
            .unwrap();
            let state_path = root.path().join(STATE_FILE);
            let started = Instant::now();
            let (state, stats) = tag_pass(&db, &governor(), &rules, &state_path)
                .await
                .unwrap();
            let total = started.elapsed();
            assert_eq!(state.high_water, FRAMES);
            assert_eq!(stats.scanned, FRAMES as u64);
            assert_eq!(stats.tagged, (FRAMES / 100 * 4) as u64);
            eprintln!(
                "PROJECT_TAGS mode={} frames={FRAMES} tagged={} longest_writer_hold_ms={:.1} total_ms={}",
                if hybrid { "compressed" } else { "sqlite" },
                stats.tagged,
                stats.longest_hold.as_secs_f64() * 1000.0,
                total.as_millis()
            );
            db.close().await;
        }
    }
}
