use std::{
    collections::VecDeque,
    net::IpAddr,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    http::{Method, header},
    response::Response,
};
use serde::Serialize;

use crate::protocol::{parse_content_range, text_header};

const LIMIT: usize = 100;

#[derive(Clone, Serialize)]
pub struct ProxyRequest {
    id: u64,
    timestamp_ms: u64,
    method: String,
    client_ip: IpAddr,
    path: String,
    status: Option<u16>,
    total: Option<u64>,
    observation_id: Option<String>,
    pending: bool,
}

#[derive(Default)]
pub struct History {
    next_id: u64,
    entries: VecDeque<ProxyRequest>,
}

impl History {
    pub fn start<'a>(
        history: &'a Mutex<Self>,
        method: &Method,
        path: &str,
        client_ip: IpAddr,
    ) -> Pending<'a> {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |time| time.as_millis() as u64);
        let mut state = history.lock().unwrap();
        let id = state.next_id;
        state.next_id += 1;
        state.entries.push_front(ProxyRequest {
            id,
            timestamp_ms,
            method: method.as_str().chars().take(32).collect(),
            client_ip,
            path: path.chars().take(1024).collect(),
            status: None,
            total: None,
            observation_id: None,
            pending: true,
        });
        state.entries.truncate(LIMIT);
        Pending {
            history,
            id: Some(id),
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn snapshot(&self) -> Vec<ProxyRequest> {
        self.entries.iter().cloned().collect()
    }
}

#[derive(Clone)]
pub struct ObservedMedia(pub String);

pub struct Pending<'a> {
    history: &'a Mutex<History>,
    id: Option<u64>,
}

impl Pending<'_> {
    pub fn finish(mut self, response: &Response) {
        self.update(|entry| {
            entry.status = Some(response.status().as_u16());
            entry.total = text_header(response.headers(), header::CONTENT_RANGE.as_str())
                .and_then(|range| parse_content_range(&range).map(|(_, _, total)| total))
                .or_else(|| {
                    text_header(response.headers(), header::CONTENT_LENGTH.as_str())
                        .and_then(|length| length.parse().ok())
                });
            entry.observation_id = response
                .extensions()
                .get::<ObservedMedia>()
                .map(|media| media.0.clone());
        });
    }

    fn update(&mut self, update: impl FnOnce(&mut ProxyRequest)) {
        let Some(id) = self.id.take() else {
            return;
        };
        let mut state = self.history.lock().unwrap();
        if let Some(entry) = state.entries.iter_mut().find(|entry| entry.id == id) {
            entry.pending = false;
            update(entry);
        }
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.update(|_| {});
    }
}
