//! Serve a stable downloaded prefix; request only the missing suffix from the origin.
use crate::{
    engine::Engine,
    protocol::{Range, Source, parse_content_range, parse_range, strong_etag, text_header},
    transfers::{DownloadSource, LocalPrefix},
};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Method, Response, StatusCode, header},
};
use futures_util::{StreamExt, TryStreamExt};
use std::{
    io::Write,
    sync::{Arc, atomic::Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::OwnedRwLockReadGuard,
};
use tokio_util::io::ReaderStream;

struct PlaybackAppend {
    engine: Arc<Engine>,
    file_guard: Arc<OwnedRwLockReadGuard<()>>,
    id: String,
    total: u64,
    check_capacity: bool,
}

impl PlaybackAppend {
    async fn write(&mut self, offset: u64, chunk: Bytes) -> bool {
        let engine = self.engine.clone();
        let id = self.id.clone();
        let total = self.total;
        let check_capacity = self.check_capacity;
        let file_guard = self.file_guard.clone();
        let append = engine.transfers.file_append(&id);
        let append_guard = append.writer.clone().lock_owned().await;
        // Keep both guards until the write finishes, even if the client disconnects.
        let saved = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let (_file_guard, _append_guard) = (file_guard, append_guard);
            if engine.shutdown.is_cancelled()
                || engine
                    .store
                    .get(&id)?
                    .is_none_or(|item| item.state != "downloading")
            {
                return Ok(false);
            }
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(engine.store.file(&id, true))?;
            let length = file.metadata()?.len();
            if offset > length {
                return Ok(false);
            }
            if check_capacity {
                engine.check_download_capacity(&id, total, length)?;
            }
            let skip = length - offset;
            if skip < chunk.len() as u64 {
                append.playback_write();
                let bytes = &chunk[skip as usize..];
                file.write_all(bytes)?;
                engine.store.checkpoint(&id, offset + chunk.len() as u64)?;
                engine
                    .transfers
                    .download_bytes(&id, bytes.len() as u64, DownloadSource::Playback);
            }
            Ok(true)
        })
        .await;
        if matches!(saved, Ok(Ok(true))) {
            self.check_capacity = false;
            true
        } else {
            false
        }
    }
}

pub async fn cached_prefix(
    engine: &Arc<Engine>,
    source: &Source,
    method: &Method,
    incoming: &HeaderMap,
) -> Option<Response<Body>> {
    if [
        "if-range",
        "if-none-match",
        "if-modified-since",
        "if-match",
        "if-unmodified-since",
    ]
    .iter()
    .any(|key| incoming.contains_key(*key))
    {
        return None;
    }
    let item = engine.store.get_for_source(source).ok()??;
    if matches!(item.state.as_str(), "complete" | "removed" | "deleting") || item.downloaded == 0 {
        return None;
    }
    let gate = Arc::new(engine.transfers.file_gate(&item.id).read_owned().await);
    // Re-read after waiting for a writer to publish its new generation.
    let item = engine.store.get(&item.id).ok()??;
    if matches!(item.state.as_str(), "complete" | "removed" | "deleting") {
        return None;
    }
    let total = item.total?;
    let mut file = tokio::fs::File::open(engine.store.file(&item.id, true))
        .await
        .ok()?;
    let available = item
        .downloaded
        .min(file.metadata().await.ok()?.len())
        .min(total);
    let (start, end, status) = match parse_range(
        incoming.get(header::RANGE).and_then(|v| v.to_str().ok()),
        total,
    ) {
        Range::Full if total > 0 => (0, total - 1, StatusCode::OK),
        Range::Partial(start, end) => (start, end, StatusCode::PARTIAL_CONTENT),
        _ => return None,
    };
    if start > available
        || (start == available
            && (*method == Method::HEAD || available == 0 || item.state != "downloading"))
    {
        return None;
    }
    let tail_start = (end + 1).min(available);
    let local_length = tail_start - start;
    let missing = method != Method::HEAD && tail_start <= end;
    if missing
        && incoming
            .get("x-fn-media-cache-only")
            .is_some_and(|value| value == "1")
    {
        return None;
    }
    let verified = engine.revalidate(source, &item).await?;
    let tail = if missing {
        let mut request = engine
            .client
            .get(source.url().ok()?)
            .headers(source.request_headers().ok()?)
            .header(header::RANGE, format!("bytes={tail_start}-{end}"));
        if item.file_version.is_none()
            && let Some(validator) = strong_etag(&item.etag).or(item.last_modified.as_deref())
        {
            request = request.header(header::IF_RANGE, validator);
        }
        let tail = request.send().await.ok()?;
        if tail.status() != StatusCode::PARTIAL_CONTENT
            || text_header(tail.headers(), "content-range").and_then(|v| parse_content_range(&v))
                != Some((tail_start, end, total))
            || tail
                .content_length()
                .is_some_and(|length| length != end + 1 - tail_start)
            || text_header(tail.headers(), "content-encoding").is_some_and(|v| v != "identity")
        {
            return None;
        }
        if item.file_version.is_none() {
            let unchanged = if let Some(etag) = strong_etag(&item.etag) {
                text_header(tail.headers(), "etag").as_deref() == Some(etag)
            } else {
                text_header(tail.headers(), "last-modified") == item.last_modified
            };
            if !unchanged {
                return None;
            }
        }
        Some(tail)
    } else {
        None
    };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&(end - start + 1).to_string()).ok()?,
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    for name in [header::CONTENT_TYPE, header::ETAG, header::LAST_MODIFIED] {
        if let Some(value) = verified.get(&name) {
            headers.insert(name, value.clone());
        }
    }
    if !headers.contains_key(header::CONTENT_TYPE) {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(
                item.content_type
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
            )
            .ok()?,
        );
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(
        "x-fn-media-cache",
        HeaderValue::from_static(match (local_length, missing) {
            (0, _) => "MISS",
            (_, true) => "PARTIAL",
            (_, false) => "HIT",
        }),
    );
    if local_length > 0 {
        headers.insert(
            "x-fn-media-cache-kind",
            HeaderValue::from_static("partial-file"),
        );
    }
    if status == StatusCode::PARTIAL_CONTENT {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).ok()?,
        );
    }
    if local_length > 0 {
        response.extensions_mut().insert(LocalPrefix(local_length));
    }
    if method != Method::HEAD {
        file.seek(std::io::SeekFrom::Start(start)).await.ok()?;
        let state = engine.clone();
        let local = ReaderStream::new(file.take(local_length)).map_ok(move |chunk| {
            state
                .counters
                .cache_bytes
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            chunk
        });
        let state = engine.clone();
        let remote = tail.map(|response| {
            let append = PlaybackAppend {
                engine: engine.clone(),
                file_guard: gate.clone(),
                id: item.id,
                total,
                check_capacity: true,
            };
            futures_util::stream::try_unfold(
                (Box::pin(response.bytes_stream()), tail_start, Some(append)),
                move |(mut stream, offset, mut append)| async move {
                    if offset > end {
                        return Ok(None);
                    }
                    let chunk = stream
                        .next()
                        .await
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "upstream suffix ended early",
                            )
                        })?
                        .map_err(std::io::Error::other)?;
                    if chunk.len() as u64 > end + 1 - offset {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "upstream suffix exceeded its range",
                        ));
                    }
                    if let Some(cache) = &mut append
                        && !cache.write(offset, chunk.clone()).await
                    {
                        append = None;
                    }
                    let offset = offset + chunk.len() as u64;
                    Ok(Some((chunk, (stream, offset, append))))
                },
            )
        });
        let remote = futures_util::stream::iter(remote)
            .flatten()
            .map_ok(move |chunk| {
                state
                    .counters
                    .origin_bytes
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                chunk
            });
        let playback = missing.then(|| engine.origin_playback());
        let stream = local.chain(remote).map(move |chunk| {
            let _ = &gate;
            // The upstream response can already be buffering while we serve the local prefix.
            let _ = &playback;
            chunk
        });
        *response.body_mut() = Body::from_stream(stream);
    }
    Some(response)
}
