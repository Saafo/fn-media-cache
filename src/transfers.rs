//! Transient transfer measurements and per-file generation guards. No playback history is persisted.
use crate::{library::MediaItem, store::Download};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::RwLock;

#[derive(Clone, Serialize)]
pub struct Playback {
    pub id: String,
    pub download_id: Option<String>,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<MediaItem>,
    pub local_bytes: u64,
    pub upstream_bytes: u64,
    pub active_requests: usize,
    pub last_seen_ms: u64,
}
#[derive(Default)]
struct State {
    generation: u64,
    playback: HashMap<String, Playback>,
    downloads: HashMap<String, VecDeque<(Instant, u64)>>,
}
#[derive(Default)]
pub struct Transfers {
    state: Mutex<State>,
    gates: Mutex<HashMap<String, Arc<RwLock<()>>>>,
}
pub struct Reading {
    transfers: Arc<Transfers>,
    id: String,
    generation: u64,
}
fn now_ms() -> u64 {
    crate::scheduler::now().max(0) as u64 * 1000
}
impl Transfers {
    pub fn playback_generation(&self) -> u64 {
        self.state.lock().unwrap().generation
    }

    pub fn file_gate(&self, id: &str) -> Arc<RwLock<()>> {
        self.gates
            .lock()
            .unwrap()
            .entry(id.into())
            .or_default()
            .clone()
    }
    pub fn clear_playback(&self) {
        let mut state = self.state.lock().unwrap();
        state.playback.clear();
        state.generation = state.generation.wrapping_add(1);
    }
    pub fn start(
        self: &Arc<Self>,
        id: String,
        download_id: Option<String>,
        path: String,
        item: Option<MediaItem>,
    ) -> Reading {
        let mut state = self.state.lock().unwrap();
        if state.playback.len() >= 100
            && !state.playback.contains_key(&id)
            && let Some(old) = state
                .playback
                .values()
                .filter(|p| p.active_requests == 0)
                .min_by_key(|p| p.last_seen_ms)
                .map(|p| p.id.clone())
        {
            state.playback.remove(&old);
        }
        let entry = state.playback.entry(id.clone()).or_insert(Playback {
            id: id.clone(),
            download_id: download_id.clone(),
            path,
            item: None,
            local_bytes: 0,
            upstream_bytes: 0,
            active_requests: 0,
            last_seen_ms: now_ms(),
        });
        entry.download_id = download_id.or(entry.download_id.clone());
        if item.is_some() {
            entry.item = item;
        }
        entry.active_requests += 1;
        entry.last_seen_ms = now_ms();
        Reading {
            transfers: self.clone(),
            id,
            generation: state.generation,
        }
    }
    pub fn playback(&self) -> Vec<Playback> {
        let mut result: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .playback
            .values()
            .cloned()
            .collect();
        result.sort_by_key(|p| std::cmp::Reverse(p.last_seen_ms));
        result
    }
    pub fn download_progress(&self, id: &str, bytes: u64) {
        let mut state = self.state.lock().unwrap();
        let samples = state.downloads.entry(id.into()).or_default();
        if samples.back().is_some_and(|(_, old)| bytes < *old) {
            samples.clear();
        }
        samples.push_back((Instant::now(), bytes));
        while samples.len() > 2
            && (samples.front().unwrap().0.elapsed() > Duration::from_secs(5)
                || samples.len() > 256)
        {
            samples.pop_front();
        }
    }
    pub fn download_view(&self, item: Download, yielding: bool) -> DownloadView {
        let state = self.state.lock().unwrap();
        let speed = if item.state == "downloading" && !yielding {
            state
                .downloads
                .get(&item.id)
                .and_then(|samples| Some((samples.front()?, samples.back()?)))
                .filter(|(_, last)| last.0.elapsed() < Duration::from_secs(2))
                .map(|(first, last)| {
                    (last.1.saturating_sub(first.1) as f64
                        / first.0.elapsed().as_secs_f64().max(0.1)) as u64
                })
                .unwrap_or(0)
        } else {
            0
        };
        let eta = (speed > 0)
            .then(|| {
                item.total
                    .map(|total| total.saturating_sub(item.downloaded).div_ceil(speed))
            })
            .flatten();
        let waiting_reason = (item.state == "downloading" && yielding).then_some("playback");
        DownloadView {
            download: item,
            bytes_per_second: speed,
            eta_seconds: eta,
            waiting_reason,
        }
    }
}
impl Reading {
    pub fn bytes(&self, local: u64, upstream: u64) {
        let mut state = self.transfers.state.lock().unwrap();
        if state.generation != self.generation {
            return;
        }
        if let Some(entry) = state.playback.get_mut(&self.id) {
            entry.local_bytes += local;
            entry.upstream_bytes += upstream;
            entry.last_seen_ms = now_ms();
        }
    }
}
impl Drop for Reading {
    fn drop(&mut self) {
        let mut state = self.transfers.state.lock().unwrap();
        if state.generation != self.generation {
            return;
        }
        if let Some(entry) = state.playback.get_mut(&self.id) {
            entry.active_requests = entry.active_requests.saturating_sub(1);
            entry.last_seen_ms = now_ms();
        }
    }
}
#[derive(Serialize)]
pub struct DownloadView {
    #[serde(flatten)]
    download: Download,
    bytes_per_second: u64,
    eta_seconds: Option<u64>,
    waiting_reason: Option<&'static str>,
}
#[derive(Clone)]
pub struct LocalPrefix(pub u64);
