//! Timeline: searchable history of what moved between PC and phone.
//!
//! Stores history in SQLite at `$XDG_DATA_HOME/smartconnect-linux/timeline.db` (mode 0600).
//! Tracks:
//! - File transfers (direction, filename, staged/destination path)
//! - Clipboard items (text preview, image indicators)
//! - Phone notifications (app, title, body)
//! - Sessions (connect / disconnect)
//!
//! Provides FTS5 full-text search (with LIKE fallback) and retention pruning (default 30 days).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

const TIMELINE_DB: &str = "timeline.db";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TimelineItem {
    pub id: i64,
    pub ts: i64,
    pub device: String,
    pub kind: String, // "file", "clipboard", "notification", "session"
    pub title: String,
    pub detail: String,
    pub path: Option<String>,
    pub direction: String, // "incoming", "outgoing"
}

pub struct TimelineStore {
    conn: Mutex<Connection>,
    pub db_path: PathBuf,
}

impl TimelineStore {
    pub fn open(dir: &Path) -> Result<Self> {
        let db_path = dir.join(TIMELINE_DB);

        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            if !db_path.exists() {
                let _ = fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&db_path);
            }
            let _ = fs::set_permissions(&db_path, fs::Permissions::from_mode(0o600));
        }

        let conn = Connection::open(&db_path)
            .with_context(|| format!("opening timeline database at {}", db_path.display()))?;

        // Initialize schema
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS timeline (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                device TEXT NOT NULL,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                detail TEXT NOT NULL,
                path TEXT,
                direction TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_timeline_ts ON timeline(ts);
            CREATE INDEX IF NOT EXISTS idx_timeline_kind ON timeline(kind);
            ",
        )?;

        // Try initializing FTS5 table
        let _ = conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS timeline_fts USING fts5(
                title,
                detail,
                content='timeline',
                content_rowid='id'
            );
            CREATE TRIGGER IF NOT EXISTS timeline_ai AFTER INSERT ON timeline BEGIN
                INSERT INTO timeline_fts(rowid, title, detail) VALUES (new.id, new.title, new.detail);
            END;
            CREATE TRIGGER IF NOT EXISTS timeline_ad AFTER DELETE ON timeline BEGIN
                INSERT INTO timeline_fts(timeline_fts, rowid, title, detail) VALUES('delete', old.id, old.title, old.detail);
            END;
            ",
        );

        Ok(Self {
            conn: Mutex::new(conn),
            db_path,
        })
    }

    /// Appends a new event into the timeline.
    pub fn append(
        &self,
        device: &str,
        kind: &str,
        title: &str,
        detail: &str,
        path: Option<&str>,
        direction: &str,
    ) -> Result<i64> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO timeline (ts, device, kind, title, detail, path, direction)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![ts, device, kind, title, detail, path, direction],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Lists recent timeline entries with optional filtering by kind.
    pub fn list(
        &self,
        limit: usize,
        offset: usize,
        kind_filter: Option<&str>,
    ) -> Result<Vec<TimelineItem>> {
        let conn = self.conn.lock().unwrap();
        let mut items = Vec::new();

        if let Some(kind) = kind_filter {
            let mut stmt = conn.prepare(
                "SELECT id, ts, device, kind, title, detail, path, direction
                 FROM timeline
                 WHERE kind = ?1
                 ORDER BY ts DESC, id DESC
                 LIMIT ?2 OFFSET ?3",
            )?;
            let rows = stmt.query_map(params![kind, limit as i64, offset as i64], |row| {
                Ok(TimelineItem {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    device: row.get(2)?,
                    kind: row.get(3)?,
                    title: row.get(4)?,
                    detail: row.get(5)?,
                    path: row.get(6)?,
                    direction: row.get(7)?,
                })
            })?;
            for row in rows {
                items.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, ts, device, kind, title, detail, path, direction
                 FROM timeline
                 ORDER BY ts DESC, id DESC
                 LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt.query_map(params![limit as i64, offset as i64], |row| {
                Ok(TimelineItem {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    device: row.get(2)?,
                    kind: row.get(3)?,
                    title: row.get(4)?,
                    detail: row.get(5)?,
                    path: row.get(6)?,
                    direction: row.get(7)?,
                })
            })?;
            for row in rows {
                items.push(row?);
            }
        }

        Ok(items)
    }

    /// Full-text search across titles and details.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<TimelineItem>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return self.list(limit, 0, None);
        }

        let conn = self.conn.lock().unwrap();
        let mut items = Vec::new();

        // Try FTS5 first
        let fts_result = (|| -> Result<Vec<TimelineItem>> {
            let mut stmt = conn.prepare(
                "SELECT t.id, t.ts, t.device, t.kind, t.title, t.detail, t.path, t.direction
                 FROM timeline_fts f
                 JOIN timeline t ON f.rowid = t.id
                 WHERE timeline_fts MATCH ?1
                 ORDER BY t.ts DESC, t.id DESC
                 LIMIT ?2",
            )?;
            // Sanitize query for FTS5 (quote word tokens)
            let fts_query = trimmed
                .split_whitespace()
                .map(|w| format!("\"{}\"", w.replace('\"', "")))
                .collect::<Vec<_>>()
                .join(" ");
            let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
                Ok(TimelineItem {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    device: row.get(2)?,
                    kind: row.get(3)?,
                    title: row.get(4)?,
                    detail: row.get(5)?,
                    path: row.get(6)?,
                    direction: row.get(7)?,
                })
            })?;
            let mut res = Vec::new();
            for r in rows {
                res.push(r?);
            }
            Ok(res)
        })();

        if let Ok(res) = fts_result {
            return Ok(res);
        }

        // Fallback to LIKE query if FTS5 fails or is unavailable
        let pattern = format!("%{trimmed}%");
        let mut stmt = conn.prepare(
            "SELECT id, ts, device, kind, title, detail, path, direction
             FROM timeline
             WHERE title LIKE ?1 OR detail LIKE ?1
             ORDER BY ts DESC, id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            Ok(TimelineItem {
                id: row.get(0)?,
                ts: row.get(1)?,
                device: row.get(2)?,
                kind: row.get(3)?,
                title: row.get(4)?,
                detail: row.get(5)?,
                path: row.get(6)?,
                direction: row.get(7)?,
            })
        })?;
        for r in rows {
            items.push(r?);
        }

        Ok(items)
    }

    /// Prunes entries older than `days` retention period.
    pub fn prune(&self, days: u32) -> Result<usize> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let cutoff = now.saturating_sub((days as i64) * 86400);

        let conn = self.conn.lock().unwrap();
        let count = conn.execute("DELETE FROM timeline WHERE ts < ?1", params![cutoff])?;
        Ok(count)
    }

    /// Clears all timeline history.
    pub fn clear(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM timeline", [])?;
        let _ = conn.execute("DELETE FROM timeline_fts", []);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeline_append_search_prune() {
        let tmp = std::env::temp_dir().join(format!("sc-tl-test-{}", rand::random::<u64>()));
        let _ = fs::create_dir_all(&tmp);

        let store = TimelineStore::open(&tmp).unwrap();

        // Check file mode 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = fs::metadata(tmp.join(TIMELINE_DB)).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }

        // Append items
        store
            .append(
                "phone-1",
                "file",
                "report.pdf",
                "1.2 MB transfer completed",
                Some("/home/user/Downloads/report.pdf"),
                "incoming",
            )
            .unwrap();

        store
            .append(
                "phone-1",
                "clipboard",
                "https://example.com/docs",
                "URL copied on phone",
                None,
                "incoming",
            )
            .unwrap();

        store
            .append(
                "phone-1",
                "session",
                "Connected",
                "Connected via Wi-Fi (192.168.1.50)",
                None,
                "incoming",
            )
            .unwrap();

        // List
        let list = store.list(10, 0, None).unwrap();
        assert_eq!(list.len(), 3);

        let files = store.list(10, 0, Some("file")).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].title, "report.pdf");

        // Search "pdf"
        let search_pdf = store.search("pdf", 10).unwrap();
        assert_eq!(search_pdf.len(), 1);
        assert_eq!(search_pdf[0].title, "report.pdf");

        // Search "example.com"
        let search_clip = store.search("example.com", 10).unwrap();
        assert_eq!(search_clip.len(), 1);

        // Pruning with 0 days should prune everything with past ts
        // But entries were just added with now, so cutoff = now - 1 day preserves them
        let pruned = store.prune(1).unwrap();
        assert_eq!(pruned, 0);

        // Clear
        store.clear().unwrap();
        assert_eq!(store.list(10, 0, None).unwrap().len(), 0);

        let _ = fs::remove_dir_all(&tmp);
    }
}
