// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

//! Rules-derived `project:<name>` vision tags. The engine's project tag
//! worker decides what to tag and when; this module does the set-based writes
//! through the shared writer. Only window_name, browser_url and document_path
//! are matched: compressed storage seals the text payload columns but leaves
//! these in `frames`, so staged and sealed rows match the same way.

use super::*;

/// Every rules-derived tag carries this prefix; clearing never touches other tags.
const PROJECT_TAG_PREFIX: &str = "project:";

/// `%pat%` for a LIKE with `ESCAPE '\'`, so `%` and `_` in a pattern are literal.
fn contains_pattern(pattern: &str) -> String {
    let escaped = pattern
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

impl DatabaseManager {
    pub async fn max_frame_id(&self) -> Result<i64, SqlxError> {
        sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM frames")
            .fetch_one(&self.pool)
            .await
    }

    /// Remove up to `limit` project tag links; returns how many were removed.
    pub async fn clear_project_tags(&self, limit: i64) -> Result<u64, SqlxError> {
        let mut tx = self.begin_immediate_with_retry().await?;
        // GLOB, not LIKE: LIKE is case-insensitive and would also match a
        // user's own "Project:..." tag.
        let removed = sqlx::query(
            "DELETE FROM vision_tags WHERE rowid IN (
                SELECT vt.rowid FROM vision_tags vt JOIN tags t ON t.id = vt.tag_id
                WHERE t.name GLOB 'project:*' LIMIT ?)",
        )
        .bind(limit)
        .execute(&mut **tx.conn())
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(removed)
    }

    /// Tag frames `from..=to` with `project:<name>` for every project having a
    /// pattern that is a case-insensitive substring of the frame's window
    /// title, browser URL or document path. Idempotent; one transaction.
    pub async fn tag_project_frames(
        &self,
        projects: &[(String, Vec<String>)],
        from: i64,
        to: i64,
    ) -> Result<u64, SqlxError> {
        const MATCH: &str = "(COALESCE(window_name,'') LIKE ? ESCAPE '\\' \
             OR COALESCE(browser_url,'') LIKE ? ESCAPE '\\' \
             OR COALESCE(document_path,'') LIKE ? ESCAPE '\\')";
        let mut tx = self.begin_immediate_with_retry().await?;
        let mut tagged = 0;
        for (name, patterns) in projects.iter().filter(|(_, p)| !p.is_empty()) {
            let tag_id: i64 = sqlx::query_scalar(
                "INSERT INTO tags (name) VALUES (?) ON CONFLICT(name) DO UPDATE SET name=name RETURNING id",
            )
            .bind(format!("{PROJECT_TAG_PREFIX}{name}"))
            .fetch_one(&mut **tx.conn())
            .await?;
            let sql = format!(
                "INSERT OR IGNORE INTO vision_tags (vision_id, tag_id) \
                 SELECT id, ? FROM frames WHERE id BETWEEN ? AND ? AND ({})",
                vec![MATCH; patterns.len()].join(" OR ")
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(tag_id)
                .bind(from)
                .bind(to);
            for pattern in patterns {
                let like = contains_pattern(pattern);
                query = query.bind(like.clone()).bind(like.clone()).bind(like);
            }
            tagged += query.execute(&mut **tx.conn()).await?.rows_affected();
        }
        tx.commit().await?;
        Ok(tagged)
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
    /// 6: a literal "100%_done" title; 7: "100xydone" must not match it.
    const SEED: &str =
        "INSERT INTO frames(id,timestamp,full_text,window_name,browser_url,document_path) VALUES \
        (1,'2026-11-26T12:00:00Z','a','DT POS - Tax',NULL,NULL), \
        (2,'2026-11-26T12:00:01Z','b','Chrome','https://Jira.example/CANOPY-1',NULL), \
        (3,'2026-11-26T12:00:02Z','c','editor',NULL,'C:/src/ZPos/main.clw'), \
        (4,'2026-11-26T12:00:03Z','d','dtpos vs canopy',NULL,NULL), \
        (5,'2026-11-26T12:00:04Z','e','Slack',NULL,NULL), \
        (6,'2026-11-26T12:00:05Z','f','report 100%_done',NULL,NULL), \
        (7,'2026-11-26T12:00:06Z','g','report 100xydone',NULL,NULL);";

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
        ]);
        // A user's own tag, even one that looks like a project tag in another case.
        db.add_tags(
            5,
            TagContentType::Vision,
            vec!["Project:mine".into(), "todo".into()],
        )
        .await
        .unwrap();

        // Two id-range batches cover everything; rerunning adds nothing.
        assert_eq!(db.max_frame_id().await.unwrap(), 7);
        let a = db.tag_project_frames(&first, 1, 3).await.unwrap();
        let b = db.tag_project_frames(&first, 4, 7).await.unwrap();
        assert_eq!(a + b, 6, "{mode}");
        assert_eq!(
            db.tag_project_frames(&first, 1, 7).await.unwrap(),
            0,
            "{mode}"
        );
        let tagged = expected(&[
            (1, "project:dtpos"),
            (2, "project:canopy"),
            (3, "project:dtpos"),
            (4, "project:canopy"),
            (4, "project:dtpos"),
            (5, "Project:mine"),
            (5, "todo"),
            (6, "project:done"),
        ]);
        assert_eq!(tags_of(db).await, tagged, "{mode}");

        // Rules change: clearing in small batches removes only project tags.
        while db.clear_project_tags(2).await.unwrap() != 0 {}
        assert_eq!(
            tags_of(db).await,
            expected(&[(5, "Project:mine"), (5, "todo")]),
            "{mode}"
        );
        let second = rules(&[("slack", &["slack"])]);
        db.tag_project_frames(&second, 1, 7).await.unwrap();
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
        assert_eq!(sealed, 7);
        exercise(&db, "compressed").await;
        db.close().await;
    }
}
