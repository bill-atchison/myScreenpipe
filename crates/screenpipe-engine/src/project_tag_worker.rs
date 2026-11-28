// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

//! Rules-based `project:<name>` tags on frames, so `/search?tags=project:x`
//! filters by project.
//!
//! `<data dir>/project-rules.json` (`{"projects": {"<name>": ["pattern"]}}`)
//! is the on/off switch: without it the worker idles. Every few minutes it
//! reloads the rules; when they change it clears every `project:` tag and
//! re-tags from the first frame. Progress lives in a sidecar
//! `project-rules.state.json`, not in the database. Each batch takes the
//! background CPU lane and writes through the shared writer; capture never
//! waits on it.

use anyhow::{Context, Result};
use screenpipe_db::DatabaseManager;
use screenpipe_resource::ResourceGovernor;
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
/// Frame ids per tagging transaction.
const BATCH: i64 = 20_000;
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

/// Bring the tags up to date with `rules`: clear and restart when they
/// changed, then tag every frame above the high water mark.
pub async fn tag_pass(
    db: &DatabaseManager,
    governor: &Arc<ResourceGovernor>,
    rules: &Rules,
    state_path: &Path,
) -> Result<State> {
    let hash = rules_hash(rules);
    let mut state = read_state(state_path);
    if state.rules_hash != hash {
        loop {
            let _permit = governor.acquire_background_cpu().await;
            if db.clear_project_tags(CLEAR_BATCH).await? == 0 {
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
    while state.high_water < max {
        let to = (state.high_water + BATCH).min(max);
        let permit = governor.acquire_background_cpu().await;
        let started = Instant::now();
        let tagged = db
            .tag_project_frames(rules, state.high_water + 1, to)
            .await?;
        state.high_water = to;
        write_state(state_path, &state)?;
        let worked = started.elapsed();
        let cooldown = permit.finish(worked, MIN_COOLDOWN, MAX_COOLDOWN).cooldown;
        debug!(
            to,
            tagged,
            worked_ms = worked.as_millis(),
            "project tags: batch"
        );
        // Hold the lane through the cooldown, as the embedding worker does.
        tokio::time::sleep(cooldown).await;
    }
    Ok(state)
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
        Ok(state) => {
            debug!(
                projects = rules.len(),
                high_water = state.high_water,
                "project tags: pass done"
            );
            Some(state)
        }
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
}
