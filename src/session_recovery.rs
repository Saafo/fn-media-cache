//! Verified account continuity; never infer identity from a new token alone.
use crate::{engine::Engine, library::Session, protocol::Source, store::Store};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};

fn credential_key(source: &Source) -> String {
    let mut scope = source.clone();
    scope.target = "/v/api/v1/media/range/session-scope".into();
    scope.cache_id()
}

impl Store {
    pub fn record_failure(
        &self,
        id: &str,
        state: &str,
        error: &str,
        retry_at: Option<i64>,
    ) -> Result<()> {
        let kind = match state {
            "retry_wait" => "temporary",
            "waiting_login" => "authentication",
            "waiting_space" => "capacity",
            _ => "permanent",
        };
        self.db.lock().unwrap().execute("UPDATE downloads SET state=?2,error=?3,retry_at=?4,attempts=attempts+CASE WHEN ?2='retry_wait' THEN 1 ELSE 0 END,failure_kind=?5 WHERE id=?1 AND state='downloading'",
            params![id,state,error,retry_at,kind])?;
        Ok(())
    }

    pub fn wake_retries(&self) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE downloads SET retry_at=?1 WHERE state='retry_wait'",
            [crate::scheduler::now()],
        )?;
        Ok(())
    }

    pub fn session_check_failed(&self, id: &str, kind: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE sessions SET checked_at=?2,error=?3 WHERE id=?1",
            params![id, crate::scheduler::now(), kind],
        )?;
        Ok(())
    }

    pub fn confirm_session(&self, session: &Session, account: &str) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let rows: Vec<(String, String, Option<String>)> = tx
            .prepare("SELECT id,source,account_key FROM sessions")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut credentials = vec![credential_key(&session.source)];
        for (id, raw, key) in &rows {
            if id == &session.id || key.as_deref() == Some(account) {
                let old: Source = serde_json::from_str(raw)?;
                credentials.push(credential_key(&old));
                let source = Source {
                    target: "/".into(),
                    ..session.source.clone()
                };
                tx.execute("UPDATE sessions SET source=?2,account_key=?3,checked_at=?4,error=NULL WHERE id=?1",
                    params![id,serde_json::to_string(&source)?,account,crate::scheduler::now()])?;
            }
        }
        let downloads: Vec<(String, String, Option<String>)> = tx
            .prepare("SELECT id,source,account_key FROM downloads")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, raw, key) in downloads {
            let old: Source = serde_json::from_str(&raw)?;
            if crate::fntv::media_guid(&old).is_none() {
                continue;
            }
            if let Some(bound) = key.as_deref() {
                if bound != account {
                    continue;
                }
            } else if !credentials.contains(&credential_key(&old)) {
                continue;
            }
            let mut new = old.clone();
            new.headers = session.source.headers.clone();
            let old_key = old.cache_id();
            let new_key = new.cache_id();
            if old_key != new_key {
                tx.execute(
                    "UPDATE cache_aliases SET current_key=?2 WHERE current_key=?1",
                    params![old_key, new_key],
                )?;
                tx.execute("INSERT INTO cache_aliases(old_key,current_key) VALUES (?1,?2) ON CONFLICT(old_key) DO UPDATE SET current_key=excluded.current_key",params![old_key,new_key])?;
            }
            tx.execute(
                "UPDATE downloads SET source=?2,cache_key=?3,account_key=?4,
                state=CASE WHEN state='waiting_login' AND ?5 THEN 'queued' ELSE state END,
                error=CASE WHEN state='waiting_login' AND ?5 THEN NULL ELSE error END,
                attempts=CASE WHEN state='waiting_login' AND ?5 THEN 0 ELSE attempts END WHERE id=?1",
                params![id, serde_json::to_string(&new)?, new_key, account,old_key!=new_key],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn account_for_source(&self, source: &Source) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT account_key FROM sessions WHERE id=?1",
                [credential_key(source)],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }
}

impl Engine {
    pub async fn verify_session(&self, session: &Session) -> Result<()> {
        let identity = crate::fntv::account_identity(&self.client, &session.source).await?;
        self.store.confirm_session(session, &identity)?;
        if session.account_key.is_none() || session.status != "ready" {
            crate::event!(
                "session {} verified; eligible tasks can resume",
                &session.id[..12]
            );
        }
        self.wake.notify_one();
        Ok(())
    }

    pub async fn refresh_sessions(&self) -> Result<()> {
        let interval = self.policy.read().unwrap().session_check_seconds as i64;
        let mut checked = std::collections::HashSet::new();
        for session in self
            .store
            .sessions()?
            .into_iter()
            .filter(|s| crate::scheduler::now() - s.checked_at >= interval)
        {
            if !checked.insert(credential_key(&session.source)) {
                continue;
            }
            if let Err(error) = self.verify_session(&session).await {
                let kind = if error.is::<crate::recovery::AuthRequired>() {
                    "authentication"
                } else if error.is::<crate::recovery::Retryable>() {
                    "network"
                } else {
                    "verification"
                };
                if session.status == "ready" || session.status == "checking" {
                    crate::event!(
                        "session {} verification failed kind={}",
                        &session.id[..12],
                        kind
                    );
                }
                self.store.session_check_failed(&session.id, kind)?;
            }
            if checked.len() >= 2 {
                break;
            }
        }
        Ok(())
    }
}
