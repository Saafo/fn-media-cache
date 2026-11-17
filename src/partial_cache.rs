//! Serve a stable downloaded prefix; request only the missing suffix from the origin.
use crate::{
    engine::Engine,
    protocol::{Range, Source, parse_content_range, parse_range, strong_etag, text_header},
    transfers::LocalPrefix,
};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, Response, StatusCode, header},
};
use futures_util::{StreamExt, TryStreamExt};
use std::sync::{Arc, atomic::Ordering};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

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
    let gate = engine.transfers.file_gate(&item.id).read_owned().await;
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
    if start >= available {
        return None;
    }
    let local_end = end.min(available - 1);
    let local_length = local_end - start + 1;
    let missing = method != Method::HEAD && local_end < end;
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
            .header(header::RANGE, format!("bytes={}-{}", local_end + 1, end));
        if item.file_version.is_none()
            && let Some(validator) = strong_etag(&item.etag).or(item.last_modified.as_deref())
        {
            request = request.header(header::IF_RANGE, validator);
        }
        let tail = request.send().await.ok()?;
        if tail.status() != StatusCode::PARTIAL_CONTENT
            || text_header(tail.headers(), "content-range").and_then(|v| parse_content_range(&v))
                != Some((local_end + 1, end, total))
            || tail
                .content_length()
                .is_some_and(|length| length != end - local_end)
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
        HeaderValue::from_static(if missing { "PARTIAL" } else { "HIT" }),
    );
    headers.insert(
        "x-fn-media-cache-kind",
        HeaderValue::from_static("partial-file"),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).ok()?,
        );
    }
    response.extensions_mut().insert(LocalPrefix(local_length));
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
        let remote = futures_util::stream::iter(tail)
            .flat_map(move |response| {
                futures_util::stream::try_unfold(
                    (Box::pin(response.bytes_stream()), end - local_end),
                    |(mut stream, remaining)| async move {
                        if remaining == 0 {
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
                        if chunk.len() as u64 > remaining {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "upstream suffix exceeded its range",
                            ));
                        }
                        let remaining = remaining - chunk.len() as u64;
                        Ok(Some((chunk, (stream, remaining))))
                    },
                )
            })
            .map_ok(move |chunk| {
                state
                    .counters
                    .origin_bytes
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                chunk
            });
        let stream = local.chain(remote).map(move |chunk| {
            let _ = &gate;
            chunk
        });
        *response.body_mut() = Body::from_stream(stream);
    }
    Some(response)
}
