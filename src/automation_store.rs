//! Durable catalog bindings, playback work and local cache retention state.
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;

use crate::{
    library::{MediaItem, Session},
    protocol::Source,
    scheduler::{Policy, now},
    store::Store,
};

type StoredManagedRow = (String, String, String, String, Option<i64>, bool, bool);
type StoredSessionRow = (String, String, Option<String>, i64, Option<String>);

#[derive(Clone, Serialize)]
pub struct ManagedMedia {
    pub session_id: String,
    pub item: MediaItem,
    pub media_guid: String,
    pub download_id: String,
    pub completed_at: Option<i64>,
    pub keep: bool,
    pub suppressed: bool,
}

pub struct PlaybackWork {
    pub session_id: String,
    pub item_guid: String,
    pub seen_at: i64,
}

impl Store {
    pub fn initialize_automation(&self) -> Result<()> {
        self.db.lock().unwrap().execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, updated_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS managed_media (
                session_id TEXT NOT NULL, item_guid TEXT NOT NULL, item_json TEXT NOT NULL,
                media_guid TEXT NOT NULL, download_id TEXT NOT NULL, completed_at INTEGER,
                manual_completed INTEGER, upstream_watched INTEGER, keep INTEGER NOT NULL DEFAULT 0,
                suppressed INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(session_id,item_guid));
             CREATE INDEX IF NOT EXISTS managed_download ON managed_media(download_id);
             CREATE TABLE IF NOT EXISTS prefetch_suppressions (
                session_id TEXT NOT NULL, item_guid TEXT NOT NULL, PRIMARY KEY(session_id,item_guid));
             CREATE TABLE IF NOT EXISTS playback_work (session_id TEXT NOT NULL, item_guid TEXT NOT NULL,
                seen_at INTEGER NOT NULL, PRIMARY KEY(session_id,item_guid));"
        )?;
        let db = self.db.lock().unwrap();
        let columns = db
            .prepare("PRAGMA table_info(sessions)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (name, kind) in [
            ("account_key", "TEXT"),
            ("checked_at", "INTEGER NOT NULL DEFAULT 0"),
            ("error", "TEXT"),
        ] {
            if !columns.iter().any(|column| column == name) {
                db.execute(
                    &format!("ALTER TABLE sessions ADD COLUMN {name} {kind}"),
                    [],
                )?;
            }
        }
        db.execute_batch("CREATE TABLE IF NOT EXISTS cache_aliases(old_key TEXT PRIMARY KEY,current_key TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS cache_alias_current ON cache_aliases(current_key);")?;
        crate::local_history::initialize(&db)?;
        Ok(())
    }

    pub fn save_session(&self, source: &Source) -> Result<Option<Session>> {
        if !source
            .headers
            .iter()
            .any(|(key, value)| key == "authorization" && !value.is_empty())
        {
            return Ok(None);
        }
        let mut source = source.clone();
        source
            .headers
            .retain(|(key, _)| !matches!(key.as_str(), "play-link" | "authx"));
        source.target = "/v/api/v1/media/range/session-scope".into();
        let id = source.cache_id();
        source.target = "/".into();
        self.db.lock().unwrap().execute(
            "INSERT INTO sessions(id,source,updated_at) VALUES (?1,?2,?3)
             ON CONFLICT(id) DO UPDATE SET source=excluded.source,updated_at=excluded.updated_at",
            params![id, serde_json::to_string(&source)?, now()],
        )?;
        Ok(self
            .sessions()?
            .into_iter()
            .find(|session| session.id == id))
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let db = self.db.lock().unwrap();
        let rows: Vec<StoredSessionRow> = db
            .prepare("SELECT id,source,account_key,checked_at,error FROM sessions ORDER BY updated_at DESC,id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(id, raw, account_key, checked_at, error)| {
                let source: Source = serde_json::from_str(&raw)?;
                Ok(Session {
                    id,
                    origin: source.origin.clone(),
                    status: if error.as_deref() == Some("authentication") {
                        "needs_login"
                    } else if error.as_deref() == Some("network") {
                        "offline"
                    } else if error.is_some() {
                        "needs_attention"
                    } else if account_key.is_some() {
                        "ready"
                    } else {
                        "checking"
                    }
                    .into(),
                    account_key,
                    checked_at,
                    source,
                })
            })
            .collect()
    }

    pub fn policy(&self) -> Result<Policy> {
        let raw: Option<String> = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key='automation_policy'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
            .unwrap_or_else(|| Ok(Policy::default()))
    }

    pub fn save_policy(&self, policy: &Policy, clear_playback_work: bool) -> Result<()> {
        policy.validate()?;
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO settings(key,value) VALUES ('automation_policy',?1)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [serde_json::to_string(policy)?],
        )?;
        if clear_playback_work {
            tx.execute("DELETE FROM playback_work", [])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn enqueue_catalog_media(
        &self,
        session: &Session,
        item: &MediaItem,
        media: &str,
        source: &Source,
        explicit: bool,
        paused: bool,
    ) -> Result<String> {
        let account = self.account_for_source(source)?;
        let mut db = self.db.lock().unwrap();
        let transaction = db.transaction()?;
        if !explicit && is_suppressed(&transaction, &session.id, &item.guid)? {
            bail!("automatic download suppressed");
        }
        let download = Self::enqueue_download(&transaction, source, account.as_deref(), paused)?;
        let state: String = transaction.query_row(
            "SELECT state FROM downloads WHERE id=?1",
            [&download],
            |r| r.get(0),
        )?;
        if state == "deleting" {
            bail!("cache cleanup is in progress; retry shortly");
        }
        let completed = (item.watched == Some(true)).then(now);
        transaction.execute("INSERT INTO managed_media(session_id,item_guid,item_json,media_guid,download_id,completed_at,upstream_watched)
            VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(session_id,item_guid) DO UPDATE SET
            item_json=excluded.item_json,media_guid=excluded.media_guid,download_id=excluded.download_id",
            params![session.id,item.guid,serde_json::to_string(item)?,media,download,completed,item.watched])?;
        if explicit {
            transaction.execute(
                "DELETE FROM prefetch_suppressions WHERE session_id=?1 AND item_guid=?2",
                params![session.id, item.guid],
            )?;
            transaction.execute("UPDATE managed_media SET suppressed=0,completed_at=CASE WHEN suppressed=1 THEN ?3 ELSE completed_at END WHERE session_id=?1 AND item_guid=?2",
                params![session.id,item.guid,completed])?;
            transaction.execute(
                "UPDATE downloads SET state='queued',error=NULL WHERE id=?1 AND state IN ('removed','cancelled')",
                [&download],
            )?;
        }
        transaction.commit()?;
        Ok(download)
    }

    pub fn managed(&self) -> Result<Vec<ManagedMedia>> {
        let db = self.db.lock().unwrap();
        let rows: Vec<StoredManagedRow> = db.prepare(
            "SELECT session_id,item_json,media_guid,download_id,completed_at,keep,suppressed FROM managed_media ORDER BY rowid DESC")?
            .query_map([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(
                |(session_id, raw, media_guid, download_id, completed_at, keep, suppressed)| {
                    Ok(ManagedMedia {
                        session_id,
                        item: serde_json::from_str(&raw)?,
                        media_guid,
                        download_id,
                        completed_at,
                        keep,
                        suppressed,
                    })
                },
            )
            .collect()
    }

    pub fn suppressed(&self, session: &str, item: &str) -> Result<bool> {
        is_suppressed(&self.db.lock().unwrap(), session, item)
    }

    pub fn update_completion(
        &self,
        session: &str,
        item: &str,
        watched: Option<bool>,
        at: i64,
    ) -> Result<()> {
        let db = self.db.lock().unwrap();
        let manual: Option<Option<bool>> = db
            .query_row(
                "SELECT manual_completed FROM managed_media WHERE session_id=?1 AND item_guid=?2",
                params![session, item],
                |r| r.get(0),
            )
            .optional()?;
        let effective = manual.flatten().or(watched);
        if let Some(watched) = effective {
            db.execute("UPDATE managed_media SET upstream_watched=?3,completed_at=CASE WHEN ?4 THEN COALESCE(completed_at,?5) ELSE NULL END,
                suppressed=CASE WHEN ?4 THEN suppressed ELSE 0 END WHERE session_id=?1 AND item_guid=?2",
                params![session,item,watched,watched,at])?;
        }
        Ok(())
    }

    pub fn set_retention(
        &self,
        session: &str,
        item: &str,
        keep: Option<bool>,
        completed: Option<bool>,
        at: i64,
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let download: String = tx
            .query_row(
                "SELECT download_id FROM managed_media WHERE session_id=?1 AND item_guid=?2",
                params![session, item],
                |r| r.get(0),
            )
            .optional()?
            .context("managed item not found")?;
        let state: String =
            tx.query_row("SELECT state FROM downloads WHERE id=?1", [download], |r| {
                r.get(0)
            })?;
        if state == "deleting" {
            bail!("cache cleanup is in progress");
        }
        if let Some(keep) = keep {
            tx.execute(
                "UPDATE managed_media SET keep=?3 WHERE session_id=?1 AND item_guid=?2",
                params![session, item, keep],
            )?;
        }
        if let Some(completed) = completed {
            tx.execute("UPDATE managed_media SET manual_completed=?3,completed_at=CASE WHEN ?3 THEN COALESCE(completed_at,?4) ELSE NULL END,
            suppressed=CASE WHEN ?3 THEN suppressed ELSE 0 END WHERE session_id=?1 AND item_guid=?2",params![session,item,completed,at])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn note_playback(&self, session: &str, item: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO playback_work(session_id,item_guid,seen_at) VALUES (?1,?2,?3)
            ON CONFLICT(session_id,item_guid) DO UPDATE SET seen_at=excluded.seen_at",
            params![session, item, now()],
        )?;
        Ok(())
    }

    pub fn playback_work(&self) -> Result<Vec<PlaybackWork>> {
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM playback_work WHERE seen_at<?1", [now() - 600])?;
        Ok(db.prepare("SELECT session_id,item_guid,seen_at FROM playback_work ORDER BY seen_at DESC LIMIT 100")?
            .query_map([],|r|Ok(PlaybackWork{session_id:r.get(0)?,item_guid:r.get(1)?,seen_at:r.get(2)?}))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn finish_playback_work(&self, work: &PlaybackWork) -> Result<()> {
        self.db.lock().unwrap().execute(
            "DELETE FROM playback_work WHERE session_id=?1 AND item_guid=?2 AND seen_at<=?3",
            params![work.session_id, work.item_guid, work.seen_at],
        )?;
        Ok(())
    }

    pub fn clear_playback_work(&self) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM playback_work", [])?;
        Ok(())
    }

    pub fn cleanup_candidates(
        &self,
        at: i64,
        retention: u64,
        manual_only: bool,
    ) -> Result<Vec<String>> {
        Ok(self.db.lock().unwrap().prepare("SELECT download_id FROM managed_media GROUP BY download_id
            HAVING MIN(CASE WHEN keep=0 AND completed_at IS NOT NULL AND completed_at<=?1 AND (NOT ?2 OR manual_completed=1) THEN 1 ELSE 0 END)=1")?
            .query_map(params![at.saturating_sub(retention as i64),manual_only],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn pending_cleanup(&self) -> Result<Vec<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .prepare("SELECT id FROM downloads WHERE state='deleting'")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn begin_cleanup(
        &self,
        download: &str,
        at: i64,
        retention: u64,
        manual_only: bool,
    ) -> Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let pending: bool = tx
            .query_row(
                "SELECT state='deleting' FROM downloads WHERE id=?1",
                [download],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if pending {
            return Ok(true);
        }
        let blocked: i64 = tx.query_row("SELECT COUNT(*) FROM managed_media WHERE download_id=?1 AND (keep=1 OR completed_at IS NULL OR completed_at>?2 OR (?3 AND manual_completed IS NOT 1))", params![download,at.saturating_sub(retention as i64),manual_only],|r|r.get(0))?;
        if blocked > 0 {
            return Ok(false);
        }
        let changed = tx.execute("UPDATE downloads SET state='deleting' WHERE id=?1 AND state IN ('complete','paused','failed','waiting_space','deleting')",[download])?;
        if changed > 0 {
            tx.execute(
                "UPDATE managed_media SET suppressed=1 WHERE download_id=?1",
                [download],
            )?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    pub fn finish_cleanup(&self, download: &str, cache_key: &str) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        if !purge_download(&tx, download, "deleting")? {
            return Ok(());
        }
        tx.execute("DELETE FROM segments WHERE id=?1 OR id IN (SELECT old_key FROM cache_aliases WHERE current_key=?1)", [cache_key])?;
        tx.execute(
            "DELETE FROM cache_aliases WHERE current_key=?1",
            [cache_key],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn prune_removed(&self, item: &crate::store::Download) -> Result<bool> {
        if item.id.len() != 64 || !item.id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(false);
        }
        for partial in [false, true] {
            if !matches!(self.file(&item.id, partial).try_exists(), Ok(false)) {
                return Ok(false);
            }
        }
        if !self.segments(&item.source.cache_id())?.is_empty() {
            return Ok(false);
        }
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let purged = purge_download(&tx, &item.id, "removed")?;
        tx.commit()?;
        Ok(purged)
    }
}

fn is_suppressed(db: &Connection, session: &str, item: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM prefetch_suppressions WHERE session_id=?1 AND item_guid=?2
          UNION ALL SELECT 1 FROM managed_media WHERE session_id=?1 AND item_guid=?2 AND suppressed=1)",
        params![session, item],
        |row| row.get(0),
    )?)
}

fn purge_download(tx: &Transaction<'_>, id: &str, state: &str) -> Result<bool> {
    let removed = tx.execute(
        "DELETE FROM downloads WHERE id=?1 AND state=?2",
        params![id, state],
    )?;
    if removed == 0 {
        return Ok(false);
    }
    tx.execute(
        "INSERT OR IGNORE INTO prefetch_suppressions(session_id,item_guid)
         SELECT session_id,item_guid FROM managed_media WHERE download_id=?1 AND (suppressed=1 OR ?2='deleting')",
        params![id, state],
    )?;
    tx.execute("DELETE FROM managed_media WHERE download_id=?1", [id])?;
    Ok(true)
}
