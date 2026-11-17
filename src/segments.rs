//! Persist a bounded sample as real range-cache entries, without claiming a full file.
use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    engine::Engine,
    fntv,
    protocol::{Source, hex},
    routes::{RouteFailure, transfer_error},
    store::Segment,
};

#[derive(Serialize)]
pub struct SampleCacheResult {
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub file_version: String,
    pub segments: Vec<Segment>,
    pub download_origin: String,
}

impl Engine {
    pub async fn cache_samples(&self, source: &Source) -> Result<SampleCacheResult> {
        let _lease = self.cache_lease(source.cache_id(), false);
        let route = self
            .download_routes(source)
            .await
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no sample download route available"))?;
        let _guard = self.probe_lock.lock().await;
        // Keep the sample operation's 3 MiB budget even when a route fails.
        let result = self
            .cache_samples_from(
                source,
                &route.source(source),
                route.expected_version.as_deref(),
            )
            .await;
        if result
            .as_ref()
            .is_err_and(|error| error.downcast_ref::<RouteFailure>().is_some())
        {
            self.routes.failed(&route.origin);
        }
        result
    }

    async fn cache_samples_from(
        &self,
        logical: &Source,
        source: &Source,
        expected_version: Option<&str>,
    ) -> Result<SampleCacheResult> {
        let version = fntv::file_version(&self.client, source)
            .await
            .context(RouteFailure("FN route metadata request failed".into()))?;
        if expected_version.is_some_and(|expected| expected != version.fingerprint) {
            return Err(transfer_error("alternate route file version changed"));
        }
        let sample_size = 1024 * 1024u64;
        let required = version.size.min(sample_size).saturating_mul(3);
        if self.store.occupied_except("")?.saturating_add(required) > self.cache_limit() {
            bail!("sample cache would exceed quota");
        }
        if fs2::available_space(&self.config.data_dir)?
            < required.saturating_add(self.disk_reserve())
        {
            bail!("not enough free disk space for sample cache");
        }
        let mut segments = vec![];
        let mut downloaded = 0;
        for start in [
            0,
            version.size / 2,
            version.size.saturating_sub(sample_size),
        ] {
            if segments.iter().any(|s: &Segment| s.start == start) {
                continue;
            }
            let sample = self
                .probe_range(
                    source,
                    start,
                    (start + sample_size - 1).min(version.size - 1),
                    Some(version.size),
                )
                .await
                .context(RouteFailure("sample range request failed".into()))?;
            downloaded += sample.bytes.len() as u64;
            let segment = Segment {
                id: logical.cache_id(),
                start,
                length: sample.bytes.len() as u64,
                total: version.size,
                file_version: version.fingerprint.clone(),
                checksum: hex(&Sha256::digest(&sample.bytes)),
                content_type: sample.content_type,
            };
            let file = self.store.segment_file(&segment);
            if !file.exists() {
                let staging = file.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
                let mut handle = tokio::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&staging)
                    .await?;
                tokio::io::AsyncWriteExt::write_all(&mut handle, &sample.bytes).await?;
                handle.sync_all().await?;
                drop(handle);
                tokio::fs::rename(staging, &file).await?;
            }
            segments.push(segment);
        }
        let after = fntv::file_version(&self.client, source)
            .await
            .context(RouteFailure("FN route metadata request failed".into()))?;
        if after.fingerprint != version.fingerprint {
            return Err(transfer_error("FN file metadata changed while sampling"));
        }
        self.store.save_segments(&segments)?;
        crate::event!(
            "sample cache {} saved bytes={}",
            &logical.cache_id()[..12],
            downloaded
        );
        Ok(SampleCacheResult {
            total_bytes: version.size,
            downloaded_bytes: downloaded,
            file_version: version.fingerprint,
            segments,
            download_origin: source.origin.clone(),
        })
    }
}
