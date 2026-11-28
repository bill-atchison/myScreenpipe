// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

//! Rules-derived `project:<name>` vision tags. The engine's project tag
//! worker decides what to tag and when; this module reads frames on the read
//! pool, matches in Rust, and holds the shared writer only for the inserts.
//! Only window_name, browser_url and document_path are matched: compressed
//! storage seals the text payload columns but leaves these in `frames`, so
//! staged and sealed rows match the same way.

use super::*;
use std::time::{Duration, Instant};

/// Every rules-derived tag carries this prefix; clearing never touches other tags.
const PROJECT_TAG_PREFIX: &str = "project:";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProjectTagBatch {
    pub scanned: u64,
    /// Tag links added (links already present are not counted).
    pub tagged: u64,
    /// Highest frame id read; `after` when nothing was read.
    pub last_id: i64,
    /// Time the shared writer was held.
    pub writer_hold: Duration,
}

impl DatabaseManager {
    pub async fn max_frame_id(&self) -> Result<i64, SqlxError> {
        sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM frames")
            .fetch_one(&self.pool)
            .await
    }

    /// Remove up to `limit` project tag links; returns how many were removed
    /// and how long the writer was held.
    pub async fn clear_project_tags(&self, limit: i64) -> Result<(u64, Duration), SqlxError> {
        let mut tx = self.begin_immediate_with_retry().await?;
        let held = Instant::now();
        // GLOB, not LIKE: LIKE is case-insensitive and would also match a
        // user's own "Project:..." tag.
        let removed = sqlx::query(
            "DELETE FROM vision_tags WHERE rowid IN (
                SELECT rowid FROM vision_tags
                WHERE tag_id IN (SELECT id FROM tags WHERE name GLOB 'project:*') LIMIT ?)",
        )
        .bind(limit)
        .execute(&mut **tx.conn())
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok((removed, held.elapsed()))
    }

    /// Tag up to `limit` frames after id `after` with `project:<name>` for
    /// every project having a pattern that is a case-insensitive substring of
    /// the frame's window title, browser URL or document path. Idempotent.
    /// Frames are read and matched without the writer; it is held only for
    /// the inserts.
    pub async fn tag_project_frames(
        &self,
        projects: &[(String, Vec<String>)],
        after: i64,
        limit: i64,
    ) -> Result<ProjectTagBatch, SqlxError> {
        let rows: Vec<(i64, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT id, window_name, browser_url, document_path FROM frames \
             WHERE id > ? ORDER BY id LIMIT ?",
        )
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut batch = ProjectTagBatch {
            scanned: rows.len() as u64,
            last_id: rows.last().map_or(after, |r| r.0),
            ..Default::default()
        };
        // to_lowercase on both sides also folds non-ASCII case, which SQLite LIKE does not.
        let lowered: Vec<(String, Vec<String>)> = projects
            .iter()
            .map(|(name, patterns)| {
                let patterns = patterns.iter().map(|p| p.to_lowercase()).collect();
                (name.clone(), patterns)
            })
            .collect();
        let mut links: Vec<(i64, &str)> = Vec::new();
        for (id, window, url, doc) in rows {
            let fields: Vec<String> = [window, url, doc]
                .iter()
                .flatten()
                .map(|f| f.to_lowercase())
                .collect();
            let matches = lowered.iter().filter(|(_, patterns)| {
                patterns
                    .iter()
                    .any(|p| fields.iter().any(|f| f.contains(p.as_str())))
            });
            links.extend(matches.map(|(name, _)| (id, name.as_str())));
        }
        if links.is_empty() {
            return Ok(batch);
        }
        let mut names: Vec<&str> = links.iter().map(|(_, n)| *n).collect();
        names.sort_unstable();
        names.dedup();

        let mut tx = self.begin_immediate_with_retry().await?;
        let held = Instant::now();
        let mut tag_ids = std::collections::HashMap::new();
        for name in names {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO tags (name) VALUES (?) ON CONFLICT(name) DO UPDATE SET name=name RETURNING id",
            )
            .bind(format!("{PROJECT_TAG_PREFIX}{name}"))
            .fetch_one(&mut **tx.conn())
            .await?;
            tag_ids.insert(name, id);
        }
        let pairs: Vec<[i64; 2]> = links.iter().map(|(f, n)| [*f, tag_ids[n]]).collect();
        batch.tagged = sqlx::query(
            "INSERT OR IGNORE INTO vision_tags (vision_id, tag_id) \
             SELECT json_extract(value, '$[0]'), json_extract(value, '$[1]') FROM json_each(?)",
        )
        .bind(serde_json::to_string(&pairs).expect("pairs serialize"))
        .execute(&mut **tx.conn())
        .await?
        .rows_affected();
        tx.commit().await?;
        batch.writer_hold = held.elapsed();
        Ok(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(spec: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        spec.iter()
            .map(|(n, p)| (n.to_string(), p.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    /// 1: window title; 2: URL; 3: document path; 4: two projects; 5: none;
    /// 6: a literal "100%_done" title; 7: "100xydone" must not match it;
    /// 8: non-ASCII case folding.
    const SEED: &str =
        "INSERT INTO frames(id,timestamp,full_text,window_name,browser_url,document_path) VALUES \
        (1,'2026-11-26T12:00:00Z','a','DT POS - Tax',NULL,NULL), \
        (2,'2026-11-26T12:00:01Z','b','Chrome','https://Jira.example/CANOPY-1',NULL), \
        (3,'2026-11-26T12:00:02Z','c','editor',NULL,'C:/src/ZPos/main.clw'), \
        (4,'2026-11-26T12:00:03Z','d','dtpos vs canopy',NULL,NULL), \
        (5,'2026-11-26T12:00:04Z','e','Slack',NULL,NULL), \
        (6,'2026-11-26T12:00:05Z','f','report 100%_done',NULL,NULL), \
        (7,'2026-11-26T12:00:06Z','g','report 100xydone',NULL,NULL), \
        (8,'2026-11-26T12:00:07Z','h','ÜBERSICHT board',NULL,NULL);";

    async fn tags_of(db: &DatabaseManager) -> Vec<(i64, String)> {
        sqlx::query_as(
            "SELECT vt.vision_id, t.name FROM vision_tags vt JOIN tags t ON t.id=vt.tag_id \
             ORDER BY vt.vision_id, t.name",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap()
    }

    fn expected(rows: &[(i64, &str)]) -> Vec<(i64, String)> {
        rows.iter().map(|(i, n)| (*i, n.to_string())).collect()
    }

    async fn exercise(db: &DatabaseManager, mode: &str) {
        let first = rules(&[
            ("dtpos", &["dt pos", "DTPOS", "zpos"]),
            ("canopy", &["canopy"]),
            ("done", &["100%_done"]),
            ("uber", &["übersicht"]),
        ]);
        // A user's own tag, even one that looks like a project tag in another case.
        db.add_tags(
            5,
            TagContentType::Vision,
            vec!["Project:mine".into(), "todo".into()],
        )
        .await
        .unwrap();

        // Two batches cover everything; rerunning adds nothing.
        assert_eq!(db.max_frame_id().await.unwrap(), 8);
        let a = db.tag_project_frames(&first, 0, 3).await.unwrap();
        assert_eq!((a.scanned, a.last_id), (3, 3), "{mode}");
        let b = db.tag_project_frames(&first, a.last_id, 100).await.unwrap();
        assert_eq!((b.scanned, b.last_id), (5, 8), "{mode}");
        assert_eq!(a.tagged + b.tagged, 7, "{mode}");
        let again = db.tag_project_frames(&first, 0, 100).await.unwrap();
        assert_eq!((again.scanned, again.tagged), (8, 0), "{mode}");
        let past_end = db.tag_project_frames(&first, 8, 100).await.unwrap();
        assert_eq!((past_end.scanned, past_end.last_id), (0, 8), "{mode}");
        let tagged = expected(&[
            (1, "project:dtpos"),
            (2, "project:canopy"),
            (3, "project:dtpos"),
            (4, "project:canopy"),
            (4, "project:dtpos"),
            (5, "Project:mine"),
            (5, "todo"),
            (6, "project:done"),
            (8, "project:uber"),
        ]);
        assert_eq!(tags_of(db).await, tagged, "{mode}");

        // Rules change: clearing in small batches removes only project tags.
        while db.clear_project_tags(2).await.unwrap().0 != 0 {}
        assert_eq!(
            tags_of(db).await,
            expected(&[(5, "Project:mine"), (5, "todo")]),
            "{mode}"
        );
        let second = rules(&[("slack", &["slack"])]);
        db.tag_project_frames(&second, 0, 100).await.unwrap();
        assert_eq!(
            tags_of(db).await,
            expected(&[(5, "Project:mine"), (5, "project:slack"), (5, "todo")]),
            "{mode}"
        );
    }

    #[tokio::test]
    async fn tags_frames_by_rules_in_sqlite_storage() {
        let db = DatabaseManager::new("sqlite::memory:", Default::default())
            .await
            .unwrap();
        db.execute_raw_sql_write(SEED).await.unwrap();
        exercise(&db, "sqlite").await;
    }

    /// Sealed rows keep window_name/browser_url/document_path in `frames`.
    #[tokio::test]
    async fn tags_sealed_frames_in_compressed_storage() {
        let root = tempfile::tempdir().unwrap();
        let db = DatabaseManager::new_hybrid(root.path(), Default::default(), Default::default())
            .await
            .unwrap();
        db.execute_raw_sql_write(SEED).await.unwrap();
        while db.seal_payloads().await.unwrap() != 0 {}
        let sealed: i64 =
            sqlx::query_scalar("SELECT count(*) FROM frame_payloads WHERE state='sealed'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(sealed, 8);
        exercise(&db, "compressed").await;
        db.close().await;
    }
}
