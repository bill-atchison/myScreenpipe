// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

//! `GET /search/semantic`: hybrid search. Nearest embedded rows and keyword
//! (FTS) matches are fused by reciprocal rank. Read-only. Needs the model
//! the embedding worker loaded; this route never loads or downloads it.
//!
//! Every item passes the text privacy release gate. A vector hit is also
//! dropped unless the row's current released text still hashes to the text
//! it was embedded from, so edited or re-redacted text never surfaces
//! through a stale vector.

use crate::history_access::HistoryAccessPolicy;
use crate::routes::search::{apply_search_history_access, OptionalPipePerms};
use crate::server::AppState;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use screenpipe_db::{DatabaseManager, EmbeddingSourceType, SemanticRow, EMBEDDING_DIMS};
use screenpipe_embed::{chunk, text_hash};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::error;

const MAX_QUERY_CHARS: usize = 1000;
const DEFAULT_LIMIT: u32 = 20;
const MAX_LIMIT: u32 = 100;
/// Each side contributes this many candidates per requested result.
const DEPTH_PER_RESULT: u32 = 4;
const MAX_DEPTH: u32 = 200;
/// The usual reciprocal-rank-fusion constant.
const RRF_K: f64 = 60.0;
const EXCERPT_CHARS: usize = 300;

type Vector = [f32; EMBEDDING_DIMS];
type Key = (EmbeddingSourceType, i64);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentFilter {
    Frame,
    Audio,
    #[default]
    All,
}

#[derive(Debug, Deserialize)]
pub struct SemanticSearchParams {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub content_type: ContentFilter,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
}

fn default_limit() -> u32 {
    DEFAULT_LIMIT
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SemanticSearchItem {
    pub source_type: &'static str,
    pub id: i64,
    pub timestamp: DateTime<Utc>,
    pub text: String,
    pub app_name: Option<String>,
    pub window_name: Option<String>,
    pub device: Option<String>,
    pub speaker: Option<String>,
    pub score: f64,
    pub matched_by: &'static str,
}

type Failure = (StatusCode, Json<Value>);

fn failure(status: StatusCode, message: &str) -> Failure {
    (status, Json(json!({ "error": message })))
}

fn internal(e: impl std::fmt::Display) -> Failure {
    error!("semantic search failed: {e}");
    failure(StatusCode::INTERNAL_SERVER_ERROR, "semantic search failed")
}

pub(crate) async fn semantic_search(
    State(state): State<Arc<AppState>>,
    OptionalPipePerms(pipe): OptionalPipePerms,
    Query(params): Query<SemanticSearchParams>,
) -> Result<Json<Value>, Failure> {
    // Per-item app/window/content filtering is not built here.
    if pipe.is_some_and(|p| p.has_data_restrictions() || p.privacy_filter) {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "semantic search is not available to pipes with data restrictions; use /search",
        ));
    }
    let q = params.q.trim().to_string();
    if q.is_empty() || params.q.chars().count() > MAX_QUERY_CHARS {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "q must be 1 to 1000 characters",
        ));
    }
    let Some(model) = crate::embedding_worker::model() else {
        return Err(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "semantic search not enabled or model not loaded",
        ));
    };
    let text = q.clone();
    // CPU-bound inference stays off the async executor. Only the query's
    // first chunk (~254 tokens) is embedded.
    let vector = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vector>> {
        let Some(first) = chunk(&text, model.tokenizer()).into_iter().next() else {
            return Ok(None);
        };
        Ok(model.embed(&[first.as_str()])?.into_iter().next())
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    let items = hybrid_search(
        &state.db,
        &q,
        vector.as_ref(),
        &params,
        &state.history_access,
    )
    .await
    .map_err(internal)?;
    Ok(Json(json!({ "data": items })))
}

/// The search behind the route, given the query's vector (`None`: keyword
/// matches only). `db` must be the API's protected read adapter.
pub(crate) async fn hybrid_search(
    db: &DatabaseManager,
    q: &str,
    vector: Option<&Vector>,
    params: &SemanticSearchParams,
    history: &HistoryAccessPolicy,
) -> Result<Vec<SemanticSearchItem>, sqlx::Error> {
    use EmbeddingSourceType::{Audio, Frame};
    let (mut start, end) = (params.start_time, params.end_time);
    if apply_search_history_access(history, &mut start, end, Utc::now()) {
        return Ok(Vec::new());
    }
    let limit = params.limit.clamp(1, MAX_LIMIT) as usize;
    let depth = (limit as u32 * DEPTH_PER_RESULT).min(MAX_DEPTH) as usize;
    let types: &[EmbeddingSourceType] = match params.content_type {
        ContentFilter::Frame => &[Frame],
        ContentFilter::Audio => &[Audio],
        ContentFilter::All => &[Frame, Audio],
    };

    // Nearest rows first, each with the hash of the text it was embedded from.
    let mut embedded: HashMap<Key, i64> = HashMap::new();
    let mut nearest: Vec<Key> = Vec::new();
    if let Some(vector) = vector {
        for (kind, id, _, _, hash) in db.knn(vector, depth as i64).await? {
            if nearest.len() < depth && types.contains(&kind) && !embedded.contains_key(&(kind, id))
            {
                embedded.insert((kind, id), hash);
                nearest.push((kind, id));
            }
        }
    }
    // FTS ranks are per table, so each table is its own ranked list.
    let mut keyword: Vec<Vec<Key>> = Vec::new();
    for &kind in types {
        let ids = db.keyword_hits(kind, q, start, end, depth as i64).await?;
        keyword.push(ids.into_iter().map(|id| (kind, id)).collect());
    }

    // Read every candidate through the release gate; deleted, unreleased or
    // out-of-range rows are not returned.
    let mut rows: HashMap<Key, SemanticRow> = HashMap::new();
    for &kind in types {
        let ids: HashSet<i64> = nearest
            .iter()
            .chain(keyword.iter().flatten())
            .filter(|key| key.0 == kind)
            .map(|key| key.1)
            .collect();
        let ids: Vec<i64> = ids.into_iter().collect();
        for row in db.semantic_rows(kind, &ids, start, end).await? {
            rows.insert((kind, row.id), row);
        }
    }
    nearest.retain(|key| {
        rows.get(key).is_some_and(|row| {
            row.embedded_text.as_deref().map(text_hash) == embedded.get(key).copied()
        })
    });
    for list in &mut keyword {
        list.retain(|key| rows.contains_key(key));
    }

    let mut lists = vec![(Side::Vector, nearest.as_slice())];
    lists.extend(keyword.iter().map(|list| (Side::Keyword, list.as_slice())));
    Ok(fuse(&lists)
        .into_iter()
        .take(limit)
        .filter_map(|(key, score, matched_by)| {
            let row = rows.remove(&key)?;
            Some(SemanticSearchItem {
                source_type: key.0.as_str(),
                id: key.1,
                timestamp: row.timestamp,
                text: row.text.chars().take(EXCERPT_CHARS).collect(),
                app_name: row.app_name,
                window_name: row.window_name,
                device: row.device,
                speaker: row.speaker,
                score,
                matched_by,
            })
        })
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Vector,
    Keyword,
}

/// Reciprocal-rank fusion: each list adds `1 / (RRF_K + rank)` (rank from 1,
/// a key counted once per list) to a key's score. Best first; equal scores
/// keep first-seen order. Returns `(key, score, matched_by)`.
fn fuse(lists: &[(Side, &[Key])]) -> Vec<(Key, f64, &'static str)> {
    // Per key: (score, matched by vector, matched by keyword).
    let mut scores: HashMap<Key, (f64, bool, bool)> = HashMap::new();
    let mut order: Vec<Key> = Vec::new();
    for &(side, list) in lists {
        let mut seen = HashSet::new();
        for (rank, key) in list.iter().filter(|key| seen.insert(**key)).enumerate() {
            let entry = scores.entry(*key).or_insert_with(|| {
                order.push(*key);
                (0.0, false, false)
            });
            entry.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
            match side {
                Side::Vector => entry.1 = true,
                Side::Keyword => entry.2 = true,
            }
        }
    }
    let mut fused: Vec<_> = order
        .into_iter()
        .map(|key| {
            let (score, vector, keyword) = scores[&key];
            let matched_by = match (vector, keyword) {
                (true, true) => "both",
                (true, false) => "vector",
                _ => "keyword",
            };
            (key, score, matched_by)
        })
        .collect();
    // Stable: equal scores keep first-seen order.
    fused.sort_by(|a, b| b.1.total_cmp(&a.1));
    fused
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, Router};
    use screenpipe_audio::audio_manager::AudioManagerBuilder;
    use screenpipe_core::pipes::permissions::{PermissionRule, PipePermissions};
    use screenpipe_db::privacy_release::ReleasePolicy;
    use screenpipe_db::{EmbeddingChunk, EmbeddingSource};
    use tower::ServiceExt;

    const KEY: &str = "test-key";
    const FRAME: EmbeddingSourceType = EmbeddingSourceType::Frame;
    const AUDIO: EmbeddingSourceType = EmbeddingSourceType::Audio;

    #[test]
    fn fusion_ranks_by_reciprocal_rank_and_dedupes() {
        let (a, b, c, d) = ((FRAME, 1), (FRAME, 2), (AUDIO, 1), (AUDIO, 2));
        let vector = [a, b, c];
        // A repeated key counts once, at its first rank.
        let keyword = [c, d, a, a];
        let fused = fuse(&[(Side::Vector, &vector[..]), (Side::Keyword, &keyword[..])]);
        let order: Vec<_> = fused.iter().map(|f| (f.0, f.2)).collect();
        // a and c tie at 1/61 + 1/63, b and d at 1/62: first seen wins.
        assert_eq!(
            order,
            [(a, "both"), (c, "both"), (b, "vector"), (d, "keyword")]
        );
        let score = |rank: f64| 1.0 / (RRF_K + rank);
        assert!((fused[0].1 - (score(1.0) + score(3.0))).abs() < 1e-12);
        assert!((fused[2].1 - score(2.0)).abs() < 1e-12);
        let none: &[Key] = &[];
        assert!(fuse(&[(Side::Vector, none), (Side::Keyword, none)]).is_empty());
    }

    async fn router(db: Arc<DatabaseManager>) -> Router {
        let dir = tempfile::tempdir().unwrap();
        let audio = Arc::new(
            AudioManagerBuilder::new()
                .is_disabled(true)
                .output_path(dir.path().join("audio"))
                .build(db.clone())
                .await
                .unwrap(),
        );
        let mut server = crate::SCServer::new(
            db,
            ([127, 0, 0, 1], 0).into(),
            dir.path().to_path_buf(),
            true,
            true,
            audio,
            false,
            "balanced".into(),
        );
        server.timeline_disabled = true;
        server.advertise_mdns = false;
        server.api_auth = true;
        server.api_auth_key = Some(KEY.into());
        let content_deny = vec![PermissionRule::Content {
            value: "audio".into(),
        }];
        for (token, deny_rules, privacy_filter) in [
            (OPEN_PIPE, vec![], false),
            (NO_AUDIO_PIPE, content_deny, false),
            (PII_PIPE, vec![], true),
        ] {
            let perms = PipePermissions {
                pipe_name: token.into(),
                allow_rules: vec![],
                deny_rules,
                use_default_allowlist: false,
                time_range: None,
                days: None,
                pipe_token: Some(token.into()),
                pipe_dir: None,
                privacy_filter,
            };
            server
                .pipe_permissions
                .insert(token.into(), Arc::new(perms));
        }
        server.try_create_router().await.unwrap()
    }

    const OPEN_PIPE: &str = "sp_pipe_open";
    const NO_AUDIO_PIPE: &str = "sp_pipe_no_audio";
    const PII_PIPE: &str = "sp_pipe_pii";

    /// Per-item filtering is not built: restricted pipes are refused.
    #[tokio::test]
    async fn refuses_pipes_with_data_restrictions() {
        let r = memory_router().await;
        for pipe in [NO_AUDIO_PIPE, PII_PIPE] {
            let (status, body) = get(&r, Some(pipe), "q=hello").await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{pipe}");
            assert_eq!(
                body["error"],
                "semantic search is not available to pipes with data restrictions; use /search"
            );
        }
        // Unrestricted pipes and the app get past the check (503: no model).
        for token in [OPEN_PIPE, KEY] {
            let (status, _) = get(&r, Some(token), "q=hello").await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{token}");
        }
    }

    async fn get(router: &Router, token: Option<&str>, query: &str) -> (StatusCode, Value) {
        let mut req = Request::builder().uri(format!("/search/semantic?{query}"));
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let res = router
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn memory_router() -> Router {
        router(Arc::new(
            DatabaseManager::new("sqlite::memory:", Default::default())
                .await
                .unwrap(),
        ))
        .await
    }

    #[tokio::test]
    async fn requires_token() {
        let r = memory_router().await;
        assert_eq!(get(&r, None, "q=hello").await.0, StatusCode::FORBIDDEN);
        assert_eq!(
            get(&r, Some("nope"), "q=hello").await.0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn rejects_empty_or_oversized_query() {
        let r = memory_router().await;
        for query in ["q=", "q=%20%20", "limit=5"] {
            assert_eq!(
                get(&r, Some(KEY), query).await.0,
                StatusCode::BAD_REQUEST,
                "{query}"
            );
        }
        // 1000 multi-byte characters are allowed; 1001 are not.
        let ok = "%C3%A9".repeat(1000);
        assert_ne!(
            get(&r, Some(KEY), &format!("q={ok}")).await.0,
            StatusCode::BAD_REQUEST
        );
        let big = "x".repeat(1001);
        assert_eq!(
            get(&r, Some(KEY), &format!("q={big}")).await.0,
            StatusCode::BAD_REQUEST
        );
    }

    /// The engine test binary never loads the model.
    #[tokio::test]
    async fn unavailable_without_model() {
        assert!(crate::embedding_worker::model().is_none());
        let r = memory_router().await;
        let (status, body) = get(&r, Some(KEY), "q=hello").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body["error"],
            "semantic search not enabled or model not loaded"
        );
    }

    fn unit(axis: usize) -> Vector {
        let mut v = [0.0; EMBEDDING_DIMS];
        v[axis] = 1.0;
        v
    }

    fn params(q: &str) -> SemanticSearchParams {
        SemanticSearchParams {
            q: q.into(),
            limit: DEFAULT_LIMIT,
            content_type: ContentFilter::All,
            start_time: None,
            end_time: None,
        }
    }

    /// Text privacy release on. Frames 1 "alpha cat" and 2 "beta cat" are
    /// released, frame 3 "gamma cat" is not. Embedded, all equally near
    /// `unit(0)`: frame 1 from its current text, frame 2 from text since
    /// edited, frame 3 (unreleased), and frame 99, an orphan with no row.
    /// Audio 1 "spoken cat" is released and embedded on another axis.
    /// Returns the API's protected read adapter.
    async fn privacy_fixture(root: &tempfile::TempDir) -> Arc<DatabaseManager> {
        let path = root.path().join("db.sqlite");
        let db = Arc::new(
            DatabaseManager::new(path.to_str().unwrap(), Default::default())
                .await
                .unwrap(),
        );
        db.configure_release(&ReleasePolicy {
            identity: "test:model-v1".into(),
            text: true,
            images: false,
        })
        .await
        .unwrap();
        db.execute_raw_sql_write(
            "INSERT INTO frames(id,timestamp,full_text,app_name,window_name) VALUES \
               (1,'2026-11-26T12:00:01Z','alpha cat','Notes','n1'), \
               (2,'2026-11-26T12:00:02Z','beta cat','Notes','n2'), \
               (3,'2026-11-26T12:00:03Z','gamma cat','Notes','n3'); \
             INSERT INTO audio_chunks(id,file_path) VALUES(1,'a.wav'); \
             INSERT INTO audio_transcriptions(id,audio_chunk_id,offset_index,timestamp,transcription,device) \
               VALUES(1,1,0,'2026-11-26T12:00:04Z','spoken cat','mic');",
        )
        .await
        .unwrap();
        for kind in ["frames", "audio"] {
            for job in db.release_pending(kind, 10).await.unwrap() {
                if !(kind == "frames" && job.id == "3") {
                    assert!(db.publish_release(&job, &job.values).await.unwrap());
                }
            }
        }
        let source = |source_type, source_id, text: &str| EmbeddingSource {
            source_type,
            source_id,
            text_hash: text_hash(text),
        };
        let chunk = |text: &str, axis| EmbeddingChunk {
            text_hash: text_hash(text),
            chunk_index: 0,
            embedding: unit(axis),
        };
        db.insert_embeddings(
            &[
                source(FRAME, 1, "alpha cat"),
                source(FRAME, 2, "beta dog"),
                source(FRAME, 3, "gamma cat"),
                source(AUDIO, 1, "spoken cat"),
            ],
            &[],
            &[
                chunk("alpha cat", 0),
                chunk("beta dog", 0),
                chunk("gamma cat", 0),
                chunk("spoken cat", 1),
            ],
        )
        .await
        .unwrap();
        // A source whose row is gone (the delete trigger normally removes it).
        db.execute_raw_sql_write(&format!(
            "INSERT INTO embedding_sources(source_type,source_id,text_hash) VALUES('frame',99,{})",
            text_hash("alpha cat")
        ))
        .await
        .unwrap();
        db.protected_reads().await.unwrap()
    }

    /// Frame hits as `(key, matched_by)`, best first.
    async fn ids(
        db: &DatabaseManager,
        q: &str,
        vector: Option<&Vector>,
    ) -> Vec<(Key, &'static str)> {
        let mut frames = params(q);
        frames.content_type = ContentFilter::Frame;
        hybrid_search(db, q, vector, &frames, &open())
            .await
            .unwrap()
            .into_iter()
            .map(|item| {
                let kind = if item.source_type == "frame" {
                    FRAME
                } else {
                    AUDIO
                };
                ((kind, item.id), item.matched_by)
            })
            .collect()
    }

    /// (a) frame 3 is embedded but not released: never returned.
    #[tokio::test]
    async fn unreleased_vector_hit_is_dropped() {
        let root = tempfile::tempdir().unwrap();
        let db = privacy_fixture(&root).await;
        let hits = ids(&db, "zzz", Some(&unit(0))).await;
        assert!(!hits.iter().any(|h| h.0 == (FRAME, 3)), "{hits:?}");
        assert!(hits.contains(&((FRAME, 1), "vector")), "{hits:?}");
        // Nor through the keyword side.
        assert!(ids(&db, "gamma", None).await.is_empty());
    }

    /// (b) frame 2's text changed since it was embedded: the vector hit is
    /// dropped (a keyword match on its current text still counts).
    #[tokio::test]
    async fn vector_hit_with_stale_text_is_dropped() {
        let root = tempfile::tempdir().unwrap();
        let db = privacy_fixture(&root).await;
        let hits = ids(&db, "zzz", Some(&unit(0))).await;
        assert_eq!(hits, [((FRAME, 1), "vector")]);
        // Equal scores (1/61 each): the vector list is seen first.
        assert_eq!(
            ids(&db, "beta", Some(&unit(0))).await,
            [((FRAME, 1), "vector"), ((FRAME, 2), "keyword")]
        );
    }

    /// (c) frame 99's source outlived its row: never returned.
    #[tokio::test]
    async fn orphaned_source_is_never_returned() {
        let root = tempfile::tempdir().unwrap();
        let db = privacy_fixture(&root).await;
        let hits = ids(&db, "alpha", Some(&unit(0))).await;
        assert_eq!(hits, [((FRAME, 1), "both")]);
    }

    /// Released rows come back with their metadata, filtered by type and
    /// time; both tables' keyword matches are fused.
    #[tokio::test]
    async fn returns_released_rows_with_filters() {
        let root = tempfile::tempdir().unwrap();
        let db = privacy_fixture(&root).await;
        let all = hybrid_search(&db, "cat", Some(&unit(1)), &params("cat"), &open())
            .await
            .unwrap();
        assert_eq!(all[0].source_type, "audio");
        assert_eq!(all[0].matched_by, "both");
        assert_eq!(all[0].device.as_deref(), Some("mic"));
        assert_eq!(all[0].text, "spoken cat");
        let mut found: Vec<_> = all.iter().map(|i| (i.source_type, i.id)).collect();
        found.sort();
        assert_eq!(found, [("audio", 1), ("frame", 1), ("frame", 2)]);
        let frame = all
            .iter()
            .find(|i| i.id == 1 && i.source_type == "frame")
            .unwrap();
        assert_eq!(frame.app_name.as_deref(), Some("Notes"));
        assert_eq!(frame.window_name.as_deref(), Some("n1"));

        let mut only_frames = params("cat");
        only_frames.content_type = ContentFilter::Frame;
        only_frames.start_time = Some("2026-11-26T12:00:02Z".parse().unwrap());
        only_frames.limit = 1;
        let hits = hybrid_search(&db, "cat", Some(&unit(1)), &only_frames, &open())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].source_type, hits[0].id), ("frame", 2));
    }

    fn open() -> HistoryAccessPolicy {
        HistoryAccessPolicy::unrestricted()
    }

    /// Rows older than the history-access cutoff never return from either
    /// side; a range ending before it returns nothing.
    #[tokio::test]
    async fn history_cutoff_hides_older_rows() {
        let db = DatabaseManager::new("sqlite::memory:", Default::default())
            .await
            .unwrap();
        let recent = (Utc::now() - chrono::Duration::hours(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        db.execute_raw_sql_write(&format!(
            "INSERT INTO frames(id,timestamp,full_text) VALUES \
               (1,'2020-01-01T00:00:00Z','old cat'),(2,'{recent}','new cat')"
        ))
        .await
        .unwrap();
        let (old, new) = (text_hash("old cat"), text_hash("new cat"));
        let source = |source_id, text_hash| EmbeddingSource {
            source_type: FRAME,
            source_id,
            text_hash,
        };
        let chunk = |text_hash| EmbeddingChunk {
            text_hash,
            chunk_index: 0,
            embedding: unit(0),
        };
        db.insert_embeddings(
            &[source(1, old), source(2, new)],
            &[],
            &[chunk(old), chunk(new)],
        )
        .await
        .unwrap();
        let search = |params: SemanticSearchParams, history: HistoryAccessPolicy| {
            let db = &db;
            async move {
                hybrid_search(db, "cat", Some(&unit(0)), &params, &history)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|i| (i.id, i.matched_by))
                    .collect::<Vec<_>>()
            }
        };
        let mut all = search(params("cat"), open()).await;
        all.sort();
        assert_eq!(all, [(1, "both"), (2, "both")]);
        let limited = HistoryAccessPolicy::last_24_hours;
        assert_eq!(search(params("cat"), limited()).await, [(2, "both")]);
        let mut before = params("cat");
        before.end_time = Some("2020-06-01T00:00:00Z".parse().unwrap());
        assert!(search(before, limited()).await.is_empty());
    }

    const EVAL_FRAMES: [&str; 30] = [
        "fn retry_with_backoff(attempts: u32) { sleep(Duration::from_secs(2u64.pow(attempts))); } // wait longer after each failed request",
        "Subject: Statement from Acme Corp. Hi, the bill from Acme is past due by 14 days. Please remit the balance of $2,300 to avoid late fees.",
        "Onboarding guide: new hires receive a laptop on their first day and must enroll in two-factor authentication before accessing internal systems.",
        "Maria: can we push the release to Thursday? QA found a crash when opening large files",
        "$ git push origin main\nerror: failed to push some refs. hint: Updates were rejected because the remote contains work that you do not have locally",
        "Slow-cooker chili: brown the beef, add beans, tomatoes and cumin, then simmer for six hours.",
        "Team standup moved to 10:30. Conference room Birch is booked; join by video link if remote.",
        "SELECT customer_id, SUM(amount) FROM orders GROUP BY customer_id HAVING SUM(amount) > 10000;",
        "Expense policy: meals while traveling are reimbursed up to $60 per day with itemized receipts.",
        "error[E0382]: borrow of moved value: `config` --> src/main.rs:42:20",
        "Sam: my flight lands at 7pm, can someone pick me up from the airport?",
        "Your password will expire in 3 days. Choose a new one to keep your account secure.",
        "Quarterly revenue grew 12% compared to last year, driven by subscription renewals in the enterprise segment.",
        "Heavy rain expected across the region this weekend with flooding possible near the river.",
        "def test_login_rejects_wrong_password(): assert client.post('/login', json={'password': 'bad'}).status_code == 401",
        "Reminder: the dentist appointment is tomorrow at 3pm, remember to bring your insurance card",
        "Kubernetes pods keep restarting because the container runs out of memory; raise the memory limit in the deployment manifest.",
        "Lee: I'm stepping out for lunch, back in an hour.",
        "Q3 budget: marketing 45,000 engineering 210,000 travel 18,000 total headcount 52",
        "Congratulations! Your job application has moved to the final interview round. Please select a time slot.",
        "npm ERR! ENOSPC: no space left on device, write",
        "Meeting notes: we decided to drop the legacy dashboard and migrate every customer to the new analytics page by March.",
        "// TODO: cache the thumbnails so we don't regenerate them on every scroll",
        "Best hiking trails near Seattle: Rattlesnake Ledge, Mount Si, and Snow Lake, all under ten miles.",
        "Priya: the client hated the first mockup, they want a darker color scheme",
        "Privacy notice: we never sell personal data and you may request deletion of your account at any time.",
        "Your package has shipped and will arrive on Friday. Track it with the number below.",
        "Connection refused: could not reach database server at localhost:5432",
        "button.addEventListener('click', () => { modal.classList.add('hidden'); });",
        "Tom: great news, the funding round closed and we raised the seed money",
    ];

    const EVAL_AUDIO: [&str; 4] = [
        "so basically the sprint is slipping because the backend team is waiting on the design specs",
        "remember to buy milk eggs and bread on the way home",
        "I think we should hire another engineer before the product launch",
        "the quarterly review with the board is scheduled for next Tuesday afternoon",
    ];

    /// (query, target). Frame ids are 1-based positions in `EVAL_FRAMES`.
    const EVAL_QUERIES: [(&str, Key); 11] = [
        ("invoice overdue payment", (FRAME, 2)),
        ("why does my service keep crashing out of memory", (FRAME, 17)),
        ("can't push my code changes", (FRAME, 5)),
        ("groceries to pick up", (AUDIO, 2)),
        ("when does the plane arrive", (FRAME, 11)),
        ("forgot login credentials expiring", (FRAME, 12)),
        ("disk is full", (FRAME, 21)),
        ("decided to retire the old reporting tool", (FRAME, 22)),
        ("wait before retrying failed network calls", (FRAME, 1)),
        ("raised money from investors", (FRAME, 30)),
        ("doctor visit reminder", (FRAME, 16)),
    ];

    /// Recall of keyword-only search versus hybrid search with the real
    /// model, on a small paraphrase fixture. Prints each query's target rank.
    #[tokio::test]
    #[ignore = "downloads all-MiniLM-L6-v2 (~90 MB) from Hugging Face"]
    async fn recall_eval() {
        use screenpipe_embed::{default_cache_dir, Embedder};
        let model = Embedder::load(default_cache_dir().unwrap()).await.unwrap();
        let db = DatabaseManager::new("sqlite::memory:", Default::default())
            .await
            .unwrap();
        let sql_text = |t: &str| t.replace('\'', "''");
        let mut sql = String::new();
        for (i, text) in EVAL_FRAMES.iter().enumerate() {
            sql += &format!(
                "INSERT INTO frames(id,timestamp,full_text) VALUES({},'2026-11-26T12:00:{:02}Z','{}');",
                i + 1,
                i,
                sql_text(text)
            );
        }
        sql += "INSERT INTO audio_chunks(id,file_path) VALUES(1,'a.wav');";
        for (i, text) in EVAL_AUDIO.iter().enumerate() {
            sql += &format!(
                "INSERT INTO audio_transcriptions(id,audio_chunk_id,offset_index,timestamp,transcription,device) \
                 VALUES({0},1,{0},'2026-11-26T13:00:{0:02}Z','{1}','mic');",
                i + 1,
                sql_text(text)
            );
        }
        db.execute_raw_sql_write(&sql).await.unwrap();

        let (mut sources, mut chunks) = (Vec::new(), Vec::new());
        let frames = EVAL_FRAMES.iter().map(|t| (FRAME, *t));
        let audio = EVAL_AUDIO.iter().map(|t| (AUDIO, *t));
        for (n, (kind, text)) in frames.chain(audio).enumerate() {
            // Frames take ids 1..=30; audio restarts at 1.
            let id = if kind == FRAME { n + 1 } else { n + 1 - EVAL_FRAMES.len() } as i64;
            let pieces = chunk(text, model.tokenizer());
            let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
            sources.push(EmbeddingSource {
                source_type: kind,
                source_id: id,
                text_hash: text_hash(text),
            });
            for (index, embedding) in model.embed(&refs).unwrap().into_iter().enumerate() {
                chunks.push(EmbeddingChunk {
                    text_hash: text_hash(text),
                    chunk_index: index as i64,
                    embedding,
                });
            }
        }
        db.insert_embeddings(&sources, &[], &chunks).await.unwrap();

        let rank = |hits: &[SemanticSearchItem], target: Key| {
            hits.iter()
                .position(|h| h.id == target.1 && h.source_type == target.0.as_str())
                .map(|p| p + 1)
        };
        let show = |r: Option<usize>| r.map_or("miss".to_string(), |r| r.to_string());
        let (mut keyword_hits, mut hybrid_hits) = (0, 0);
        eprintln!("{:<50} {:>8} {:>8}", "query", "keyword", "hybrid");
        for (q, target) in EVAL_QUERIES {
            let first = chunk(q, model.tokenizer()).remove(0);
            let vector = model.embed(&[first.as_str()]).unwrap().remove(0);
            let keyword = hybrid_search(&db, q, None, &params(q), &open())
                .await
                .unwrap();
            let hybrid = hybrid_search(&db, q, Some(&vector), &params(q), &open())
                .await
                .unwrap();
            let (k, h) = (rank(&keyword, target), rank(&hybrid, target));
            keyword_hits += k.is_some_and(|r| r <= 5) as usize;
            hybrid_hits += h.is_some_and(|r| r <= 5) as usize;
            eprintln!("{q:<50} {:>8} {:>8}", show(k), show(h));
        }
        let total = EVAL_QUERIES.len() as f64;
        eprintln!(
            "recall@5: keyword {keyword_hits}/{0} = {1:.2}, hybrid {hybrid_hits}/{0} = {2:.2}",
            EVAL_QUERIES.len(),
            keyword_hits as f64 / total,
            hybrid_hits as f64 / total
        );
        assert!(hybrid_hits >= keyword_hits);
        assert!(hybrid_hits as f64 / total >= 0.7);
    }
}
