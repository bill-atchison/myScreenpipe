// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

//! Code snippets saved from clipboard copies. The engine's snippet worker
//! decides what is code; this module reads clipboard `ui_events` on the read
//! pool and holds the shared writer only for short inserts and updates.

use super::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::time::Instant;

/// `ui_events.modifiers` holds the clipboard operation as a byte; pastes
/// repeat text that was copied (or came from outside) and are not snippets.
const PASTE: i64 = b'v' as i64;
const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%fZ";

/// One clipboard copy or cut with text, as the worker sees it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ClipboardCopy {
    pub id: i64,
    /// UTC, `YYYY-MM-DDTHH:MM:SS.SSSZ`.
    pub created_at: String,
    pub content: String,
    pub app_name: Option<String>,
    pub window_title: Option<String>,
    pub browser_url: Option<String>,
    pub document_path: Option<String>,
    pub frame_id: Option<i64>,
    /// Released under the text privacy policy and, when the worker waits for
    /// it, PII-redacted. Rows that are not ready must not be stored.
    pub ready: bool,
    /// Not ready after a day: the worker gives up on it instead of waiting.
    pub stale: bool,
    /// Text privacy receipt key `ready` was judged under; `None` while
    /// release is off.
    pub release_key: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewSnippet {
    pub created_at: String,
    pub content: String,
    pub language: Option<String>,
    pub app_name: Option<String>,
    pub window_title: Option<String>,
    pub browser_url: Option<String>,
    pub document_path: Option<String>,
    pub frame_id: Option<i64>,
    pub ui_event_id: i64,
    pub release_key: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snippet {
    pub id: i64,
    pub created_at: String,
    pub content: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub app_name: Option<String>,
    pub window_title: Option<String>,
    pub browser_url: Option<String>,
    pub document_path: Option<String>,
    pub frame_id: Option<i64>,
    pub ui_event_id: Option<i64>,
    pub enriched_at: Option<String>,
}

#[derive(sqlx::FromRow)]
struct SnippetRow {
    id: i64,
    created_at: String,
    content: String,
    language: Option<String>,
    title: Option<String>,
    description: Option<String>,
    tags: String,
    app_name: Option<String>,
    window_title: Option<String>,
    browser_url: Option<String>,
    document_path: Option<String>,
    frame_id: Option<i64>,
    ui_event_id: Option<i64>,
    enriched_at: Option<String>,
}

impl From<SnippetRow> for Snippet {
    fn from(r: SnippetRow) -> Self {
        Snippet {
            id: r.id,
            created_at: r.created_at,
            content: r.content,
            language: r.language,
            title: r.title,
            description: r.description,
            tags: serde_json::from_str(&r.tags).unwrap_or_default(),
            app_name: r.app_name,
            window_title: r.window_title,
            browser_url: r.browser_url,
            document_path: r.document_path,
            frame_id: r.frame_id,
            ui_event_id: r.ui_event_id,
            enriched_at: r.enriched_at,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct SnippetFilter {
    /// Substring of content, title or description (case-insensitive for ASCII).
    pub q: Option<String>,
    pub language: Option<String>,
    pub app_name: Option<String>,
    /// UTC bounds in the stored format.
    pub start: Option<String>,
    pub end: Option<String>,
    pub unenriched: bool,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Default, Clone)]
pub struct SnippetUpdate {
    pub title: Option<String>,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    pub language: Option<String>,
}

const COLUMNS: &str = "id, created_at, content, language, title, description, tags, app_name, \
     window_title, browser_url, document_path, frame_id, ui_event_id, enriched_at";

/// Dedupe key: the same text copied again maps to the same snippet.
pub fn snippet_hash(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// A user's delete of a time range also deletes snippets copied in it.
/// Retention does not: it only unlinks them (see the migration). Bounds are
/// RFC 3339 in any offset, so they are compared as times, not strings.
// ponytail: julianday() scans every snippet; fine for a personal library.
pub(super) async fn delete_snippets_in_range(
    conn: &mut sqlx::pool::PoolConnection<Sqlite>,
    start: &str,
    end: &str,
) -> Result<u64, SqlxError> {
    Ok(sqlx::query(
        "DELETE FROM snippets WHERE julianday(created_at) BETWEEN julianday(?1) AND julianday(?2)",
    )
    .bind(start)
    .bind(end)
    .execute(&mut **conn)
    .await?
    .rows_affected())
}

impl DatabaseManager {
    pub async fn max_ui_event_id(&self) -> Result<i64, SqlxError> {
        sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM ui_events")
            .fetch_one(&self.pool)
            .await
    }

    /// Up to `limit` clipboard copies/cuts with text after ui_event id
    /// `after`, oldest first. `wait_for_redaction`: a row is ready only once
    /// the PII redaction worker has processed it.
    pub async fn clipboard_copies_after(
        &self,
        after: i64,
        limit: i64,
        wait_for_redaction: bool,
    ) -> Result<Vec<ClipboardCopy>, SqlxError> {
        let input_gate = self.release_gate("input", "u.id").await?;
        let frame_gate = self.release_gate("frames", "f.id").await?;
        let release_key = if input_gate == "1" {
            "NULL"
        } else {
            "(SELECT receipt_key FROM _privacy_policy WHERE id = 1)"
        };
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT u.id, COALESCE(strftime('{TIME_FORMAT}', u.timestamp), u.timestamp) AS created_at, \
               u.text_content AS content, u.app_name, u.window_title, u.browser_url, \
               CASE WHEN {frame_gate} THEN f.document_path END AS document_path, u.frame_id, \
               ({input_gate}) AND (?3 = 0 OR u.redacted_at IS NOT NULL) AS ready, \
               julianday(u.timestamp) < julianday('now', '-1 day') AS stale, \
               {release_key} AS release_key \
             FROM ui_events u LEFT JOIN frames f ON f.id = u.frame_id \
             WHERE u.event_type = 'clipboard' AND u.id > ?1 \
               AND u.text_content IS NOT NULL AND trim(u.text_content) != '' \
               AND (u.modifiers IS NULL OR u.modifiers != {PASTE}) \
             ORDER BY u.id LIMIT ?2"
        )))
        .bind(after)
        .bind(limit)
        .bind(wait_for_redaction)
        .fetch_all(&self.pool)
        .await
    }

    /// Insert snippets, skipping text already saved. Returns how many were
    /// added and how long the writer was held.
    pub async fn insert_snippets(
        &self,
        snippets: &[NewSnippet],
    ) -> Result<(u64, Duration), SqlxError> {
        if snippets.is_empty() {
            return Ok((0, Duration::ZERO));
        }
        let rows: Vec<serde_json::Value> = snippets
            .iter()
            .map(|s| {
                let mut v = serde_json::to_value(s).expect("snippet serializes");
                v["content_hash"] = snippet_hash(&s.content).into();
                v
            })
            .collect();
        let payload = serde_json::to_string(&rows).expect("snippets serialize");
        let mut tx = self.begin_immediate_with_retry().await?;
        let held = Instant::now();
        let added = sqlx::query(
            "INSERT OR IGNORE INTO snippets (created_at, content, content_hash, language, \
               app_name, window_title, browser_url, document_path, frame_id, ui_event_id, \
               release_key) \
             SELECT json_extract(value, '$.created_at'), json_extract(value, '$.content'), \
               json_extract(value, '$.content_hash'), json_extract(value, '$.language'), \
               json_extract(value, '$.app_name'), json_extract(value, '$.window_title'), \
               json_extract(value, '$.browser_url'), json_extract(value, '$.document_path'), \
               json_extract(value, '$.frame_id'), json_extract(value, '$.ui_event_id'), \
               json_extract(value, '$.release_key') \
             FROM json_each(?)",
        )
        .bind(payload)
        .execute(&mut **tx.conn())
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok((added, held.elapsed()))
    }

    /// Copy `frame_id` (and the frame's document path) onto snippets created
    /// at or after `since` whose clipboard event was linked to a frame after
    /// the snippet was saved. `guess` re-derives a missing language with the
    /// new path: `(content, document_path, window_title)`. Returns rows linked.
    pub async fn link_snippet_frames(
        &self,
        since: &str,
        limit: i64,
        guess: impl Fn(&str, Option<&str>, Option<&str>) -> Option<String>,
    ) -> Result<u64, SqlxError> {
        let frame_gate = self.release_gate("frames", "f.id").await?;
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            i64,
            String,
            Option<String>,
            Option<String>,
            i64,
            Option<String>,
        )> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT s.id, s.content, s.window_title, s.language, u.frame_id, \
                   CASE WHEN {frame_gate} THEN f.document_path END \
                 FROM snippets s JOIN ui_events u ON u.id = s.ui_event_id \
                 LEFT JOIN frames f ON f.id = u.frame_id \
                 WHERE s.frame_id IS NULL AND u.frame_id IS NOT NULL AND s.created_at >= ?1 \
                 ORDER BY s.id LIMIT ?2"
        )))
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        if rows.is_empty() {
            return Ok(0);
        }
        let updates: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|(id, content, title, language, frame_id, path)| {
                let language = language
                    .or_else(|| guess(&content, path.as_deref(), title.as_deref()));
                serde_json::json!({"id": id, "frame_id": frame_id, "path": path, "language": language})
            })
            .collect();
        let mut tx = self.begin_immediate_with_retry().await?;
        let linked = sqlx::query(
            "UPDATE snippets SET frame_id = j.linked_frame, \
               document_path = COALESCE(snippets.document_path, j.linked_path), \
               language = COALESCE(snippets.language, j.guessed_language) \
             FROM (SELECT json_extract(value, '$.id') AS snippet_id, \
                     json_extract(value, '$.frame_id') AS linked_frame, \
                     json_extract(value, '$.path') AS linked_path, \
                     json_extract(value, '$.language') AS guessed_language \
                   FROM json_each(?)) AS j \
             WHERE snippets.id = j.snippet_id AND snippets.frame_id IS NULL",
        )
        .bind(serde_json::to_string(&updates).expect("links serialize"))
        .execute(&mut **tx.conn())
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(linked)
    }

    /// While text privacy release is on, a snippet is served only if it was
    /// saved under the current receipt key or its clipboard event is
    /// released now; snippets saved before release was switched on stay
    /// hidden until then.
    async fn snippet_release_gate(&self) -> Result<String, SqlxError> {
        let event = self.release_gate("input", "ui_event_id").await?;
        Ok(if event == "1" {
            event
        } else {
            format!(
                "(release_key = (SELECT receipt_key FROM _privacy_policy WHERE id = 1) OR {event})"
            )
        })
    }

    /// Newest first.
    // ponytail: `q` is a LIKE scan over every snippet; add FTS5 if the
    // library grows past what a scan answers quickly (tens of thousands).
    pub async fn list_snippets(&self, filter: &SnippetFilter) -> Result<Vec<Snippet>, SqlxError> {
        let pattern = filter.q.as_ref().map(|q| {
            let escaped = q
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{escaped}%")
        });
        let gate = self.snippet_release_gate().await?;
        let rows: Vec<SnippetRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM snippets \
             WHERE {gate} AND (?1 IS NULL OR content LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\' \
                    OR description LIKE ?1 ESCAPE '\\') \
               AND (?2 IS NULL OR language = ?2 COLLATE NOCASE) \
               AND (?3 IS NULL OR app_name = ?3 COLLATE NOCASE) \
               AND (?4 IS NULL OR created_at >= ?4) \
               AND (?5 IS NULL OR created_at <= ?5) \
               AND (?6 = 0 OR enriched_at IS NULL) \
             ORDER BY created_at DESC, id DESC LIMIT ?7 OFFSET ?8"
        )))
        .bind(pattern)
        .bind(&filter.language)
        .bind(&filter.app_name)
        .bind(&filter.start)
        .bind(&filter.end)
        .bind(filter.unenriched)
        .bind(filter.limit)
        .bind(filter.offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Snippet::from).collect())
    }

    pub async fn get_snippet(&self, id: i64) -> Result<Option<Snippet>, SqlxError> {
        let gate = self.snippet_release_gate().await?;
        let row: Option<SnippetRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM snippets WHERE id = ? AND {gate}"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Snippet::from))
    }

    /// Set the given fields and mark the snippet enriched. `None`: not found.
    pub async fn update_snippet(
        &self,
        id: i64,
        update: &SnippetUpdate,
    ) -> Result<Option<Snippet>, SqlxError> {
        let tags = update
            .tags
            .as_ref()
            .map(|t| serde_json::to_string(t).expect("tags serialize"));
        let mut tx = self.begin_immediate_with_retry().await?;
        let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE snippets SET title = COALESCE(?1, title), \
               description = COALESCE(?2, description), tags = COALESCE(?3, tags), \
               language = COALESCE(?4, language), enriched_at = strftime('{TIME_FORMAT}', 'now') \
             WHERE id = ?5"
        )))
        .bind(&update.title)
        .bind(&update.description)
        .bind(tags)
        .bind(&update.language)
        .bind(id)
        .execute(&mut **tx.conn())
        .await?
        .rows_affected();
        tx.commit().await?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_snippet(id).await
    }

    /// `false`: not found.
    pub async fn delete_snippet(&self, id: i64) -> Result<bool, SqlxError> {
        let mut tx = self.begin_immediate_with_retry().await?;
        let removed = sqlx::query("DELETE FROM snippets WHERE id = ?")
            .bind(id)
            .execute(&mut **tx.conn())
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(removed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1: copy; 2: paste ('v' = 118) of the same text; 3: cut ('x' = 120);
    /// 4: a click; 5: an empty copy; 6: copy with an unknown operation
    /// (NULL), linked to frame 1.
    const SEED: &str = "INSERT INTO frames(id,timestamp,document_path) VALUES (1,'2026-11-28T10:00:00Z','/src/main.rs'); \
        INSERT INTO ui_events(id,timestamp,event_type,modifiers,text_content,app_name,window_title) VALUES \
        (1,'2026-11-28 10:00:00.123456+00:00','clipboard',99,'fn a() {}','Code','main.rs'), \
        (2,'2026-11-28T10:00:01Z','clipboard',118,'fn a() {}','Code','main.rs'), \
        (3,'2026-11-28T10:00:02Z','clipboard',120,'let b = 1;','Code','main.rs'), \
        (4,'2026-11-28T10:00:03Z','click',NULL,NULL,'Code','main.rs'), \
        (5,'2026-11-28T10:00:04Z','clipboard',99,'   ','Code','main.rs'); \
        INSERT INTO ui_events(id,timestamp,event_type,text_content,app_name,frame_id) VALUES \
        (6,'2026-11-28T10:00:05Z','clipboard','SELECT 1;','Terminal',1);";

    fn new_snippet(copy: &ClipboardCopy) -> NewSnippet {
        NewSnippet {
            created_at: copy.created_at.clone(),
            content: copy.content.clone(),
            language: None,
            app_name: copy.app_name.clone(),
            window_title: copy.window_title.clone(),
            browser_url: copy.browser_url.clone(),
            document_path: copy.document_path.clone(),
            frame_id: copy.frame_id,
            ui_event_id: copy.id,
            release_key: copy.release_key,
        }
    }

    async fn exercise(db: &DatabaseManager, mode: &str) {
        assert_eq!(db.max_ui_event_id().await.unwrap(), 6, "{mode}");
        let copies = db.clipboard_copies_after(0, 100, false).await.unwrap();
        let ids: Vec<i64> = copies.iter().map(|c| c.id).collect();
        assert_eq!(ids, [1, 3, 6], "{mode}: no paste, click or blank copy");
        assert_eq!(copies[0].created_at, "2026-11-28T10:00:00.123Z", "{mode}");
        assert_eq!(copies[0].content, "fn a() {}", "{mode}");
        assert_eq!(copies[2].document_path.as_deref(), Some("/src/main.rs"));
        assert!(copies.iter().all(|c| c.ready), "{mode}");
        let later = db.clipboard_copies_after(1, 1, false).await.unwrap();
        assert_eq!(later.iter().map(|c| c.id).collect::<Vec<_>>(), [3]);
        // Not yet PII-redacted: not ready while the worker waits for it.
        assert!(db
            .clipboard_copies_after(0, 100, true)
            .await
            .unwrap()
            .iter()
            .all(|c| !c.ready));

        let new: Vec<NewSnippet> = copies.iter().map(new_snippet).collect();
        let (added, _) = db.insert_snippets(&new).await.unwrap();
        assert_eq!(added, 3, "{mode}");
        assert_eq!(
            db.insert_snippets(&new).await.unwrap().0,
            0,
            "{mode}: deduped"
        );

        let all = db
            .list_snippets(&SnippetFilter {
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            all.iter().map(|s| s.ui_event_id).collect::<Vec<_>>(),
            [Some(6), Some(3), Some(1)],
            "{mode}: newest first"
        );
        let first = all.last().unwrap().clone();
        assert_eq!(first.tags, Vec::<String>::new());

        // The frame linker links event 1 after its snippet was saved.
        db.execute_raw_sql_write("UPDATE ui_events SET frame_id = 1 WHERE id = 1")
            .await
            .unwrap();
        let linked = db
            .link_snippet_frames("2026-11-28T00:00:00.000Z", 100, |_, path, _| {
                path.map(|_| "rust".to_string())
            })
            .await
            .unwrap();
        assert_eq!(linked, 1, "{mode}");
        let first = db.get_snippet(first.id).await.unwrap().unwrap();
        assert_eq!(first.frame_id, Some(1));
        assert_eq!(first.document_path.as_deref(), Some("/src/main.rs"));
        assert_eq!(first.language.as_deref(), Some("rust"));
        let again = db
            .link_snippet_frames("2026-11-28T00:00:00.000Z", 100, |_, _, _| None)
            .await
            .unwrap();
        assert_eq!(again, 0, "{mode}");

        let filtered = |f: SnippetFilter| async move {
            db.list_snippets(&SnippetFilter { limit: 50, ..f })
                .await
                .unwrap()
                .into_iter()
                .map(|s| s.ui_event_id.unwrap())
                .collect::<Vec<_>>()
        };
        let q = |q: &str| SnippetFilter {
            q: Some(q.into()),
            ..Default::default()
        };
        assert_eq!(filtered(q("LET B")).await, [3], "{mode}");
        assert_eq!(
            filtered(q("%")).await,
            Vec::<i64>::new(),
            "{mode}: literal %"
        );
        let by_app = SnippetFilter {
            app_name: Some("terminal".into()),
            ..Default::default()
        };
        assert_eq!(filtered(by_app).await, [6], "{mode}");
        let by_lang = SnippetFilter {
            language: Some("Rust".into()),
            ..Default::default()
        };
        assert_eq!(filtered(by_lang).await, [1], "{mode}");
        let window = SnippetFilter {
            start: Some("2026-11-28T10:00:01.000Z".into()),
            end: Some("2026-11-28T10:00:03.000Z".into()),
            ..Default::default()
        };
        assert_eq!(filtered(window).await, [3], "{mode}");
        let page = SnippetFilter {
            limit: 1,
            offset: 1,
            ..Default::default()
        };
        assert_eq!(
            db.list_snippets(&page).await.unwrap()[0].ui_event_id,
            Some(3)
        );

        // Enrichment: set fields; unenriched listing leaves it out.
        let update = SnippetUpdate {
            title: Some("Answer".into()),
            tags: Some(vec!["sql".into()]),
            ..Default::default()
        };
        let six = all[0].id;
        let updated = db.update_snippet(six, &update).await.unwrap().unwrap();
        assert_eq!(updated.title.as_deref(), Some("Answer"));
        assert_eq!(updated.tags, ["sql"]);
        assert!(updated.enriched_at.is_some());
        let unenriched = SnippetFilter {
            unenriched: true,
            ..Default::default()
        };
        assert_eq!(filtered(unenriched).await, [3, 1], "{mode}");
        assert!(db.update_snippet(9999, &update).await.unwrap().is_none());

        assert!(db.delete_snippet(six).await.unwrap());
        assert!(!db.delete_snippet(six).await.unwrap());
        assert!(db.get_snippet(six).await.unwrap().is_none());

        // Retention deletes the clipboard events; the snippets stay, unlinked.
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let result = db
            .delete_retention_time_slice(
                at("2026-11-28T09:00:00Z"),
                at("2026-11-28T09:00:00Z"),
                at("2026-11-28T10:00:03Z"),
            )
            .await
            .unwrap();
        assert!(result.ui_events_deleted >= 1, "{mode}");
        let cut = db
            .list_snippets(&SnippetFilter {
                limit: 50,
                ..q("let b")
            })
            .await
            .unwrap()
            .remove(0);
        assert_eq!(cut.ui_event_id, None, "{mode}: unlinked from event 3");
        let kept = db
            .list_snippets(&SnippetFilter {
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(kept.len(), 2, "{mode}: retention keeps snippets");

        // A user's delete of a time range deletes the snippets copied in it.
        db.delete_time_range(at("2026-11-28T10:00:02Z"), at("2026-11-28T10:00:03Z"))
            .await
            .unwrap();
        let left = db
            .list_snippets(&SnippetFilter {
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            left.iter().map(|s| s.content.as_str()).collect::<Vec<_>>(),
            ["fn a() {}"],
            "{mode}: range delete"
        );
    }

    #[tokio::test]
    async fn snippets_round_trip_in_sqlite_storage() {
        let db = DatabaseManager::new("sqlite::memory:", Default::default())
            .await
            .unwrap();
        db.execute_raw_sql_write(SEED).await.unwrap();
        exercise(&db, "sqlite").await;
    }

    /// Sealed clipboard text is read back through the logical view.
    #[tokio::test]
    async fn snippets_read_sealed_clipboard_rows_in_compressed_storage() {
        let root = tempfile::tempdir().unwrap();
        let db = DatabaseManager::new_hybrid(root.path(), Default::default(), Default::default())
            .await
            .unwrap();
        db.execute_raw_sql_write(SEED).await.unwrap();
        while db.seal_payloads().await.unwrap() != 0 {}
        exercise(&db, "compressed").await;
        db.close().await;
    }

    /// With text privacy release on, a copy is ready only once its input
    /// receipt is certified, and then with the released text.
    #[tokio::test]
    async fn snippets_wait_for_privacy_release() {
        use crate::privacy_release::ReleasePolicy;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("db.sqlite");
        let db = DatabaseManager::new(path.to_str().unwrap(), Default::default())
            .await
            .unwrap();
        // Saved while release was off, from event 2.
        db.execute_raw_sql_write(
            "INSERT INTO ui_events(id,timestamp,event_type,text_content) \
             VALUES (2,'2026-11-28T09:00:00Z','clipboard','fn early() {}'); \
             INSERT INTO snippets(id,created_at,content,content_hash,ui_event_id) \
             VALUES (1,'2026-11-28T09:00:00.000Z','fn early() {}','early',2);",
        )
        .await
        .unwrap();
        let all = SnippetFilter {
            limit: 50,
            ..Default::default()
        };
        assert_eq!(db.list_snippets(&all).await.unwrap().len(), 1);
        db.configure_release(&ReleasePolicy {
            identity: "test:model-v1".into(),
            text: true,
            images: false,
        })
        .await
        .unwrap();
        assert!(
            db.list_snippets(&all).await.unwrap().is_empty(),
            "saved before release, event not released: hidden"
        );
        assert!(db.get_snippet(1).await.unwrap().is_none());
        db.execute_raw_sql_write(
            "INSERT INTO ui_events(id,timestamp,event_type,text_content,redacted_at) \
             VALUES (3,'2026-11-28T10:00:00Z','clipboard','key = \"alice@example.com\";',1)",
        )
        .await
        .unwrap();
        let copies = db.clipboard_copies_after(2, 10, false).await.unwrap();
        assert!(
            !copies[0].ready,
            "a redaction timestamp alone is not a release"
        );

        let job = db
            .release_pending("input", 10)
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.id == "3")
            .unwrap();
        let mut values = job.values.clone();
        values.insert("text_content".into(), Some("key = \"[EMAIL]\";".into()));
        assert!(db.publish_release(&job, &values).await.unwrap());
        let copies = db.clipboard_copies_after(2, 10, true).await.unwrap();
        assert!(copies[0].ready);
        assert!(copies[0].release_key.is_some());
        assert_eq!(copies[0].content, "key = \"[EMAIL]\";");

        // Saved under the current release: served; the early one still is not.
        db.insert_snippets(&[new_snippet(&copies[0])])
            .await
            .unwrap();
        let served = db.list_snippets(&all).await.unwrap();
        assert_eq!(
            served
                .iter()
                .map(|s| s.content.as_str())
                .collect::<Vec<_>>(),
            ["key = \"[EMAIL]\";"]
        );
        // Retention unlinks it; the receipt key it was saved under still serves it.
        db.execute_raw_sql_write("DELETE FROM ui_events WHERE id = 3")
            .await
            .unwrap();
        assert_eq!(db.list_snippets(&all).await.unwrap().len(), 1);
        db.close().await;
    }
}
