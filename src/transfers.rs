//! Transient transfer measurements and per-file generation guards. No playback history is persisted.
use crate::{library::MediaItem, store::Download};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
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

#[derive(Clone, Copy, Default, Serialize)]
pub struct PlaybackTotals {
    local_bytes: u64,
    upstream_bytes: u64,
}

#[derive(Default)]
struct State {
    generation: u64,
    playback: HashMap<String, Playback>,
    playback_totals: PlaybackTotals,
    downloads: HashMap<String, DownloadRates>,
}

pub enum DownloadSource {
    Background,
    Playback,
}

#[derive(Default)]
struct DownloadRates {
    background: Rate,
    playback: Rate,
}

#[derive(Default)]
struct Rate(VecDeque<(Instant, u64)>);

impl Rate {
    fn record(&mut self, bytes: u64) {
        let now = Instant::now();
        if self
            .0
            .back()
            .is_some_and(|(at, _)| at.elapsed() >= Duration::from_secs(2))
        {
            self.0.clear();
        }
        let previous = self.0.back().map_or(0, |(_, bytes)| *bytes);
        if self.0.is_empty() {
            self.0.push_back((now, 0));
        }
        self.0.push_back((now, previous + bytes));
        while self.0.len() > 2
            && (self.0.front().unwrap().0.elapsed() > Duration::from_secs(5) || self.0.len() > 256)
        {
            self.0.pop_front();
        }
    }

    fn speed(&self) -> u64 {
        self.0
            .front()
            .zip(self.0.back())
            .filter(|(_, last)| last.0.elapsed() < Duration::from_secs(2))
            .map_or(0, |(first, last)| {
                ((last.1 - first.1) as f64 / first.0.elapsed().as_secs_f64().max(0.1)) as u64
            })
    }
}
#[derive(Clone, Default)]
struct FileLocks {
    generation: Arc<RwLock<()>>,
    append: Arc<FileAppend>,
}

#[derive(Default)]
pub struct FileAppend {
    pub writer: Arc<tokio::sync::Mutex<()>>,
    playback_revision: AtomicU64,
}

impl FileAppend {
    pub fn revision(&self) -> u64 {
        self.playback_revision.load(Ordering::Relaxed)
    }

    pub fn playback_write(&self) {
        self.playback_revision.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct Transfers {
    state: Mutex<State>,
    files: Mutex<HashMap<String, FileLocks>>,
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
        self.file_locks(id).generation
    }

    pub fn file_append(&self, id: &str) -> Arc<FileAppend> {
        self.file_locks(id).append
    }

    fn file_locks(&self, id: &str) -> FileLocks {
        self.files
            .lock()
            .unwrap()
            .entry(id.into())
            .or_default()
            .clone()
    }
    pub fn clear_playback(&self) {
        let mut state = self.state.lock().unwrap();
        state.playback.clear();
        state.playback_totals = PlaybackTotals::default();
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
    pub fn playback(&self) -> (PlaybackTotals, Vec<Playback>) {
        let (totals, mut rows) = {
            let state = self.state.lock().unwrap();
            (
                state.playback_totals,
                state.playback.values().cloned().collect::<Vec<_>>(),
            )
        };
        rows.sort_by_key(|p| std::cmp::Reverse(p.last_seen_ms));
        (totals, rows)
    }
    pub fn download_bytes(&self, id: &str, bytes: u64, source: DownloadSource) {
        let mut state = self.state.lock().unwrap();
        let rates = state.downloads.entry(id.into()).or_default();
        let rate = match source {
            DownloadSource::Background => &mut rates.background,
            DownloadSource::Playback => &mut rates.playback,
        };
        rate.record(bytes);
    }
    pub fn download_view(&self, item: Download, yielding: bool) -> DownloadView {
        let state = self.state.lock().unwrap();
        let speed = if item.state == "downloading" {
            state.downloads.get(&item.id).map_or(0, |rates| {
                rates.playback.speed()
                    + if yielding {
                        0
                    } else {
                        rates.background.speed()
                    }
            })
        } else {
            0
        };
        let eta = (speed > 0)
            .then(|| {
                item.total
                    .map(|total| total.saturating_sub(item.downloaded).div_ceil(speed))
            })
            .flatten();
        let waiting_reason =
            (item.state == "downloading" && yielding && speed == 0).then_some("playback");
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
            state.playback_totals.local_bytes += local;
            state.playback_totals.upstream_bytes += upstream;
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
