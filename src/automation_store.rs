//! Durable catalog bindings, playback work and local cache retention state.
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    library::{MediaItem, Session},
    protocol::Source,
    scheduler::{Policy, now},
    store::Store,
};

type StoredManagedRow = (String, String, String, String, Option<i64>, bool, bool);
type StoredSessionRow = (String, String, Option<String>, i64, Option<String>);

// A shared file can only be deleted when all its catalog bindings are eligible.
const CLEANUP_ELIGIBILITY: &str = "
    GROUP BY media.download_id
    HAVING MIN(CASE WHEN media.keep=0
        AND ((media.completed_at<=?1 AND (NOT ?2 OR media.manual_completed=1)) OR (
            ?3
            AND json_extract(media.item_json,'$.kind')='Episode'
            AND json_extract(media.item_json,'$.season_number')>0
            AND json_extract(media.item_json,'$.episode_number')>0
            AND EXISTS (
                SELECT 1 FROM sessions
                JOIN episode_playback ON episode_playback.account_key=sessions.account_key
                LEFT JOIN season_series AS played ON played.account_key=sessions.account_key
                    AND played.season_guid=episode_playback.season_guid
                    AND played.season_number=episode_playback.season_number
                LEFT JOIN season_series AS cached ON cached.account_key=sessions.account_key
                    AND cached.season_guid=json_extract(media.item_json,'$.parent_guid')
                    AND cached.season_number=json_extract(media.item_json,'$.season_number')
                WHERE sessions.id=media.session_id
                AND (
                    (episode_playback.season_guid=json_extract(media.item_json,'$.parent_guid')
                        AND episode_playback.season_number=json_extract(media.item_json,'$.season_number')
                        AND episode_playback.episode_number>json_extract(media.item_json,'$.episode_number'))
                    OR (played.series_guid=cached.series_guid AND played.season_number>cached.season_number)
                )
            )
        ))
        THEN 1 ELSE 0 END)=1";

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

fn playback_work_timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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
                seen_at INTEGER NOT NULL, PRIMARY KEY(session_id,item_guid));
             CREATE TABLE IF NOT EXISTS episode_playback (
                account_key TEXT NOT NULL, season_guid TEXT NOT NULL, season_number INTEGER NOT NULL,
                episode_number INTEGER NOT NULL, PRIMARY KEY(account_key,season_guid,season_number));
             CREATE TABLE IF NOT EXISTS season_series (
                account_key TEXT NOT NULL, season_guid TEXT NOT NULL, season_number INTEGER NOT NULL,
                series_guid TEXT NOT NULL, PRIMARY KEY(account_key,season_guid,season_number));"
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
            tx.execute_batch("DELETE FROM playback_work; DELETE FROM episode_playback;")?;
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
            ON CONFLICT(session_id,item_guid) DO UPDATE SET seen_at=MAX(excluded.seen_at,playback_work.seen_at+1)",
            params![session, item, playback_work_timestamp_ms()],
        )?;
        Ok(())
    }

    pub fn playback_work(&self) -> Result<Vec<PlaybackWork>> {
        let db = self.db.lock().unwrap();
        db.execute(
            "DELETE FROM playback_work WHERE seen_at<?1",
            [playback_work_timestamp_ms() - 600_000],
        )?;
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

    pub fn note_episode_playback(&self, session_id: &str, item: &MediaItem) -> Result<()> {
        if item.kind != "Episode"
            || item.season_number == 0
            || item.episode_number == 0
            || !crate::library::valid_guid(&item.parent_guid)
        {
            return Ok(());
        }
        let (Ok(season), Ok(episode)) = (
            i64::try_from(item.season_number),
            i64::try_from(item.episode_number),
        ) else {
            return Ok(());
        };
        self.db.lock().unwrap().execute(
            "INSERT INTO episode_playback(account_key,season_guid,season_number,episode_number)
             SELECT account_key,?2,?3,?4 FROM sessions
             WHERE id=?1 AND account_key IS NOT NULL AND error IS NOT 'authentication'
             ON CONFLICT(account_key,season_guid,season_number) DO UPDATE SET episode_number=excluded.episode_number
             WHERE excluded.episode_number>episode_playback.episode_number",
            params![session_id, item.parent_guid, season, episode],
        )?;
        Ok(())
    }

    pub fn unresolved_playback_seasons(&self) -> Result<Vec<(String, String, u64)>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .prepare(
                "SELECT MIN(sessions.id), progress.season_guid, progress.season_number
             FROM episode_playback AS progress JOIN sessions USING (account_key)
             LEFT JOIN season_series USING (account_key, season_guid, season_number)
             WHERE progress.season_number>1 AND season_series.series_guid IS NULL
             AND sessions.error IS NOT 'authentication'
             GROUP BY progress.account_key, progress.season_guid, progress.season_number
             ORDER BY progress.account_key, progress.season_number, progress.season_guid",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn remember_series_seasons(
        &self,
        account: &str,
        series: &str,
        seasons: &[MediaItem],
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        for season in seasons {
            tx.execute(
                "INSERT INTO season_series(account_key,season_guid,season_number,series_guid)
                 VALUES (?1,?2,?3,?4)
                 ON CONFLICT DO UPDATE SET series_guid=excluded.series_guid",
                params![account, season.guid, season.season_number, series],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn cleanup_candidates(&self, at: i64, policy: &Policy) -> Result<Vec<String>> {
        let query =
            format!("SELECT media.download_id FROM managed_media AS media {CLEANUP_ELIGIBILITY}");
        Ok(self
            .db
            .lock()
            .unwrap()
            .prepare(&query)?
            .query_map(
                params![
                    policy.retention_cutoff(at),
                    policy.local_history,
                    policy.accelerates_episode_cleanup(),
                ],
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?)
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

    pub fn begin_cleanup(&self, download: &str, at: i64, policy: &Policy) -> Result<bool> {
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
        if !policy.cleanup_enabled {
            return Ok(false);
        }
        let query = format!(
            "SELECT 1 FROM managed_media AS media WHERE media.download_id=?4 {CLEANUP_ELIGIBILITY}"
        );
        let eligible = tx
            .query_row(
                &query,
                params![
                    policy.retention_cutoff(at),
                    policy.local_history,
                    policy.accelerates_episode_cleanup(),
                    download,
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !eligible {
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
