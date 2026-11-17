use anyhow::{Result, bail};
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Source {
    pub origin: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
}

impl Source {
    pub fn new(origin: &str, target: &str, headers: &HeaderMap) -> Self {
        let mut selected: Vec<_> = clean_headers(headers)
            .iter()
            .filter_map(|(name, value)| {
                let name = name.as_str();
                if matches!(
                    name,
                    "range"
                        | "if-range"
                        | "if-none-match"
                        | "if-modified-since"
                        | "if-match"
                        | "if-unmodified-since"
                        | "content-length"
                        | "accept-encoding"
                ) {
                    return None;
                }
                value.to_str().ok().map(|v| (name.to_owned(), v.to_owned()))
            })
            .collect();
        selected.sort();
        Self {
            origin: origin.into(),
            target: target.into(),
            headers: selected,
        }
    }

    pub fn id(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("source serializes");
        hex(&Sha256::digest(bytes))
    }

    pub fn cache_id(&self) -> String {
        if crate::fntv::media_guid(self).is_none() {
            return self.id();
        }
        // Playback tokens vary between sessions. Current credentials still gate every hit.
        let scope: Vec<_> = self
            .headers
            .iter()
            .filter(|(key, _)| matches!(key.as_str(), "authorization" | "cookie"))
            .collect();
        hex(&Sha256::digest(
            serde_json::to_vec(&(&self.origin, &self.target, scope))
                .expect("cache scope serializes"),
        ))
    }

    pub fn display_path(&self) -> String {
        self.target.split('?').next().unwrap_or("/").to_owned()
    }

    pub fn url(&self) -> Result<Url> {
        if !self.target.starts_with('/') || self.target.contains('#') {
            bail!("invalid upstream path");
        }
        let url = Url::parse(&format!(
            "{}{}",
            self.origin.trim_end_matches('/'),
            self.target
        ))?;
        if url.origin() != Url::parse(&self.origin)?.origin() {
            bail!("cross-origin request rejected");
        }
        Ok(url)
    }

    pub fn request_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            headers.append(
                HeaderName::try_from(name.as_str())?,
                HeaderValue::try_from(value.as_str())?,
            );
        }
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        Ok(headers)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn clean_headers(input: &HeaderMap) -> HeaderMap {
    let connection_tokens: Vec<_> = input
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    let mut output = HeaderMap::new();
    for (name, value) in input {
        if matches!(
            name.as_str(),
            "host"
                | "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
                | "x-fn-media-cache-only"
        ) || connection_tokens.iter().any(|token| token == name.as_str())
        {
            continue;
        }
        output.append(name.clone(), value.clone());
    }
    output
}

pub fn text_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

pub fn strong_etag(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .filter(|v| v.starts_with('"') && v.ends_with('"'))
}

pub fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total) = (start.parse().ok()?, end.parse().ok()?, total.parse().ok()?);
    (start <= end && end < total).then_some((start, end, total))
}

#[derive(Debug, PartialEq)]
pub enum Range {
    Full,
    Partial(u64, u64),
    Unsatisfiable,
    Unsupported,
}

pub fn parse_range(value: Option<&str>, size: u64) -> Range {
    let Some(value) = value else {
        return Range::Full;
    };
    if value.contains(',') || !value.starts_with("bytes=") {
        return Range::Unsupported;
    }
    let Some((start, end)) = value[6..].split_once('-') else {
        return Range::Unsupported;
    };
    if start.is_empty() {
        let Ok(suffix) = end.parse::<u64>() else {
            return Range::Unsupported;
        };
        if suffix == 0 || size == 0 {
            return Range::Unsatisfiable;
        }
        return Range::Partial(size.saturating_sub(suffix), size - 1);
    }
    let Ok(start) = start.parse::<u64>() else {
        return Range::Unsupported;
    };
    let end = if end.is_empty() {
        size.saturating_sub(1)
    } else {
        let Ok(end) = end.parse::<u64>() else {
            return Range::Unsupported;
        };
        end.min(size.saturating_sub(1))
    };
    if size == 0 || start >= size || end < start {
        Range::Unsatisfiable
    } else {
        Range::Partial(start, end)
    }
}

pub fn media_candidate(headers: &HeaderMap, path: &str) -> bool {
    let kind = text_header(headers, "content-type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if kind.contains("mpegurl") || kind.contains("dash+xml") || kind.contains("mp2t") {
        return false;
    }
    let path = path.split('?').next().unwrap_or("").to_ascii_lowercase();
    kind.starts_with("video/")
        || kind.starts_with("application/octet-stream")
        || [".mp4", ".mkv", ".mov", ".avi", ".m4v"]
            .iter()
            .any(|ext| path.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_cache_scope_survives_playback_session_changes() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "session-a".parse().unwrap());
        headers.insert("play-link", "playback-one".parse().unwrap());
        let first = Source::new(
            "https://example.com",
            "/v/api/v1/media/range/media-one",
            &headers,
        );
        headers.insert("play-link", "playback-two".parse().unwrap());
        headers.insert("user-agent", "another-player".parse().unwrap());
        let second = Source::new(
            "https://example.com",
            "/v/api/v1/media/range/media-one",
            &headers,
        );
        assert_ne!(first.id(), second.id());
        assert_eq!(first.cache_id(), second.cache_id());
        headers.insert("authorization", "session-b".parse().unwrap());
        assert_ne!(
            first.cache_id(),
            Source::new(
                "https://example.com",
                "/v/api/v1/media/range/media-one",
                &headers
            )
            .cache_id()
        );
    }
    #[test]
    fn ranges_cover_seek_and_empty_files() {
        assert_eq!(parse_range(None, 10), Range::Full);
        assert_eq!(parse_range(Some("bytes=2-5"), 10), Range::Partial(2, 5));
        assert_eq!(parse_range(Some("bytes=7-"), 10), Range::Partial(7, 9));
        assert_eq!(parse_range(Some("bytes=-3"), 10), Range::Partial(7, 9));
        assert_eq!(parse_range(Some("bytes=-99"), 10), Range::Partial(0, 9));
        assert_eq!(parse_range(Some("bytes=10-"), 10), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-0"), 10), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=0-"), 0), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=0-1,4-5"), 10), Range::Unsupported);
    }
    #[test]
    fn identities_isolate_accounts_but_not_seek_offsets() {
        let mut a = HeaderMap::new();
        a.insert("authorization", "Bearer A".parse().unwrap());
        let first = Source::new("https://example.com", "/movie?token=x", &a);
        a.insert("range", "bytes=10-".parse().unwrap());
        assert_eq!(
            first.id(),
            Source::new("https://example.com", "/movie?token=x", &a).id()
        );
        a.insert("authorization", "Bearer B".parse().unwrap());
        assert_ne!(
            first.id(),
            Source::new("https://example.com", "/movie?token=x", &a).id()
        );
        assert_ne!(
            first.id(),
            Source::new("https://example.com", "/movie?token=y", &a).id()
        );
    }
    #[test]
    fn hop_headers_are_removed_and_duplicate_cookies_survive() {
        let mut input = HeaderMap::new();
        input.insert("connection", "keep-alive, x-private".parse().unwrap());
        input.insert("x-private", "value".parse().unwrap());
        input.append("set-cookie", "a=1".parse().unwrap());
        input.append("set-cookie", "b=2".parse().unwrap());
        let clean = clean_headers(&input);
        assert!(!clean.contains_key("x-private"));
        assert_eq!(clean.get_all("set-cookie").iter().count(), 2);
    }
}
