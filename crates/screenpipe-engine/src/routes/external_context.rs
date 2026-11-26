// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

//! `POST /context/external`: a helper inside a VM reports its foreground
//! app/title so the host can relabel the opaque VM window. Authenticated by the
//! API-auth middleware; additionally refused outright when auth is disabled so
//! an unauthenticated local process cannot spoof app labels.

use crate::external_context::{self, GLOBAL};
use crate::server::AppState;
use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use std::{sync::Arc, time::Instant};

const SOURCE: &str = "parallels-vm";
const MAX_APP_CHARS: usize = 512;
const MAX_TITLE_CHARS: usize = 1024;

#[derive(Deserialize)]
pub struct ExternalContextBody {
    pub source: String,
    pub epoch: u64,
    pub seq: u64,
    pub app_name: String,
    pub window_title: String,
}

pub(crate) async fn post_external_context(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ExternalContextBody>,
) -> StatusCode {
    if !state.api_auth {
        return StatusCode::FORBIDDEN;
    }
    if body.source != SOURCE
        || body.app_name.chars().count() > MAX_APP_CHARS
        || body.window_title.chars().count() > MAX_TITLE_CHARS
    {
        return StatusCode::BAD_REQUEST;
    }
    // A stale post is not an error: the helper retries out of order.
    let _ = external_context::accept(
        &GLOBAL,
        body.epoch,
        body.seq,
        body.app_name,
        body.window_title,
        Instant::now(),
    );
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_context::{current_label_seq, override_for, TEST_LOCK};
    use axum::{body::Body, http::Request, Router};
    use screenpipe_audio::audio_manager::AudioManagerBuilder;
    use screenpipe_db::DatabaseManager;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tower::ServiceExt;

    const KEY: &str = "test-key";

    // GLOBAL keeps a tombstone across tests; every test needs a newer epoch.
    static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1_000_000);
    fn epoch() -> u64 {
        NEXT_EPOCH.fetch_add(1, Ordering::SeqCst)
    }

    async fn router(api_auth: bool) -> Router {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            DatabaseManager::new("sqlite::memory:", Default::default())
                .await
                .unwrap(),
        );
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
        server.api_auth = api_auth;
        server.api_auth_key = Some(KEY.into());
        server.try_create_router().await.unwrap()
    }

    async fn post(
        router: &Router,
        token: Option<&str>,
        source: &str,
        epoch: u64,
        app: &str,
        title: &str,
    ) -> StatusCode {
        let body = serde_json::json!({
            "source": source, "epoch": epoch, "seq": 1, "app_name": app, "window_title": title,
        });
        let mut req = Request::builder()
            .method("POST")
            .uri("/context/external")
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        router
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    // TEST_LOCK serializes GLOBAL across tests and is held across .await on purpose.
    #[allow(clippy::await_holding_lock)]
    async fn rejects_when_auth_disabled() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let r = router(false).await;
        let s = post(&r, Some(KEY), "parallels-vm", epoch(), "A", "t").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    // TEST_LOCK serializes GLOBAL across tests and is held across .await on purpose.
    #[allow(clippy::await_holding_lock)]
    async fn rejects_missing_token() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let r = router(true).await;
        let s = post(&r, None, "parallels-vm", epoch(), "A", "t").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    // TEST_LOCK serializes GLOBAL across tests and is held across .await on purpose.
    #[allow(clippy::await_holding_lock)]
    async fn rejects_wrong_token() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let r = router(true).await;
        let s = post(&r, Some("nope"), "parallels-vm", epoch(), "A", "t").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    // TEST_LOCK serializes GLOBAL across tests and is held across .await on purpose.
    #[allow(clippy::await_holding_lock)]
    async fn rejects_unknown_source() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let r = router(true).await;
        let s = post(&r, Some(KEY), "other", epoch(), "A", "t").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    // TEST_LOCK serializes GLOBAL across tests and is held across .await on purpose.
    #[allow(clippy::await_holding_lock)]
    async fn rejects_oversized_app() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let r = router(true).await;
        // 512 multi-byte chars are > 512 bytes but must still be accepted.
        let ok = "é".repeat(512);
        assert_eq!(
            post(&r, Some(KEY), "parallels-vm", epoch(), &ok, "t").await,
            StatusCode::NO_CONTENT
        );
        let big = "é".repeat(513);
        let s = post(&r, Some(KEY), "parallels-vm", epoch(), &big, "t").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    // TEST_LOCK serializes GLOBAL across tests and is held across .await on purpose.
    #[allow(clippy::await_holding_lock)]
    async fn rejects_oversized_title() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let r = router(true).await;
        let big = "x".repeat(1025);
        let s = post(&r, Some(KEY), "parallels-vm", epoch(), "A", &big).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    // TEST_LOCK serializes GLOBAL across tests and is held across .await on purpose.
    #[allow(clippy::await_holding_lock)]
    async fn empty_app_clears_via_route() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let r = router(true).await;
        assert_eq!(
            post(&r, Some(KEY), "parallels-vm", epoch(), "", "").await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            override_for(
                &GLOBAL,
                Some("Parallels Desktop"),
                true,
                current_label_seq(&GLOBAL),
                Instant::now()
            ),
            None
        );
    }

    #[tokio::test]
    // TEST_LOCK serializes GLOBAL across tests and is held across .await on purpose.
    #[allow(clippy::await_holding_lock)]
    async fn accepts_valid_post() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let r = router(true).await;
        let e = epoch();
        assert_eq!(
            post(&r, Some(KEY), "parallels-vm", e, "MultiTerminal", "tab").await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(current_label_seq(&GLOBAL), Some((e, 1)));
    }
}
