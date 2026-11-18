use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{config::private_file, protocol::Source};

#[derive(Clone, Deserialize, Serialize)]
pub struct Download {
    pub id: String,
    pub path: String,
    pub state: String,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub file_version: Option<String>,
    pub attempts: u32,
    pub queue_order: i64,
    pub retry_at: Option<i64>,
    pub failure_kind: Option<String>,
    #[serde(skip)]
    pub source: Source,
}

pub struct Store {
    pub(crate) db: Mutex<Connection>,
    cache: PathBuf,
}

#[derive(Clone, Serialize)]
pub struct Segment {
    pub id: String,
    pub start: u64,
    pub length: u64,
    pub total: u64,
    pub file_version: String,
    pub checksum: String,
    pub content_type: String,
}

impl Store {
    pub fn open(root: &Path, upstream: &str) -> Result<Self> {
        let path = root.join("state.sqlite3");
        let db = Connection::open(&path)?;
        private_file(&path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS downloads (
                id TEXT PRIMARY KEY, path TEXT NOT NULL, source TEXT NOT NULL,
                state TEXT NOT NULL, downloaded INTEGER NOT NULL DEFAULT 0,
                total INTEGER, error TEXT, etag TEXT, last_modified TEXT, content_type TEXT,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE TABLE IF NOT EXISTS segments (
                id TEXT NOT NULL, start INTEGER NOT NULL, length INTEGER NOT NULL,
                total INTEGER NOT NULL, file_version TEXT NOT NULL, checksum TEXT NOT NULL,
                content_type TEXT NOT NULL, PRIMARY KEY(id,start)
            );",
        )?;
        let columns = db
            .prepare("PRAGMA table_info(downloads)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !columns.iter().any(|column| column == "file_version") {
            db.execute("ALTER TABLE downloads ADD COLUMN file_version TEXT", [])?;
        }
        if !columns.iter().any(|column| column == "cache_key") {
            db.execute("ALTER TABLE downloads ADD COLUMN cache_key TEXT", [])?;
        }
        for (name, kind) in [
            ("attempts", "INTEGER NOT NULL DEFAULT 0"),
            ("retry_at", "INTEGER"),
            ("failure_kind", "TEXT"),
            ("account_key", "TEXT"),
        ] {
            if !columns.iter().any(|column| column == name) {
                db.execute(
                    &format!("ALTER TABLE downloads ADD COLUMN {name} {kind}"),
                    [],
                )?;
            }
        }
        if !columns.iter().any(|column| column == "queue_order") {
            db.execute(
                "ALTER TABLE downloads ADD COLUMN queue_order INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
            db.execute("UPDATE downloads SET queue_order=rowid", [])?;
        }
        let missing: Vec<(String, String)> = db
            .prepare("SELECT id,source FROM downloads WHERE cache_key IS NULL")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, raw) in missing {
            let source: Source = serde_json::from_str(&raw)?;
            db.execute(
                "UPDATE downloads SET cache_key=?2 WHERE id=?1",
                params![id, source.cache_id()],
            )?;
        }
        db.execute(
            "CREATE INDEX IF NOT EXISTS downloads_cache_key ON downloads(cache_key)",
            [],
        )?;
        let saved: Option<String> = db
            .query_row("SELECT value FROM settings WHERE key='upstream'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if saved.as_deref().is_some_and(|old| old != upstream) {
            bail!("this data directory belongs to a different upstream; use another directory");
        }
        db.execute(
            "INSERT OR IGNORE INTO settings VALUES ('upstream', ?1)",
            [upstream],
        )?;
        db.execute(
            "UPDATE downloads SET state='queued', error=NULL WHERE state='downloading'",
            [],
        )?;
        let store = Self {
            db: Mutex::new(db),
            cache: root.join("cache"),
        };
        store.initialize_automation()?;
        for item in store.list()? {
            if item.state != "removed" || !store.prune_removed(&item)? {
                store.save_session(&item.source)?;
            }
        }
        Ok(store)
    }

    pub fn file(&self, id: &str, partial: bool) -> PathBuf {
        // Task IDs are service-generated hex strings, never caller-supplied paths.
        self.cache
            .join(format!("{id}.{}", if partial { "part" } else { "bin" }))
    }

    pub fn list(&self) -> Result<Vec<Download>> {
        let db = self.db.lock().unwrap();
        let mut statement = db.prepare("SELECT id,path,state,downloaded,total,error,etag,last_modified,content_type,source,file_version,attempts,retry_at,failure_kind,queue_order FROM downloads ORDER BY queue_order,id")?;
        let rows = statement.query_map([], Self::row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Download> {
        let raw: String = row.get(9)?;
        let source = serde_json::from_str(&raw).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(e))
        })?;
        Ok(Download {
            id: row.get(0)?,
            path: row.get(1)?,
            state: row.get(2)?,
            downloaded: row.get(3)?,
            total: row.get(4)?,
            error: row.get(5)?,
            etag: row.get(6)?,
            last_modified: row.get(7)?,
            content_type: row.get(8)?,
            file_version: row.get(10)?,
            attempts: row.get(11)?,
            queue_order: row.get(14)?,
            retry_at: row.get(12)?,
            failure_kind: row.get(13)?,
            source,
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<Download>> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT id,path,state,downloaded,total,error,etag,last_modified,content_type,source,file_version,attempts,retry_at,failure_kind,queue_order FROM downloads WHERE id=?1",
            [id], Self::row,
        ).optional()?)
    }

    pub fn enqueue(&self, source: &Source, paused: bool) -> Result<String> {
        let account = self.account_for_source(source)?;
        Self::enqueue_download(&self.db.lock().unwrap(), source, account.as_deref(), paused)
    }

    pub(crate) fn enqueue_download(
        db: &Connection,
        source: &Source,
        account: Option<&str>,
        paused: bool,
    ) -> Result<String> {
        let cache_key = source.cache_id();
        let existing: Option<String> = db.query_row("SELECT id FROM downloads WHERE cache_key=?1 ORDER BY state='complete' DESC,created_at DESC LIMIT 1", [&cache_key], |row| row.get(0)).optional()?;
        if let Some(id) = existing {
            db.execute(
                "UPDATE downloads SET source=?2,account_key=COALESCE(?3,account_key) WHERE id=?1 AND state NOT IN ('downloading','deleting')",
                params![id, serde_json::to_string(source)?,account],
            )?;
            return Ok(id);
        }
        // A new task must not be affected by stale commands or workers for a deleted task.
        let id = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        db.execute(
            "INSERT INTO downloads(id,path,source,state,cache_key,account_key,queue_order) VALUES (?1,?2,?3,?4,?5,?6,(SELECT COALESCE(MAX(queue_order),0)+1 FROM downloads))",
            params![id, source.display_path(), serde_json::to_string(source)?, if paused { "paused" } else { "queued" }, cache_key,account],
        )?;
        Ok(id)
    }

    pub fn get_for_source(&self, source: &Source) -> Result<Option<Download>> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT id,path,state,downloaded,total,error,etag,last_modified,content_type,source,file_version,attempts,retry_at,failure_kind,queue_order FROM downloads WHERE cache_key=?1 ORDER BY state='complete' DESC,created_at DESC LIMIT 1",
            [source.cache_id()], Self::row,
        ).optional()?)
    }

    pub fn next(&self) -> Result<Option<Download>> {
        let db = self.db.lock().unwrap();
        let item = db.query_row(
            "SELECT id,path,state,downloaded,total,error,etag,last_modified,content_type,source,file_version,attempts,retry_at,failure_kind,queue_order FROM downloads WHERE state='queued' OR (state='retry_wait' AND retry_at<=?1) ORDER BY queue_order,id LIMIT 1",
            [crate::scheduler::now()], Self::row,
        ).optional()?;
        if let Some(item) = &item {
            db.execute(
                "UPDATE downloads SET state='downloading',error=NULL,retry_at=NULL WHERE id=?1",
                [&item.id],
            )?;
        }
        Ok(item)
    }

    pub fn progress(&self, item: &Download) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE downloads SET downloaded=?2,total=?3,etag=?4,last_modified=?5,content_type=?6,file_version=?7 WHERE id=?1",
            params![item.id, item.downloaded, item.total, item.etag, item.last_modified, item.content_type, item.file_version],
        )?;
        Ok(())
    }

    pub fn transition(&self, id: &str, from: &str, to: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().execute(
            "UPDATE downloads SET state=?3,error=NULL,retry_at=NULL,attempts=CASE WHEN ?3='complete' THEN 0 ELSE attempts END,failure_kind=CASE WHEN ?3='complete' THEN NULL ELSE failure_kind END WHERE id=?1 AND state=?2",
            params![id,from,to],
        )? > 0)
    }

    pub fn command(&self, id: &str, resume: bool) -> Result<bool> {
        let sql = if resume {
            "UPDATE downloads SET state='queued',error=NULL,attempts=0,retry_at=NULL,failure_kind=NULL WHERE id=?1 AND state IN ('paused','failed','retry_wait','waiting_login','cancelled','removed')"
        } else {
            "UPDATE downloads SET state='paused',retry_at=NULL WHERE id=?1 AND state IN ('queued','downloading','waiting_space','retry_wait','waiting_login')"
        };
        let db = self.db.lock().unwrap();
        let changed = db.execute(sql, [id])? > 0;
        if changed && resume {
            db.execute(
                "UPDATE managed_media SET suppressed=0 WHERE download_id=?1",
                [id],
            )?;
        }
        Ok(changed)
    }

    pub fn occupied_except(&self, id: &str) -> Result<u64> {
        let mut total = 0u64;
        for entry in std::fs::read_dir(&self.cache)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == format!("{id}.part") || name == format!("{id}.bin") {
                continue;
            }
            if entry.file_type()?.is_file() {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
        Ok(total)
    }

    pub fn segment_file(&self, segment: &Segment) -> PathBuf {
        self.cache.join(format!(
            "{}-{}-{}.seg",
            segment.id, segment.start, segment.checksum
        ))
    }

    pub fn save_segments(&self, segments: &[Segment]) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let transaction = db.transaction()?;
        if let Some(first) = segments.first() {
            transaction.execute("DELETE FROM segments WHERE id=?1 OR id IN (SELECT old_key FROM cache_aliases WHERE current_key=?1)", [&first.id])?;
        }
        for segment in segments {
            transaction.execute("INSERT INTO segments(id,start,length,total,file_version,checksum,content_type) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![segment.id,segment.start,segment.length,segment.total,segment.file_version,segment.checksum,segment.content_type])?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn segments(&self, id: &str) -> Result<Vec<Segment>> {
        let db = self.db.lock().unwrap();
        let mut statement = db.prepare("SELECT id,start,length,total,file_version,checksum,content_type FROM segments WHERE id=?1 OR id IN (SELECT old_key FROM cache_aliases WHERE current_key=?1) ORDER BY start,rowid DESC")?;
        Ok(statement
            .query_map([id], |row| {
                Ok(Segment {
                    id: row.get(0)?,
                    start: row.get(1)?,
                    length: row.get(2)?,
                    total: row.get(3)?,
                    file_version: row.get(4)?,
                    checksum: row.get(5)?,
                    content_type: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn cancel(&self, id: &str) -> Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let changed = tx.execute("UPDATE downloads SET state='cancelled',error=NULL,retry_at=NULL WHERE id=?1 AND state IN ('queued','downloading','paused','failed','retry_wait','waiting_login','waiting_space')", [id])?;
        if changed > 0 {
            tx.execute(
                "UPDATE managed_media SET suppressed=1 WHERE download_id=?1",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }
    pub fn request_delete(&self, id: &str) -> Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let changed = tx.execute("UPDATE downloads SET state='deleting',error=NULL,retry_at=NULL WHERE id=?1 AND state!='removed'", [id])?;
        if changed > 0 {
            tx.execute(
                "UPDATE managed_media SET suppressed=1 WHERE download_id=?1",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }
    pub fn reorder(&self, id: &str, direction: &str) -> Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut ids: Vec<String> = tx.prepare("SELECT id FROM downloads WHERE state IN ('queued','paused','failed','retry_wait','waiting_login','waiting_space') ORDER BY queue_order,id")?
            .query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let Some(index) = ids.iter().position(|value| value == id) else {
            return Ok(false);
        };
        let destination = match direction {
            "up" => index.saturating_sub(1),
            "down" => (index + 1).min(ids.len() - 1),
            "first" => 0,
            _ => bail!("invalid queue direction"),
        };
        let moved = ids.remove(index);
        ids.insert(destination, moved);
        for (position, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE downloads SET queue_order=?2 WHERE id=?1",
                params![id, position as i64],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }
}
