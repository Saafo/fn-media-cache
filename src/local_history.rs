//! Recent plays owned by the local proxy. Only allowlisted client metadata is persisted.
use std::{collections::VecDeque, net::IpAddr, sync::Mutex, time::Instant};

use anyhow::Result;
use axum::{
    Json,
    body::{Body, to_bytes},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    engine::Engine, library::valid_guid, privacy, protocol::Source,
    session_recovery::credential_key, store::Store,
};

const HISTORY_LIMIT: usize = 100;
pub const INFO_LIMIT: usize = 512 * 1024;

pub fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS local_history (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT, owner TEXT NOT NULL,
            item_guid TEXT NOT NULL, entry TEXT NOT NULL, UNIQUE(owner, item_guid)
        );
        CREATE INDEX IF NOT EXISTS local_history_recent ON local_history(owner, sequence DESC);",
    )?;
    Ok(())
}

fn session_owner(session: &str) -> String {
    format!("session:{session}")
}
fn account_owner(account: &str) -> String {
    format!("account:{account}")
}

fn trim(db: &Connection, owner: &str) -> Result<()> {
    db.execute(
        "DELETE FROM local_history WHERE sequence IN (
           SELECT sequence FROM local_history WHERE owner=?1 ORDER BY sequence DESC LIMIT -1 OFFSET ?2
         )", params![owner, HISTORY_LIMIT],
    )?;
    Ok(())
}

pub fn bind_account(db: &Connection, session: &str, account: &str) -> Result<()> {
    let session = session_owner(session);
    let account = account_owner(account);
    // Prefer whichever record was written later when the session and account both have an entry.
    db.execute(
        "DELETE FROM local_history WHERE owner IN (?1,?2) AND EXISTS (
           SELECT 1 FROM local_history newer WHERE newer.owner IN (?1,?2)
             AND newer.item_guid=local_history.item_guid AND newer.sequence>local_history.sequence
         )",
        params![session, account],
    )?;
    db.execute(
        "UPDATE local_history SET owner=?2 WHERE owner=?1",
        params![session, account],
    )?;
    trim(db, &account)
}

fn owner(db: &Connection, source: &Source) -> Result<String> {
    let session = credential_key(source);
    let verified: Option<(String, String)> = db
        .query_row(
            "SELECT account_key,source FROM sessions
         WHERE id=?1 AND account_key IS NOT NULL AND error IS NOT 'authentication'",
            [&session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((account, raw)) = verified {
        // Session recovery may have replaced this row's credentials with a newer login.
        let current: Source = serde_json::from_str(&raw)?;
        if credential_key(&current) == session {
            return Ok(account_owner(&account));
        }
    }
    Ok(session_owner(&session))
}

impl Store {
    fn history_list(&self, source: &Source) -> Result<Vec<Value>> {
        let db = self.db.lock().unwrap();
        let mut query = db.prepare(
            "SELECT entry FROM local_history WHERE owner=?1 ORDER BY sequence DESC LIMIT ?2",
        )?;
        let entries = query.query_map(params![owner(&db, source)?, HISTORY_LIMIT], |row| {
            row.get::<_, String>(0)
        })?;
        entries
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect()
    }

    fn history_entry(&self, source: &Source, item: &str) -> Result<Option<Value>> {
        let db = self.db.lock().unwrap();
        entry(&db, &owner(&db, source)?, item)
    }

    fn write_history(
        &self,
        source: &Source,
        item: &str,
        report: Report,
        metadata: Option<Value>,
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let owner = owner(&tx, source)?;
        let previous = entry(&tx, &owner, item)?.unwrap_or_else(|| json!({}));
        let mut value = metadata.unwrap_or_else(|| previous.clone());
        let media = report.media_guid.as_deref().filter(|id| !id.is_empty());
        if let Some(media) = media {
            if value["media_guid"]
                .as_str()
                .is_some_and(|previous| previous != media)
            {
                for key in ["audio_guid", "video_guid", "subtitle_guid", "duration"] {
                    value.as_object_mut().unwrap().remove(key);
                }
            }
            value["media_guid"] = json!(media);
        }
        value["guid"] = json!(item);
        value["ts"] = json!(report.ts);
        value["watched"] = json!(0);
        value["watched_ts"] = json!(0);
        if let Some(duration) = report
            .duration
            .filter(|duration| duration.as_f64().is_some_and(|value| value > 0.0))
        {
            value["duration"] = json!(duration);
        } else if value["duration"].is_null() && previous["media_guid"] == value["media_guid"] {
            value["duration"] = previous["duration"].clone();
        }
        tx.execute(
            "INSERT OR REPLACE INTO local_history(owner,item_guid,entry) VALUES (?1,?2,?3)",
            params![owner, item, serde_json::to_string(&value)?],
        )?;
        trim(&tx, &owner)?;
        tx.commit()?;
        Ok(())
    }

    fn delete_history(&self, source: &Source, item: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "DELETE FROM local_history WHERE owner=?1 AND item_guid=?2",
            params![owner(&db, source)?, item],
        )?;
        Ok(())
    }
}

fn entry(db: &Connection, owner: &str, item: &str) -> Result<Option<Value>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT entry FROM local_history WHERE owner=?1 AND item_guid=?2",
            params![owner, item],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}

#[derive(Deserialize)]
struct Report {
    item_guid: Option<String>,
    media_guid: Option<String>,
    ts: serde_json::Number,
    duration: Option<serde_json::Number>,
}

#[derive(Default)]
pub struct Metadata(Mutex<VecDeque<(String, String, Value)>>);

impl Metadata {
    fn remember(&self, source: &Source, item: &str, value: Value) {
        let key = credential_key(source);
        let mut entries = self.0.lock().unwrap();
        entries.retain(|(owner, guid, _)| owner != &key || guid != item);
        entries.push_front((key, item.to_owned(), value));
        entries.truncate(HISTORY_LIMIT);
    }

    fn get(&self, source: &Source, item: &str) -> Option<Value> {
        let key = credential_key(source);
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(owner, guid, _)| owner == &key && guid == item)
            .map(|(_, _, value)| value.clone())
    }

    fn item_for_media(&self, source: &Source, media: &str) -> Option<String> {
        self.with_media(source, media, |guid, _| guid.to_owned())
    }

    pub fn display_item(&self, source: &Source) -> Option<crate::library::MediaItem> {
        let media =
            privacy::endpoint(source.target.split('?').next()?)?.strip_prefix("media/range/")?;
        if !valid_guid(media) {
            return None;
        }
        self.with_media(source, media, |guid, info| {
            let mut item = info.clone();
            item["guid"] = json!(guid);
            crate::library::MediaItem::parse(&item).ok()
        })?
    }

    fn with_media<T>(
        &self,
        source: &Source,
        media: &str,
        read: impl FnOnce(&str, &Value) -> T,
    ) -> Option<T> {
        let key = credential_key(source);
        let entries = self.0.lock().unwrap();
        let mut matches = entries
            .iter()
            .filter(|(owner, _, info)| owner == &key && info["media_guid"] == media);
        let (_, guid, info) = matches.next()?;
        matches.next().is_none().then(|| read(guid, info))
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

pub fn remember_display_info(engine: &Engine, source: &Source, body: &Value) {
    let policy = engine.policy.read().unwrap();
    if policy.syncs_history()
        && has_credentials(source)
        && body["code"] == 0
        && let Some(item) = body["data"]["guid"].as_str().filter(|id| valid_guid(id))
    {
        engine
            .history_metadata
            .remember(source, item, recent_metadata(&body["data"]));
    }
}

fn has_credentials(source: &Source) -> bool {
    source
        .headers
        .iter()
        .any(|(name, value)| name == "authorization" && !value.trim().is_empty())
}

pub fn list_request(method: &Method, path: &str) -> bool {
    matches!(*method, Method::GET | Method::HEAD) && privacy::endpoint(path) == Some("play/list")
}

pub fn info_request(method: &Method, path: &str) -> bool {
    *method == Method::POST && privacy::endpoint(path) == Some("play/info")
}

fn response(data: Value) -> Response {
    let mut response = Json(json!({"code":0,"msg":"","data":data})).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        "x-fn-media-cache-privacy",
        HeaderValue::from_static("local"),
    );
    response
}

fn store_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "local playback history unavailable",
    )
        .into_response()
}

pub async fn handle(
    engine: &Engine,
    source: &Source,
    method: &Method,
    path: &str,
    body: Body,
    peer: IpAddr,
    received_at: Instant,
) -> Response {
    if !has_credentials(source) {
        return (StatusCode::UNAUTHORIZED, "playback session required").into_response();
    }
    if list_request(method, path) {
        let policy = engine.policy.read().unwrap();
        if !policy.local_history {
            return (StatusCode::CONFLICT, "history mode changed").into_response();
        }
        return match engine.store.history_list(source) {
            Ok(entries) => response(json!(entries)),
            Err(_) => store_error(),
        };
    }
    if privacy::endpoint(path) != Some("play/record") {
        // Watched and playing writes are separate from the recent-play list.
        return response(json!(true));
    }
    let Ok(body) = to_bytes(body, 64 * 1024).await else {
        return (StatusCode::BAD_REQUEST, "cannot read playback report").into_response();
    };
    let policy = engine.policy.read().unwrap();
    if !policy.local_history {
        return (StatusCode::CONFLICT, "history mode changed").into_response();
    }
    if *method == Method::DELETE {
        #[derive(Deserialize)]
        struct Removal {
            item_guid: String,
        }
        let item = if body.is_empty() {
            source.url().ok().and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "item_guid")
                    .map(|(_, guid)| guid.into_owned())
            })
        } else {
            serde_json::from_slice::<Removal>(&body)
                .ok()
                .map(|removal| removal.item_guid)
        };
        if let Some(item) = item.filter(|item| valid_guid(item)) {
            return match engine.store.delete_history(source, &item) {
                Ok(()) => response(json!(true)),
                Err(_) => store_error(),
            };
        }
        return (StatusCode::BAD_REQUEST, "invalid playback record to remove").into_response();
    }
    let Ok(report) = serde_json::from_slice::<Report>(&body) else {
        return (StatusCode::BAD_REQUEST, "invalid playback report").into_response();
    };
    let Some(position) = report.ts.as_f64().filter(|value| *value >= 0.0) else {
        return (StatusCode::BAD_REQUEST, "invalid playback report").into_response();
    };
    if report
        .media_guid
        .as_deref()
        .is_some_and(|id| !id.is_empty() && !valid_guid(id))
    {
        return (StatusCode::BAD_REQUEST, "invalid playback report").into_response();
    }
    let item = report
        .item_guid
        .clone()
        .filter(|item| !item.is_empty())
        .or_else(|| {
            engine.history_metadata.item_for_media(
                source,
                report
                    .media_guid
                    .as_deref()
                    .filter(|media| !media.is_empty())?,
            )
        });
    let Some(item) = item.filter(|item| valid_guid(item)) else {
        return (StatusCode::BAD_REQUEST, "unknown playback item").into_response();
    };
    let metadata = engine.history_metadata.get(source, &item);
    if engine
        .store
        .write_history(source, &item, report, metadata)
        .is_err()
    {
        return store_error();
    }
    if (policy.tracks_watch_time() || policy.prevent_idle_sleep)
        && let Ok(Some(session)) = engine.store.save_session(source)
    {
        engine.report_playback(&session.id, &item, position, peer, received_at, &policy);
        if policy.tracks_watch_time()
            && engine.watch_time.lock().unwrap().reached_threshold(
                &session.id,
                &item,
                policy.prefetch_after_seconds,
                Instant::now(),
            )
        {
            let _ = engine.store.note_playback(&session.id, &item);
            engine.scheduler_wake.notify_one();
        }
    }
    response(json!(true))
}

pub fn overlay_play_info(engine: &Engine, source: &Source, body: &mut Value) -> Result<()> {
    if body["code"] != 0 || !body["data"].is_object() {
        return Ok(());
    }
    let data = &mut body["data"];
    let item = data["guid"].as_str().filter(|id| valid_guid(id));
    let policy = engine.policy.read().unwrap();
    let record = if let Some(item) = item
        && policy.local_history
        && has_credentials(source)
    {
        engine
            .history_metadata
            .remember(source, item, recent_metadata(data));
        engine.store.history_entry(source, item)?
    } else {
        None
    };
    let position = record
        .filter(|record| {
            record["media_guid"].is_null() || record["media_guid"] == data["media_guid"]
        })
        .map(|record| record["ts"].clone())
        .unwrap_or_else(|| json!(0));
    data["ts"] = position;
    if let Some(item) = data["item"].as_object_mut() {
        for field in ["watched_ts", "watched", "is_watched"] {
            if item.contains_key(field) {
                item.insert(field.into(), json!(0));
            }
        }
    }
    Ok(())
}

fn recent_metadata(info: &Value) -> Value {
    let mut result = serde_json::Map::new();
    let item = &info["item"];
    for key in [
        "title",
        "tv_title",
        "parent_title",
        "type",
        "parent_guid",
        "single_child_guid",
        "season_number",
        "episode_number",
        "number_of_seasons",
        "number_of_episodes",
        "local_number_of_seasons",
        "local_number_of_episodes",
        "ancestor_guid",
        "ancestor_name",
        "ancestor_category",
        "poster",
        "poster_width",
        "poster_height",
        "runtime",
        "duration",
        "file_name",
    ] {
        if let Some(value) = item
            .get(key)
            .filter(|v| v.is_number() || v.as_str().is_some_and(|s| s.len() <= 2048))
        {
            result.insert(key.into(), value.clone());
        }
    }
    if !result.contains_key("poster")
        && let Some(poster) = item["posters"].as_str().filter(|s| s.len() <= 2048)
    {
        result.insert("poster".into(), json!(poster));
    }
    for key in ["media_guid", "video_guid", "audio_guid", "subtitle_guid"] {
        if let Some(guid) = info[key].as_str().filter(|s| s.is_empty() || valid_guid(s)) {
            result.insert(key.into(), json!(guid));
        }
    }
    Value::Object(result)
}
