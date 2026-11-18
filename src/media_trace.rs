use std::{sync::Arc, time::Instant};

use axum::{
    http::{HeaderMap, HeaderName, header},
    response::Response,
};

use crate::{protocol, transfers::Transfers};

pub struct Request {
    started: Instant,
    ranges: String,
    generation: u64,
}

impl Request {
    pub fn new(headers: &HeaderMap, generation: u64) -> Self {
        Self {
            started: Instant::now(),
            ranges: safe_range(headers, header::RANGE, "bytes=", b"-, "),
            generation,
        }
    }

    pub fn response(
        self,
        transfers: Arc<Transfers>,
        source: &protocol::Source,
        response: &Response,
    ) -> Transfer {
        let headers = response.headers();
        let content_range = safe_range(headers, header::CONTENT_RANGE, "bytes ", b"-/*");
        let expected = headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .or_else(|| {
                protocol::parse_content_range(&content_range).map(|(start, end, _)| end - start + 1)
            });
        let cache = match headers
            .get("x-fn-media-cache")
            .map(|value| value.as_bytes())
        {
            Some(b"HIT") => "cache",
            Some(b"MISS") => "origin",
            _ => "proxy",
        };
        let source_kind = if response
            .extensions()
            .get::<crate::transfers::LocalPrefix>()
            .is_some()
        {
            "partial"
        } else {
            cache
        };
        Transfer {
            request: self,
            transfers,
            media_id: source.cache_id(),
            status: response.status().as_u16(),
            source_kind,
            content_range,
            expected,
            bytes: 0,
            local: 0,
            end_reached: false,
            failed: false,
        }
    }
}

pub struct Transfer {
    request: Request,
    transfers: Arc<Transfers>,
    media_id: String,
    status: u16,
    source_kind: &'static str,
    content_range: String,
    expected: Option<u64>,
    bytes: u64,
    local: u64,
    end_reached: bool,
    failed: bool,
}

impl Transfer {
    pub fn chunk(&mut self, bytes: u64, local: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
        self.local = self.local.saturating_add(local);
    }

    pub fn finish(&mut self, failed: bool) {
        self.failed |= failed;
        self.end_reached = true;
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if self.transfers.playback_generation() != self.request.generation {
            return;
        }
        let outcome = if self.failed {
            "body_error"
        } else if self
            .expected
            .map_or(self.end_reached, |size| size == self.bytes)
        {
            "complete"
        } else if self.end_reached {
            "short_body"
        } else {
            "closed_early"
        };
        crate::event!(
            "media transfer id={} status={} source={} range={:?} content_range={:?} bytes={} local_bytes={} origin_bytes={} outcome={} elapsed_ms={}",
            &self.media_id[..12],
            self.status,
            self.source_kind,
            self.request.ranges,
            self.content_range,
            self.bytes,
            self.local,
            self.bytes.saturating_sub(self.local),
            outcome,
            self.request.started.elapsed().as_millis()
        );
    }
}

fn safe_range(headers: &HeaderMap, key: HeaderName, prefix: &str, punctuation: &[u8]) -> String {
    let Some(raw) = headers.get(key) else {
        return "-".to_owned();
    };
    if let Ok(raw) = raw.to_str()
        && raw.len() <= 128
        && raw.strip_prefix(prefix).is_some_and(|value| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || punctuation.contains(&byte))
        })
    {
        raw.to_owned()
    } else {
        "invalid".to_owned()
    }
}
