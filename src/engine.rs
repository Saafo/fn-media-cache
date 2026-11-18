use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::http::{HeaderMap, header};
use futures_util::StreamExt;
use serde::Serialize;
use tokio::{io::AsyncWriteExt, sync::Notify};
use tokio_util::sync::CancellationToken;

use crate::{
    config::Config,
    fntv,
    protocol::{Source, media_candidate, parse_content_range, strong_etag, text_header},
    routes::{RouteFailure, Routes, transfer_error},
    scheduler::{PlaybackActivity, Policy},
    store::{Download, Store},
};

#[derive(Clone, Serialize)]
pub struct Observation {
    pub id: String,
    pub path: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub total: Option<u64>,
    pub cacheable: bool,
    #[serde(skip)]
    pub source: Source,
}

#[derive(Default)]
pub struct Counters {
    pub requests: AtomicU64,
    pub cache_hits: AtomicU64,
    pub origin_bytes: AtomicU64,
    pub cache_bytes: AtomicU64,
    pub external_redirects: AtomicU64,
}

#[derive(Clone, Serialize)]
pub struct ProgressEvent {
    pub timestamp_ms: u64,
    pub origin: &'static str,
    pub http_status: u16,
    pub json_parsed: bool,
    pub position: Option<f64>,
    pub duration: Option<f64>,
    pub media_hash: Option<String>,
}

pub struct Engine {
    pub config: Config,
    pub store: Store,
    pub client: reqwest::Client,
    pub token: String,
    pub observations: Mutex<VecDeque<Observation>>,
    pub history_metadata: crate::local_history::Metadata,
    pub progress_events: Mutex<VecDeque<ProgressEvent>>,
    pub counters: Counters,
    pub transfers: Arc<crate::transfers::Transfers>,
    pub wake: Notify,
    pub shutdown: CancellationToken,
    pub probe_lock: tokio::sync::Mutex<()>,
    pub routes: Routes,
    pub policy: RwLock<Policy>,
    pub scheduler_wake: Arc<Notify>,
    pub cache_uses: Arc<Mutex<HashMap<String, usize>>>,
    pub playback_activity: Arc<Mutex<PlaybackActivity>>,
}

#[derive(Debug)]
struct CapacityError;
impl std::fmt::Display for CapacityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("waiting for cache quota or free disk space")
    }
}
impl std::error::Error for CapacityError {}

impl Engine {
    pub fn new(config: Config, store: Store, token: String) -> Result<Arc<Self>> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(config.read_timeout_seconds))
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()?;
        let policy = store.policy()?;
        policy.validate()?;
        Ok(Arc::new(Self {
            routes: Routes::new(&config),
            config,
            store,
            client,
            token,
            observations: Mutex::new(VecDeque::new()),
            history_metadata: Default::default(),
            progress_events: Mutex::new(VecDeque::new()),
            counters: Counters::default(),
            transfers: Arc::new(crate::transfers::Transfers::default()),
            wake: Notify::new(),
            shutdown: CancellationToken::new(),
            probe_lock: tokio::sync::Mutex::new(()),
            policy: RwLock::new(policy),
            scheduler_wake: Arc::new(Notify::new()),
            cache_uses: Arc::new(Mutex::new(HashMap::new())),
            playback_activity: Arc::new(Mutex::new(PlaybackActivity::default())),
        }))
    }

    pub fn observe(self: &Arc<Self>, source: Source, status: u16, headers: &HeaderMap) {
        if self.policy.read().unwrap().incognito {
            return;
        }
        if status == 206 {
            self.warm_routes(&source);
        }
        let id = source.id();
        let total = text_header(headers, "content-range")
            .and_then(|v| parse_content_range(&v).map(|r| r.2))
            .or_else(|| text_header(headers, "content-length").and_then(|v| v.parse().ok()));
        let observation = Observation {
            id: id.clone(),
            path: source.display_path(),
            status,
            content_type: text_header(headers, "content-type"),
            total,
            cacheable: matches!(status, 200 | 206) && media_candidate(headers, &source.target),
            source,
        };
        let mut observations = self.observations.lock().unwrap();
        observations.retain(|old| old.id != id);
        observations.push_front(observation);
        observations.truncate(100);
    }

    pub fn record_progress(&self, source: &Source, body: Option<Vec<u8>>, status: u16) {
        let policy = self.policy.read().unwrap();
        if !policy.syncs_history() {
            return;
        }
        use sha2::{Digest, Sha256};
        let parsed =
            body.and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        let event = ProgressEvent {
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            origin: "client",
            http_status: status,
            json_parsed: parsed.is_some(),
            position: parsed
                .as_ref()
                .and_then(|v| v.get("ts"))
                .and_then(serde_json::Value::as_f64),
            duration: parsed
                .as_ref()
                .and_then(|v| v.get("duration"))
                .and_then(serde_json::Value::as_f64),
            media_hash: parsed
                .as_ref()
                .and_then(|v| v.get("media_guid"))
                .and_then(serde_json::Value::as_str)
                .map(|value| crate::protocol::hex(&Sha256::digest(value.as_bytes()))),
        };
        if status == 200
            && let Some(item) = parsed
                .as_ref()
                .and_then(|v| v.get("item_guid"))
                .and_then(serde_json::Value::as_str)
            && crate::library::valid_guid(item)
            && let Ok(Some(session)) = self.store.save_session(source)
        {
            let _ = self.store.note_playback(&session.id, item);
            self.scheduler_wake.notify_one();
        }
        crate::event!(
            "client progress POST http_status={} position={:?} duration={:?}",
            status,
            event.position,
            event.duration
        );
        let mut events = self.progress_events.lock().unwrap();
        events.push_front(event);
        events.truncate(100);
    }

    pub async fn revalidate(&self, source: &Source, item: &Download) -> Option<HeaderMap> {
        if let Some(expected) = item.file_version.as_deref() {
            return self.revalidate_native(source, expected, item.total?).await;
        }
        let response = self
            .client
            .head(source.url().ok()?)
            .headers(source.request_headers().ok()?)
            .send()
            .await
            .ok()?;
        let headers = response.headers();
        if response.status() != reqwest::StatusCode::OK
            || text_header(headers, "content-length").and_then(|s| s.parse::<u64>().ok())
                != item.total
            || text_header(headers, "content-encoding").is_some_and(|v| v != "identity")
        {
            return None;
        }
        let unchanged = if let Some(etag) = strong_etag(&item.etag) {
            text_header(headers, "etag").as_deref() == Some(etag)
        } else {
            item.last_modified.is_some()
                && text_header(headers, "last-modified") == item.last_modified
        };
        unchanged.then(|| headers.clone())
    }

    pub(crate) async fn revalidate_native(
        &self,
        source: &Source,
        expected: &str,
        size: u64,
    ) -> Option<HeaderMap> {
        let version = fntv::file_version(&self.client, source).await.ok()?;
        if version.fingerprint != expected || version.size != size {
            return None;
        }
        let headers = fntv::authorize_range(&self.client, source, size)
            .await
            .ok()?;
        self.counters.origin_bytes.fetch_add(1, Ordering::Relaxed);
        Some(headers)
    }

    pub async fn worker(self: Arc<Self>) {
        loop {
            if self.shutdown.is_cancelled() {
                return;
            }
            match self.store.next() {
                Ok(Some(mut item)) => {
                    if let Err(error) = self.download(&mut item).await {
                        // All messages in this path are sanitized; never log a request URL/header.
                        let message = error.to_string();
                        let _ = self.store.progress(&item);
                        if self
                            .store
                            .get(&item.id)
                            .ok()
                            .flatten()
                            .is_some_and(|current| current.state == "downloading")
                        {
                            let policy = self.policy.read().unwrap().clone();
                            let state = if error.is::<CapacityError>() {
                                "waiting_space"
                            } else if error.is::<crate::recovery::AuthRequired>() {
                                "waiting_login"
                            } else if error.is::<crate::recovery::Retryable>()
                                && item.attempts < policy.retry_max_attempts
                            {
                                "retry_wait"
                            } else {
                                "failed"
                            };
                            let retry_at = (state == "retry_wait").then(|| {
                                crate::scheduler::now()
                                    + crate::recovery::delay(
                                        item.attempts + 1,
                                        policy.retry_base_seconds,
                                        policy.retry_max_seconds,
                                    ) as i64
                            });
                            let _ = self
                                .store
                                .record_failure(&item.id, state, &message, retry_at);
                            crate::event!(
                                "download {} state={} attempts={} retry_at={:?} reason={}",
                                &item.id[..12],
                                state,
                                item.attempts + 1,
                                retry_at,
                                message
                            );
                        }
                    }
                    self.scheduler_wake.notify_one();
                }
                Ok(None) => tokio::select! {
                    _ = self.shutdown.cancelled() => return,
                    _ = self.wake.notified() => {},
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                },
                Err(_) => {
                    crate::event!("download queue unavailable");
                    return;
                }
            }
        }
    }

    async fn download(&self, item: &mut Download) -> Result<()> {
        let _lease = self.cache_lease(item.source.cache_id(), false);
        if !self.wait_background(Some(&item.id)).await? {
            return self.interrupt(item);
        }
        let mut failure = anyhow::anyhow!("no download route available");
        for (index, route) in self
            .download_routes(&item.source)
            .await
            .into_iter()
            .enumerate()
        {
            crate::event!("download {} route_attempt={}", &item.id[..12], index + 1);
            let source = route.source(&item.source);
            match self
                .download_from(item, &source, route.expected_version.as_deref())
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => {
                    let retry_route = error.is::<RouteFailure>()
                        || error.is::<crate::recovery::Retryable>()
                        || error.is::<crate::recovery::AuthRequired>();
                    failure = error;
                    if !retry_route
                        || self.shutdown.is_cancelled()
                        || self
                            .store
                            .get(&item.id)?
                            .is_none_or(|current| current.state != "downloading")
                    {
                        return Err(failure);
                    }
                    self.routes.failed(&route.origin);
                }
            }
        }
        Err(failure)
    }

    async fn download_from(
        &self,
        item: &mut Download,
        source: &Source,
        expected_version: Option<&str>,
    ) -> Result<()> {
        let gate = self.transfers.file_gate(&item.id);
        let exclusive = gate.clone().write_owned().await;
        if self
            .store
            .get(&item.id)?
            .is_none_or(|row| row.state != "downloading")
        {
            return self.interrupt(item);
        }
        let partial = self.store.file(&item.id, true);
        let complete = self.store.file(&item.id, false);
        // A crash after rename but before the database commit is recoverable.
        if complete.exists()
            && item
                .total
                .is_some_and(|n| std::fs::metadata(&complete).is_ok_and(|m| m.len() == n))
        {
            item.downloaded = item.total.unwrap();
            self.store.progress(item)?;
            self.store.transition(&item.id, "downloading", "complete")?;
            return Ok(());
        }
        item.downloaded = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
        let native = if fntv::media_guid(source).is_some() {
            let version = fntv::file_version(&self.client, source)
                .await
                .context(RouteFailure("FN route metadata request failed".into()))?;
            if expected_version.is_some_and(|expected| expected != version.fingerprint) {
                return Err(transfer_error(
                    "alternate route file version changed; use primary or retest routes",
                ));
            }
            Some(version)
        } else {
            None
        };
        let native_resume = native.as_ref().is_some_and(|version| {
            item.file_version.as_deref() == Some(version.fingerprint.as_str())
                && item.total == Some(version.size)
        });
        let validator = strong_etag(&item.etag)
            .map(str::to_owned)
            .or_else(|| item.last_modified.clone());
        let mut resume_at = if native_resume || (native.is_none() && validator.is_some()) {
            item.downloaded
        } else {
            0
        };
        // The final bytes may have landed before a crash, without the rename/DB commit.
        if resume_at > 0
            && item.total == Some(resume_at)
            && self.revalidate(source, item).await.is_some()
        {
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&partial)
                .await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&partial, &complete).await?;
            self.store.progress(item)?;
            self.store.transition(&item.id, "downloading", "complete")?;
            return Ok(());
        }
        if item.total.is_some_and(|size| resume_at >= size) {
            resume_at = 0;
        }
        let mut request = self
            .client
            .get(source.url().context("invalid saved source")?)
            .headers(source.request_headers().context("invalid saved headers")?);
        if resume_at > 0 || native.is_some() {
            request = request.header(header::RANGE, format!("bytes={resume_at}-"));
            if native.is_none() {
                request = request.header(header::IF_RANGE, validator.as_ref().unwrap());
            }
        }
        let response = tokio::select! {
            _ = self.shutdown.cancelled() => { return self.interrupt(item); },
            result = request.send() => result.map_err(|_| crate::recovery::Retryable("upstream request failed"))?,
        };
        if response.status().is_redirection() {
            return Err(transfer_error(
                "download redirected; only directly served files are supported in this prototype",
            ));
        }
        if !response.status().is_success() {
            return Err(crate::recovery::http_error(response.status().as_u16()));
        }
        if text_header(response.headers(), "content-encoding").is_some_and(|v| v != "identity") {
            return Err(transfer_error("encoded media cannot be cached"));
        }
        if !media_candidate(response.headers(), &item.source.target) {
            return Err(transfer_error(
                "response is not a supported single media file",
            ));
        }
        let new_etag = text_header(response.headers(), "etag");
        let new_modified = text_header(response.headers(), "last-modified");
        let ranged = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        let append = ranged && resume_at > 0;
        let total = if ranged {
            let (start, end, total) = text_header(response.headers(), "content-range")
                .and_then(|v| parse_content_range(&v))
                .ok_or_else(|| transfer_error("invalid upstream Content-Range"))?;
            if (resume_at == 0 && native.is_none()) || start != resume_at || end != total - 1 {
                return Err(transfer_error("upstream resumed the wrong byte range"));
            }
            let unchanged = if let Some(version) = &native {
                version.size == total && (resume_at == 0 || native_resume)
            } else if let Some(etag) = strong_etag(&item.etag) {
                new_etag.as_deref() == Some(etag)
            } else {
                item.last_modified.is_some() && item.last_modified == new_modified
            };
            if !unchanged || (resume_at > 0 && item.total.is_some_and(|old| old != total)) {
                return Err(transfer_error("upstream file changed during resume"));
            }
            total
        } else {
            if response.status() != reqwest::StatusCode::OK {
                return Err(transfer_error("upstream did not return a complete file"));
            }
            response
                .content_length()
                .ok_or_else(|| transfer_error("upstream has no file length"))?
        };
        if total == 0 {
            return Err(transfer_error("empty media file"));
        }
        if native.as_ref().is_some_and(|version| version.size != total) {
            return Err(transfer_error("FN metadata and stream length disagree"));
        }
        if native.is_none() && strong_etag(&new_etag).is_none() && new_modified.is_none() {
            return Err(transfer_error(
                "upstream has no usable ETag or Last-Modified validator",
            ));
        }
        let occupied = self.store.occupied_except(&item.id)?;
        item.total = Some(total);
        self.store.progress(item)?;
        if occupied.saturating_add(total) > self.cache_limit() {
            bail!(CapacityError);
        }
        let needed = total.saturating_sub(if append { resume_at } else { 0 });
        if fs2::available_space(&self.config.data_dir)? < needed.saturating_add(self.disk_reserve())
        {
            bail!(CapacityError);
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(&partial)
            .await
            .context("cannot open partial cache")?;
        if !append {
            item.downloaded = 0;
        }
        item.total = Some(total);
        item.etag = new_etag;
        item.last_modified = new_modified;
        item.content_type = text_header(response.headers(), "content-type");
        item.file_version = native.as_ref().map(|version| version.fingerprint.clone());
        self.store.progress(item)?;
        let growing = tokio::sync::OwnedRwLockWriteGuard::downgrade(exclusive);
        self.transfers.download_progress(&item.id, item.downloaded);
        let mut stream = response.bytes_stream();
        let mut saved_at = item.downloaded;
        let mut saved_time = std::time::Instant::now();
        let mut next_chunk = std::time::Instant::now();
        loop {
            if !self.wait_background(Some(&item.id)).await? {
                file.sync_data().await?;
                return self.interrupt(item);
            }
            let chunk = tokio::select! {
                _ = self.shutdown.cancelled() => {
                    file.sync_data().await?;
                    return self.interrupt(item);
                },
                next = stream.next() => next,
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if self.store.get(&item.id)?.is_none_or(|current| current.state != "downloading") {
                        file.sync_data().await?;
                        self.store.progress(item)?;
                        return Ok(());
                    }
                    continue;
                },
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = chunk.map_err(|_| {
                crate::recovery::Retryable("upstream body interrupted; retry to resume")
            })?;
            if self
                .store
                .get(&item.id)?
                .is_none_or(|current| current.state != "downloading")
            {
                file.sync_data().await?;
                self.store.progress(item)?;
                return Ok(());
            }
            if item.downloaded.saturating_add(chunk.len() as u64) > total {
                return Err(transfer_error("upstream sent more bytes than declared"));
            }
            // Bounded writes keep rate measurements, cancellation and visible prefixes responsive.
            for piece in chunk.chunks(64 * 1024) {
                if !self
                    .pace_download(&item.id, piece.len(), &mut next_chunk)
                    .await?
                {
                    file.sync_data().await?;
                    return self.interrupt(item);
                }
                file.write_all(piece).await.context("cache write failed")?;
                item.downloaded += piece.len() as u64;
                self.transfers.download_progress(&item.id, item.downloaded);
                self.counters
                    .origin_bytes
                    .fetch_add(piece.len() as u64, Ordering::Relaxed);
                if item.downloaded - saved_at >= 1024 * 1024
                    || saved_time.elapsed() >= Duration::from_millis(500)
                {
                    self.store.progress(item)?;
                    saved_at = item.downloaded;
                    saved_time = std::time::Instant::now();
                }
            }
        }
        if item.downloaded != total {
            return Err(
                crate::recovery::Retryable("incomplete upstream body; retry to resume").into(),
            );
        }
        if let Some(version) = native {
            let final_version = fntv::file_version(&self.client, source)
                .await
                .context(RouteFailure("FN route metadata request failed".into()))?;
            if version.fingerprint != final_version.fingerprint {
                return Err(transfer_error("FN file metadata changed during download"));
            }
        }
        file.sync_all().await?;
        drop(file);
        drop(growing);
        let _seal = gate.write_owned().await;
        if self
            .store
            .get(&item.id)?
            .is_none_or(|row| row.state != "downloading")
        {
            return self.interrupt(item);
        }
        tokio::fs::rename(&partial, &complete)
            .await
            .context("cannot commit cache file")?;
        self.store.progress(item)?;
        if self.store.transition(&item.id, "downloading", "complete")? {
            crate::event!("download {} complete: {} bytes", &item.id[..12], total);
        }
        Ok(())
    }

    fn interrupt(&self, item: &Download) -> Result<()> {
        self.store.progress(item)?;
        if self
            .store
            .get(&item.id)?
            .is_some_and(|current| current.state == "downloading")
        {
            self.store.transition(&item.id, "downloading", "queued")?;
        }
        Ok(())
    }
}
