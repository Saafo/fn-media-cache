//! Background scheduling and local-only retention. Playback records are never replayed.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    engine::Engine,
    library::{MediaItem, Session},
};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub auto_prefetch: bool,
    pub incognito: bool,
    pub local_history: bool,
    pub prefetch_count: usize,
    pub prefetch_after_seconds: u64,
    pub bytes_per_second: u64,
    pub pause_during_playback: bool,
    pub prevent_idle_sleep: bool,
    pub cleanup_enabled: bool,
    pub retention_seconds: u64,
    pub playback_cooldown_seconds: u64,
    pub reserved_disk_bytes: u64,
    pub cache_limit_bytes: Option<u64>,
    pub scheduler_interval_seconds: u64,
    pub route_result_ttl_seconds: u64,
    pub route_timeout_seconds: u64,
    pub route_sample_bytes: u64,
    pub retry_max_attempts: u32,
    pub retry_base_seconds: u64,
    pub retry_max_seconds: u64,
    pub session_check_seconds: u64,
    pub resume_detection_seconds: u64,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            auto_prefetch: false,
            incognito: false,
            local_history: false,
            prefetch_count: 2,
            prefetch_after_seconds: 60,
            bytes_per_second: 10 * 1024 * 1024,
            pause_during_playback: true,
            prevent_idle_sleep: false,
            cleanup_enabled: true,
            retention_seconds: 48 * 60 * 60,
            playback_cooldown_seconds: 2,
            reserved_disk_bytes: 64 * 1024 * 1024,
            cache_limit_bytes: None,
            scheduler_interval_seconds: 15,
            route_result_ttl_seconds: 900,
            route_timeout_seconds: 12,
            route_sample_bytes: 1024 * 1024,
            retry_max_attempts: 8,
            retry_base_seconds: 5,
            retry_max_seconds: 300,
            session_check_seconds: 30,
            resume_detection_seconds: 30,
        }
    }
}
impl Policy {
    pub fn syncs_history(&self) -> bool {
        !self.incognito && !self.local_history
    }

    pub fn validate(&self) -> Result<()> {
        if self.incognito && self.local_history {
            bail!("local history and incognito are mutually exclusive");
        }
        if !(1..=20).contains(&self.prefetch_count)
            || self.prefetch_after_seconds > 3600
            || self.bytes_per_second > 1024 * 1024 * 1024
            || self.retention_seconds > 365 * 86400
            || self.playback_cooldown_seconds > 3600
            || self.reserved_disk_bytes > i64::MAX as u64
            || self
                .cache_limit_bytes
                .is_some_and(|n| n == 0 || n > i64::MAX as u64)
            || !(1..=3600).contains(&self.scheduler_interval_seconds)
            || !(1..=86400).contains(&self.route_result_ttl_seconds)
            || !(1..=120).contains(&self.route_timeout_seconds)
            || !(1024..=1024 * 1024).contains(&self.route_sample_bytes)
            || self.retry_max_attempts > 100
            || self.retry_base_seconds == 0
            || self.retry_max_seconds < self.retry_base_seconds
            || self.retry_max_seconds > 86400
            || !(1..=3600).contains(&self.session_check_seconds)
            || !(1..=3600).contains(&self.resume_detection_seconds)
        {
            bail!("automation configuration is out of range");
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct PlaybackActivity {
    active: usize,
    last: Option<Instant>,
}

impl PlaybackActivity {
    fn start(activity: &Arc<Mutex<Self>>) -> PlaybackLease {
        activity.lock().unwrap().active += 1;
        PlaybackLease(activity.clone())
    }

    fn active(&self, cooldown: u64) -> bool {
        self.active > 0
            || self
                .last
                .is_some_and(|at| at.elapsed() < Duration::from_secs(cooldown))
    }
}

pub struct PlaybackLease(Arc<Mutex<PlaybackActivity>>);
impl Drop for PlaybackLease {
    fn drop(&mut self) {
        let mut activity = self.0.lock().unwrap();
        activity.active -= 1;
        activity.last = Some(Instant::now());
    }
}

pub struct CacheLease {
    uses: Arc<Mutex<HashMap<String, usize>>>,
    key: String,
    _playback: Option<PlaybackLease>,
    cleanup_wake: Arc<tokio::sync::Notify>,
}
impl Drop for CacheLease {
    fn drop(&mut self) {
        let mut uses = self.uses.lock().unwrap();
        if let Some(count) = uses.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                uses.remove(&self.key);
            }
        }
        self.cleanup_wake.notify_one();
    }
}

impl Engine {
    pub fn cache_lease(&self, key: String, playback: bool) -> CacheLease {
        *self
            .cache_uses
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default() += 1;
        CacheLease {
            uses: self.cache_uses.clone(),
            key,
            _playback: playback.then(|| PlaybackActivity::start(&self.playback_activity)),
            cleanup_wake: self.scheduler_wake.clone(),
        }
    }

    pub fn playing(&self) -> bool {
        let cooldown = self.policy.read().unwrap().playback_cooldown_seconds;
        self.playback_activity.lock().unwrap().active(cooldown)
    }

    pub fn origin_playback(&self) -> PlaybackLease {
        PlaybackActivity::start(&self.origin_playback_activity)
    }

    pub fn downloads_yielding(&self) -> bool {
        let policy = self.policy.read().unwrap();
        policy.pause_during_playback
            && self
                .origin_playback_activity
                .lock()
                .unwrap()
                .active(policy.playback_cooldown_seconds)
    }

    pub fn cache_limit(&self) -> u64 {
        self.policy
            .read()
            .unwrap()
            .cache_limit_bytes
            .unwrap_or(self.config.max_cache_bytes)
    }
    pub fn disk_reserve(&self) -> u64 {
        self.policy.read().unwrap().reserved_disk_bytes
    }

    pub async fn wait_background(&self, task: Option<&str>) -> Result<bool> {
        loop {
            if self.shutdown.is_cancelled() {
                return Ok(false);
            }
            if let Some(id) = task
                && self
                    .store
                    .get(id)?
                    .is_none_or(|row| row.state != "downloading")
            {
                return Ok(false);
            }
            if !self.downloads_yielding() {
                return Ok(true);
            }
            tokio::select! { _=self.shutdown.cancelled()=>return Ok(false), _=tokio::time::sleep(Duration::from_millis(100))=>{} }
        }
    }

    pub async fn pace_download(
        &self,
        task: &str,
        bytes: usize,
        next: &mut Instant,
        position_current: impl Fn() -> bool,
    ) -> Result<bool> {
        if !self.wait_background(Some(task)).await? || !position_current() {
            return Ok(false);
        }
        let limit = self.policy.read().unwrap().bytes_per_second;
        if limit == 0 {
            *next = Instant::now();
            return Ok(true);
        }
        let target =
            (*next).max(Instant::now()) + Duration::from_secs_f64(bytes as f64 / limit as f64);
        while Instant::now() < target {
            tokio::time::sleep(
                target
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50)),
            )
            .await;
            if !self.wait_background(Some(task)).await? || !position_current() {
                return Ok(false);
            }
        }
        *next = target;
        Ok(true)
    }

    pub async fn next_episodes(
        &self,
        session: &Session,
        current: &MediaItem,
        count: usize,
        local_history: bool,
    ) -> Result<Vec<MediaItem>> {
        if current.kind != "Episode" || current.season_number == 0 || current.episode_number == 0 {
            return Ok(vec![]);
        }
        let season = self.catalog_item(session, &current.parent_guid).await?;
        if season.kind != "Season" {
            return Ok(vec![]);
        }
        let mut seasons = self
            .catalog_children(session, &season.parent_guid, 1)
            .await?
            .items;
        seasons.retain(|s| {
            s.kind == "Season" && s.season_number >= current.season_number && s.season_number > 0
        });
        seasons.sort_by_key(|s| s.season_number);
        let mut selected = Vec::new();
        for season in seasons {
            let episodes = self.catalog_children(session, &season.guid, 1).await?.items;
            let mut numbers = std::collections::BTreeMap::<u64, Vec<MediaItem>>::new();
            for item in episodes {
                if item.kind == "Episode"
                    && item.parent_guid == season.guid
                    && item.season_number == season.season_number
                    && item.episode_number > 0
                {
                    numbers.entry(item.episode_number).or_default().push(item);
                }
            }
            for (number, items) in numbers {
                if season.season_number == current.season_number && number <= current.episode_number
                {
                    continue;
                }
                // Duplicate episode numbers may represent alternate editions. Do not guess one.
                if items.len() != 1 {
                    continue;
                }
                let item = items.into_iter().next().unwrap();
                if (!local_history && item.watched != Some(false))
                    || self.store.suppressed(&session.id, &item.guid)?
                {
                    continue;
                }
                selected.push(item);
                if selected.len() == count {
                    return Ok(selected);
                }
            }
        }
        Ok(selected)
    }

    async fn process_playback(&self) -> Result<()> {
        let policy = self.policy.read().unwrap().clone();
        if policy.incognito {
            self.store.clear_playback_work()?;
            return Ok(());
        }
        for work in self.store.playback_work()?.into_iter().take(10) {
            let session = self.session(Some(&work.session_id))?;
            let current = match self.catalog_item(&session, &work.item_guid).await {
                Ok(item) => item,
                Err(_) => continue,
            };
            {
                let active = self.policy.read().unwrap();
                if active.incognito || active.local_history != policy.local_history {
                    return Ok(());
                }
                if !active.local_history {
                    self.store.update_completion(
                        &session.id,
                        &current.guid,
                        current.watched,
                        now(),
                    )?;
                }
            }
            let eligible = {
                let active = self.policy.read().unwrap();
                self.prefetch_watch.lock().unwrap().eligible(
                    &work.session_id,
                    &work.item_guid,
                    &active,
                    Instant::now(),
                )
            };
            if eligible {
                for item in self
                    .next_episodes(
                        &session,
                        &current,
                        policy.prefetch_count,
                        policy.local_history,
                    )
                    .await?
                {
                    if self
                        .store
                        .managed()?
                        .iter()
                        .any(|m| m.session_id == session.id && m.item.guid == item.guid)
                    {
                        continue;
                    }
                    self.queue_catalog_item(&session, &item.guid, Some(&work.item_guid), false)
                        .await?;
                }
            }
            self.store.finish_playback_work(&work)?;
        }
        Ok(())
    }

    pub fn cleanup_once(&self, at: i64) -> Result<usize> {
        let policy = self.policy.read().unwrap().clone();
        // Finish a committed deletion after a crash even if the policy changed meanwhile.
        let mut candidates = self.store.pending_cleanup()?;
        if policy.cleanup_enabled {
            candidates.extend(self.store.cleanup_candidates(
                at,
                policy.retention_seconds,
                policy.local_history,
            )?);
        }
        candidates.sort();
        candidates.dedup();
        let mut removed = 0;
        for id in candidates {
            let Some(item) = self.store.get(&id)? else {
                continue;
            };
            if id.len() != 64 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
                bail!("invalid local cache identity");
            }
            let Ok(_file_guard) = self.transfers.file_gate(&id).try_write_owned() else {
                continue;
            };
            let key = item.source.cache_id();
            let uses = self.cache_uses.lock().unwrap();
            if uses.get(&key).copied().unwrap_or(0) > 0
                || !self.store.begin_cleanup(
                    &id,
                    at,
                    policy.retention_seconds,
                    policy.local_history,
                )?
            {
                continue;
            }
            let mut files = vec![self.store.file(&id, false), self.store.file(&id, true)];
            for segment in self.store.segments(&key)? {
                if segment.checksum.len() != 64
                    || !segment.checksum.bytes().all(|c| c.is_ascii_hexdigit())
                {
                    bail!("invalid local segment identity");
                }
                files.push(self.store.segment_file(&segment));
            }
            for file in files {
                if let Err(error) = std::fs::remove_file(file)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    bail!("cannot remove local cache file");
                }
            }
            self.store.finish_cleanup(&id, &key)?;
            crate::event!("cache {} removed after retention", &id[..12]);
            drop(uses);
            removed += 1;
        }
        Ok(removed)
    }

    fn retry_space(&self) -> Result<()> {
        for item in self
            .store
            .list()?
            .into_iter()
            .filter(|row| row.state == "waiting_space")
        {
            let Some(total) = item.total else {
                continue;
            };
            if self.store.occupied_except(&item.id)?.saturating_add(total) <= self.cache_limit()
                && fs2::available_space(&self.config.data_dir)?
                    > total
                        .saturating_sub(item.downloaded)
                        .saturating_add(self.disk_reserve())
            {
                if !self.store.transition(&item.id, "waiting_space", "queued")? {
                    continue;
                }
                crate::event!("download {} available space restored", &item.id[..12]);
                self.wake.notify_one();
            }
        }
        Ok(())
    }

    pub async fn scheduler(self: Arc<Self>) {
        let mut cursor = 0usize;
        let mut last_tick = Instant::now();
        let mut last_wall = now();
        loop {
            if self.shutdown.is_cancelled() {
                return;
            }
            let wall = now();
            let threshold = self.policy.read().unwrap().resume_detection_seconds;
            let elapsed = last_tick.elapsed().as_secs();
            if crate::recovery::resumed(elapsed, wall - last_wall, threshold) {
                crate::event!("scheduler resumed after time gap; retry deadlines refreshed");
                let _ = self.store.wake_retries();
                self.wake.notify_one();
            }
            last_tick = Instant::now();
            last_wall = wall;
            let _ = self.refresh_sessions().await;
            if let Err(_error) = self.process_playback().await {
                crate::event!("playback scheduling deferred; metadata unavailable");
            }
            // Reconcile a bounded rotating set, including items whose completed record left play/list.
            if self.policy.read().unwrap().syncs_history()
                && let Ok(managed) = self.store.managed()
            {
                for offset in 0..managed.len().min(4) {
                    let item = &managed[(cursor + offset) % managed.len()];
                    if let Ok(session) = self.session(Some(&item.session_id))
                        && let Ok(current) = self.catalog_item(&session, &item.item.guid).await
                        && self.policy.read().unwrap().syncs_history()
                    {
                        let _ = self.store.update_completion(
                            &session.id,
                            &current.guid,
                            current.watched,
                            now(),
                        );
                    }
                }
                cursor = cursor.wrapping_add(4);
            }
            if self.cleanup_once(now()).is_err() {
                crate::event!("local cache cleanup deferred");
            }
            let _ = self.retry_space();
            let interval = self.policy.read().unwrap().scheduler_interval_seconds;
            tokio::select! { _=self.shutdown.cancelled()=>return, _=self.scheduler_wake.notified()=>{}, _=tokio::time::sleep(Duration::from_secs(interval))=>{} }
        }
    }
}
