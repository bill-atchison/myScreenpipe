// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

//! `/snippets`: code the snippet worker saved from clipboard copies. List,
//! read, enrich (title, description, tags, language) and delete. A `PUT`
//! marks the snippet enriched, so `unenriched=true` lists what is left for an
//! enrichment pipe. Pipes with data restrictions are refused: snippets carry
//! no per-item app/window/content filtering. The history-access cutoff
//! applies to every route.

use crate::routes::search::{apply_search_history_access, OptionalPipePerms};
use crate::server::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use screenpipe_db::{Snippet, SnippetFilter, SnippetUpdate};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::error;

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;
const MAX_TITLE_CHARS: usize = 500;
const MAX_DESCRIPTION_CHARS: usize = 10_000;
const MAX_TAGS: usize = 50;
const MAX_TAG_CHARS: usize = 100;
const MAX_LANGUAGE_CHARS: usize = 50;

#[derive(Debug, Deserialize)]
pub struct ListSnippetsParams {
    pub q: Option<String>,
    pub language: Option<String>,
    pub app_name: Option<String>,
    #[serde(
        default,
        deserialize_with = "super::time::deserialize_flexible_datetime_option"
    )]
    pub start_time: Option<DateTime<Utc>>,
    #[serde(
        default,
        deserialize_with = "super::time::deserialize_flexible_datetime_option"
    )]
    pub end_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub unenriched: bool,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

fn default_limit() -> u32 {
    DEFAULT_LIMIT
}

#[derive(Debug, Deserialize)]
pub struct UpdateSnippetBody {
    pub title: Option<String>,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    pub language: Option<String>,
}

type Failure = (StatusCode, Json<Value>);

fn failure(status: StatusCode, message: &str) -> Failure {
    (status, Json(json!({ "error": message })))
}

fn internal(e: impl std::fmt::Display) -> Failure {
    error!("snippets request failed: {e}");
    failure(StatusCode::INTERNAL_SERVER_ERROR, "snippets request failed")
}

fn not_found() -> Failure {
    failure(StatusCode::NOT_FOUND, "snippet not found")
}

fn stored(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn refuse_restricted(pipe: &OptionalPipePerms) -> Result<(), Failure> {
    if pipe
        .0
        .as_ref()
        .is_some_and(|p| p.has_data_restrictions() || p.privacy_filter)
    {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "snippets are not available to pipes with data restrictions",
        ));
    }
    Ok(())
}

/// The snippet, unless it does not exist or is older than the history cutoff.
async fn visible(state: &AppState, id: i64) -> Result<Snippet, Failure> {
    let snippet = state
        .db
        .get_snippet(id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let created = DateTime::parse_from_rfc3339(&snippet.created_at)
        .map(|t| t.with_timezone(&Utc))
        .ok();
    match (state.history_access.cutoff(Utc::now()), created) {
        (None, _) => Ok(snippet),
        (Some(cutoff), Some(created)) if created >= cutoff => Ok(snippet),
        _ => Err(not_found()),
    }
}

fn nonblank(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(crate) async fn list_snippets(
    State(state): State<Arc<AppState>>,
    pipe: OptionalPipePerms,
    Query(params): Query<ListSnippetsParams>,
) -> Result<Json<Value>, Failure> {
    refuse_restricted(&pipe)?;
    let (mut start, end) = (params.start_time, params.end_time);
    if apply_search_history_access(&state.history_access, &mut start, end, Utc::now()) {
        return Ok(Json(json!({ "data": [] })));
    }
    let filter = SnippetFilter {
        q: nonblank(&params.q),
        language: nonblank(&params.language),
        app_name: nonblank(&params.app_name),
        start: start.map(stored),
        end: end.map(stored),
        unenriched: params.unenriched,
        limit: i64::from(params.limit.clamp(1, MAX_LIMIT)),
        offset: i64::from(params.offset),
    };
    let data = state.db.list_snippets(&filter).await.map_err(internal)?;
    Ok(Json(json!({ "data": data })))
}

pub(crate) async fn get_snippet(
    State(state): State<Arc<AppState>>,
    pipe: OptionalPipePerms,
    Path(id): Path<i64>,
) -> Result<Json<Value>, Failure> {
    refuse_restricted(&pipe)?;
    Ok(Json(json!(visible(&state, id).await?)))
}

pub(crate) async fn update_snippet(
    State(state): State<Arc<AppState>>,
    pipe: OptionalPipePerms,
    Path(id): Path<i64>,
    Json(body): Json<UpdateSnippetBody>,
) -> Result<Json<Value>, Failure> {
    refuse_restricted(&pipe)?;
    let too_long =
        |s: &Option<String>, max: usize| s.as_ref().is_some_and(|s| s.chars().count() > max);
    if too_long(&body.title, MAX_TITLE_CHARS)
        || too_long(&body.description, MAX_DESCRIPTION_CHARS)
        || too_long(&body.language, MAX_LANGUAGE_CHARS)
        || body.tags.as_ref().is_some_and(|tags| {
            tags.len() > MAX_TAGS || tags.iter().any(|t| t.chars().count() > MAX_TAG_CHARS)
        })
    {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "title ≤ 500, description ≤ 10000, language ≤ 50 characters; at most 50 tags of ≤ 100",
        ));
    }
    visible(&state, id).await?;
    let update = SnippetUpdate {
        title: body.title,
        description: body.description,
        tags: body.tags,
        language: body.language.map(|l| l.trim().to_lowercase()),
    };
    let snippet = state
        .db
        .update_snippet(id, &update)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    Ok(Json(json!(snippet)))
}

pub(crate) async fn delete_snippet(
    State(state): State<Arc<AppState>>,
    pipe: OptionalPipePerms,
    Path(id): Path<i64>,
) -> Result<StatusCode, Failure> {
    refuse_restricted(&pipe)?;
    visible(&state, id).await?;
    if state.db.delete_snippet(id).await.map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_access::HistoryAccessPolicy;
    use axum::{body::Body, http::Request, Router};
    use screenpipe_audio::audio_manager::AudioManagerBuilder;
    use screenpipe_core::pipes::permissions::{PermissionRule, PipePermissions};
    use screenpipe_db::DatabaseManager;
    use tower::ServiceExt;

    const KEY: &str = "test-key";
    const OPEN_PIPE: &str = "sp_pipe_open";
    const NO_AUDIO_PIPE: &str = "sp_pipe_no_audio";

    /// One old snippet (2020) and one from now.
    async fn router(history: HistoryAccessPolicy) -> Router {
        let db = Arc::new(
            DatabaseManager::new("sqlite::memory:", Default::default())
                .await
                .unwrap(),
        );
        db.execute_raw_sql_write(
            "INSERT INTO snippets(id,created_at,content,content_hash,language,app_name) VALUES \
             (1,'2020-01-01T00:00:00.000Z','fn old() {}','h1','rust','Code'), \
             (2,strftime('%Y-%m-%dT%H:%M:%fZ','now'),'SELECT 1;','h2','sql','Terminal');",
        )
        .await
        .unwrap();
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
        server.history_access = history;
        for (token, deny_rules) in [
            (OPEN_PIPE, vec![]),
            (
                NO_AUDIO_PIPE,
                vec![PermissionRule::Content {
                    value: "audio".into(),
                }],
            ),
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
                privacy_filter: false,
            };
            server
                .pipe_permissions
                .insert(token.into(), Arc::new(perms));
        }
        server.try_create_router().await.unwrap()
    }

    async fn call(
        router: &Router,
        method: &str,
        token: Option<&str>,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = router
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn ids(body: &Value) -> Vec<i64> {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_i64().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn requires_token_and_refuses_restricted_pipes() {
        let r = router(HistoryAccessPolicy::unrestricted()).await;
        assert_eq!(
            call(&r, "GET", None, "/snippets", None).await.0,
            StatusCode::FORBIDDEN
        );
        for uri in ["/snippets", "/snippets/1"] {
            let (status, body) = call(&r, "GET", Some(NO_AUDIO_PIPE), uri, None).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
            assert_eq!(
                body["error"],
                "snippets are not available to pipes with data restrictions"
            );
        }
        let (status, _) = call(&r, "DELETE", Some(NO_AUDIO_PIPE), "/snippets/1", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = call(&r, "GET", Some(OPEN_PIPE), "/snippets", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ids(&body), [2, 1]);
    }

    #[tokio::test]
    async fn lists_filters_enriches_and_deletes() {
        let r = router(HistoryAccessPolicy::unrestricted()).await;
        let get = |uri: &'static str| {
            let r = r.clone();
            async move { call(&r, "GET", Some(KEY), uri, None).await }
        };
        assert_eq!(ids(&get("/snippets?language=sql").await.1), [2]);
        assert_eq!(ids(&get("/snippets?q=old").await.1), [1]);
        assert_eq!(ids(&get("/snippets?app_name=code").await.1), [1]);
        assert_eq!(ids(&get("/snippets?limit=1&offset=1").await.1), [1]);
        assert_eq!(
            ids(&get(
                "/snippets?start_time=2019-12-31T19:00:00-05:00&end_time=2020-01-01T01:00:00Z"
            )
            .await
            .1),
            [1],
            "offset times compare in UTC"
        );
        let (status, one) = get("/snippets/1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(one["content"], "fn old() {}");
        assert_eq!(get("/snippets/99").await.0, StatusCode::NOT_FOUND);

        let (status, updated) = call(
            &r,
            "PUT",
            Some(KEY),
            "/snippets/1",
            Some(json!({"title": "Old fn", "tags": ["legacy"], "language": " Rust "})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(updated["title"], "Old fn");
        assert_eq!(updated["tags"], json!(["legacy"]));
        assert_eq!(updated["language"], "rust");
        assert!(updated["enriched_at"].is_string());
        assert_eq!(ids(&get("/snippets?unenriched=true").await.1), [2]);
        let too_many: Vec<String> = (0..51).map(|i| i.to_string()).collect();
        let (status, _) = call(
            &r,
            "PUT",
            Some(KEY),
            "/snippets/1",
            Some(json!({ "tags": too_many })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = call(&r, "DELETE", Some(KEY), "/snippets/1", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(&r, "DELETE", Some(KEY), "/snippets/1", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(ids(&get("/snippets").await.1), [2]);
    }

    /// Restricted to the last 24 hours: the 2020 snippet is invisible to
    /// every route.
    #[tokio::test]
    async fn history_cutoff_hides_old_snippets() {
        let r = router(HistoryAccessPolicy::last_24_hours()).await;
        let (_, body) = call(&r, "GET", Some(KEY), "/snippets", None).await;
        assert_eq!(ids(&body), [2]);
        let (_, body) = call(
            &r,
            "GET",
            Some(KEY),
            "/snippets?end_time=2021-01-01T00:00:00Z",
            None,
        )
        .await;
        assert_eq!(ids(&body), Vec::<i64>::new());
        for method in ["GET", "DELETE"] {
            let (status, _) = call(&r, method, Some(KEY), "/snippets/1", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
        }
        let (status, _) = call(
            &r,
            "PUT",
            Some(KEY),
            "/snippets/1",
            Some(json!({"title": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
