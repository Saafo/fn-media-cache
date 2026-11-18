//! Bounded real-upstream experiments. No full download or playback-state writes.
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use axum::http::header;
use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    config::private_dir,
    engine::Engine,
    protocol::{Source, hex, media_candidate, parse_content_range, text_header},
};

const SAMPLE_BYTES: u64 = 1024 * 1024;

#[derive(Serialize)]
pub struct ProbeResult {
    pub total_bytes: u64,
    pub upstream_sample_bytes: u64,
    pub samples: Vec<SampleResult>,
    pub repeat_matches: bool,
    pub head_status: Option<u16>,
    pub etag_present: bool,
    pub last_modified_present: bool,
    pub progress_payload_identical: Option<bool>,
}

#[derive(Serialize)]
pub struct SampleResult {
    pub label: &'static str,
    pub start: u64,
    pub bytes: usize,
    pub sha256: String,
    pub local_read_matches: bool,
}

pub(crate) struct Sample {
    pub bytes: Vec<u8>,
    pub total: u64,
    pub content_type: String,
    etag: Option<String>,
    modified: Option<String>,
}

impl Engine {
    pub async fn probe(&self, source: &Source, progress: Option<&Source>) -> Result<ProbeResult> {
        let _guard = self.probe_lock.lock().await;
        let directory = self.config.data_dir.join("probe");
        private_dir(&directory)?;
        let before = self
            .progress_snapshot(progress, "progress-before.json")
            .await;
        let head_status = match self
            .client
            .head(source.url()?)
            .headers(source.request_headers()?)
            .send()
            .await
        {
            Ok(response) => Some(response.status().as_u16()),
            Err(_) => None,
        };
        let first = self.probe_range(source, 0, SAMPLE_BYTES - 1, None).await?;
        let mut consumed = first.bytes.len() as u64;
        let mut results = vec![];
        for (label, start) in [
            ("start", 0),
            ("middle", first.total / 2),
            ("end", first.total.saturating_sub(SAMPLE_BYTES)),
        ] {
            let next;
            let sample = if start == 0 && label == "start" {
                &first
            } else {
                next = self
                    .probe_range(
                        source,
                        start,
                        (start + SAMPLE_BYTES - 1).min(first.total - 1),
                        Some(first.total),
                    )
                    .await?;
                consumed += next.bytes.len() as u64;
                if next.etag != first.etag || next.modified != first.modified {
                    bail!("file validators changed during probe");
                }
                &next
            };
            let file = directory.join(format!("{label}.sample"));
            tokio::fs::write(&file, &sample.bytes).await?;
            let local = tokio::fs::read(&file).await?;
            results.push(SampleResult {
                label,
                start,
                bytes: local.len(),
                sha256: hex(&Sha256::digest(&local)),
                local_read_matches: local == sample.bytes,
            });
        }
        let repeated = self
            .probe_range(source, 0, SAMPLE_BYTES - 1, Some(first.total))
            .await?;
        consumed += repeated.bytes.len() as u64;
        let after = self
            .progress_snapshot(progress, "progress-after.json")
            .await;
        Ok(ProbeResult {
            total_bytes: first.total,
            upstream_sample_bytes: consumed,
            samples: results,
            repeat_matches: first.bytes == repeated.bytes
                && first.etag == repeated.etag
                && first.modified == repeated.modified,
            head_status,
            etag_present: first.etag.is_some(),
            last_modified_present: first.modified.is_some(),
            progress_payload_identical: before.zip(after).map(|(a, b)| a == b),
        })
    }

    pub(crate) async fn probe_range(
        &self,
        source: &Source,
        start: u64,
        end: u64,
        expected_total: Option<u64>,
    ) -> Result<Sample> {
        let response = self
            .client
            .get(source.url()?)
            .headers(source.request_headers()?)
            .header(header::RANGE, format!("bytes={start}-{end}"))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("range probe request failed"))?;
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            bail!(
                "range probe requires HTTP 206; received {}",
                response.status().as_u16()
            );
        }
        if !media_candidate(response.headers(), &source.target)
            || text_header(response.headers(), "content-encoding").is_some_and(|v| v != "identity")
        {
            bail!("range probe returned an unsupported representation");
        }
        let (actual_start, actual_end, total) = text_header(response.headers(), "content-range")
            .and_then(|v| parse_content_range(&v))
            .context("probe has no valid Content-Range")?;
        if actual_start != start
            || actual_end != end.min(total - 1)
            || expected_total.is_some_and(|n| n != total)
        {
            bail!("probe returned an unexpected byte range");
        }
        let expected = actual_end - actual_start + 1;
        if expected > SAMPLE_BYTES {
            bail!("probe exceeds sample limit");
        }
        let etag = text_header(response.headers(), "etag");
        let modified = text_header(response.headers(), "last-modified");
        let content_type = text_header(response.headers(), "content-type")
            .unwrap_or_else(|| "application/octet-stream".into());
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::with_capacity(expected as usize);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| anyhow::anyhow!("probe body interrupted"))?;
            if bytes.len() as u64 + chunk.len() as u64 > expected {
                bail!("probe body exceeds declared byte range");
            }
            self.counters
                .origin_bytes
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() as u64 != expected {
            bail!("probe body was incomplete");
        }
        Ok(Sample {
            bytes,
            total,
            content_type,
            etag,
            modified,
        })
    }

    async fn progress_snapshot(
        &self,
        source: Option<&Source>,
        filename: &str,
    ) -> Option<serde_json::Value> {
        if !self.policy.read().unwrap().syncs_history() {
            return None;
        }
        let source = source?;
        // Only the observed, read-only playback-list request may be replayed here.
        if source.display_path() != "/v/api/v1/play/list" {
            return None;
        }
        let response = self
            .client
            .get(source.url().ok()?)
            .headers(source.request_headers().ok()?)
            .send()
            .await
            .ok()?;
        if response.status() != reqwest::StatusCode::OK {
            return None;
        }
        let mut stream = response.bytes_stream();
        let mut bytes = vec![];
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.ok()?;
            if bytes.len() + chunk.len() > 256 * 1024 {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = serde_json::from_slice(&bytes).ok()?;
        tokio::fs::write(self.config.data_dir.join("probe").join(filename), bytes)
            .await
            .ok()?;
        Some(value)
    }
}
