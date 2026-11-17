//! The native single-file FN TV protocol. Metadata is an origin version assertion,
//! not a cryptographic guarantee against an origin changing bytes without reindexing.
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use axum::http::{HeaderMap, HeaderValue, header};
use futures_util::StreamExt;
use md5::{Digest, Md5};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::protocol::{Source, hex, parse_content_range};

// Public client protocol constants, also used by the origin's shipped web client.
const CLIENT_KEY: &str = "NDzZTVxnRKP8Z0jXg1VAMonaG8akvh";
const CLIENT_SIGNING_SUFFIX: &str = "16CCEB3D-AB42-077D-36A1-F355324E4237";

pub struct FileVersion {
    pub fingerprint: String,
    pub size: u64,
}

pub fn media_guid(source: &Source) -> Option<String> {
    let url = source.url().ok()?;
    // Transcoded and direct-link variants need their own representation contracts.
    if url.query().is_some() {
        return None;
    }
    let guid = url.path().strip_prefix("/v/api/v1/media/range/")?;
    if guid.is_empty()
        || guid.len() > 128
        || !guid.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        return None;
    }
    Some(guid.to_owned())
}

fn md5(value: &str) -> String {
    hex(&Md5::digest(value.as_bytes()))
}

fn signed_headers(source: &Source, path: &str, body: &str) -> Result<HeaderMap> {
    let mut headers = source.request_headers()?;
    headers.remove("play-link");
    let nonce = (u32::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..4].try_into()?) % 900_000
        + 100_000)
        .to_string();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .to_string();
    let sign = md5(&[
        CLIENT_KEY,
        path,
        &nonce,
        &timestamp,
        &md5(body),
        CLIENT_SIGNING_SUFFIX,
    ]
    .join("_"));
    headers.insert(
        "authx",
        HeaderValue::from_str(&format!("nonce={nonce}&timestamp={timestamp}&sign={sign}"))?,
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(headers)
}

pub(crate) async fn api(
    client: &reqwest::Client,
    source: &Source,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    let text = body
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?
        .unwrap_or_default();
    let target = Source {
        target: path.into(),
        ..source.clone()
    };
    let url = target.url()?;
    let mut query: std::collections::BTreeMap<String, String> = url
        .query_pairs()
        .into_owned()
        .filter(|(_, value)| value != "undefined" && value != "null")
        .collect();
    let signed_text = if body.is_none() {
        std::mem::take(&mut query)
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("&")
    } else {
        text.clone()
    };
    let request = if body.is_some() {
        client.post(url.clone()).body(text.clone())
    } else {
        client.get(url.clone())
    };
    let response = request
        .headers(signed_headers(source, url.path(), &signed_text)?)
        .send()
        .await
        .map_err(|_| crate::recovery::Retryable("FN metadata request failed"))?;
    if !response.status().is_success() {
        return Err(crate::recovery::http_error(response.status().as_u16()));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = vec![];
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| crate::recovery::Retryable("FN metadata body interrupted"))?;
        if bytes.len() + chunk.len() > 512 * 1024 {
            bail!("FN metadata exceeds limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).context("invalid FN metadata JSON")?;
    if value.get("code").and_then(Value::as_i64) != Some(0) {
        let message = value
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        if [
            "token",
            "unauthorized",
            "login",
            "expired",
            "登录",
            "认证",
            "令牌",
        ]
        .iter()
        .any(|word| message.contains(word))
        {
            return Err(crate::recovery::AuthRequired.into());
        }
        bail!("FN metadata access rejected");
    }
    value
        .get("data")
        .cloned()
        .context("FN metadata is missing data")
}

pub async fn account_identity(client: &reqwest::Client, source: &Source) -> Result<String> {
    let user = api(client, source, "/v/api/v1/user/info", None).await?;
    let identity = user
        .get("guid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            user.get("username")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .context("account identity unavailable")?;
    Ok(hex(&Sha256::digest(serde_json::to_vec(&(
        source.origin.clone(),
        identity,
    ))?)))
}

pub async fn file_version(client: &reqwest::Client, source: &Source) -> Result<FileVersion> {
    let guid = media_guid(source).context("not a native FN single-file request")?;
    let user = api(client, source, "/v/api/v1/user/info", None).await?;
    let account = user
        .get("username")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .context("FN account metadata is missing")?;
    let user_agent = source
        .headers
        .iter()
        .find(|(key, _)| key == "user-agent")
        .map(|(_, value)| value.as_str())
        .unwrap_or("fn-media-cache");
    let data = api(client, source, "/v/api/v1/stream", Some(json!({
        "media_guid": guid, "ip": md5(account), "header": {"User-Agent": [user_agent]}, "level": 1,
    }))).await?;
    let file = data
        .get("file_stream")
        .context("FN response has no file metadata")?;
    if file.get("guid").and_then(Value::as_str) != Some(&guid)
        || file.get("can_play").and_then(Value::as_i64) != Some(1)
        || file.get("type").and_then(Value::as_i64) != Some(1)
    {
        bail!("FN file identity or permission mismatch");
    }
    let size = file
        .get("size")
        .and_then(Value::as_u64)
        .filter(|s| *s > 0)
        .context("FN file has no size")?;
    let timestamp = file
        .get("timestamp")
        .and_then(Value::as_i64)
        .filter(|t| *t > 0)
        .context("FN file has no timestamp")?;
    let update = file
        .get("update_time")
        .and_then(Value::as_i64)
        .filter(|t| *t > 0)
        .context("FN file has no update time")?;
    let file_path = file
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .context("FN file has no path")?;
    let identity = json!([
        guid,
        size,
        timestamp,
        update,
        file.get("create_time"),
        file.get("file_birth_time"),
        file_path
    ]);
    Ok(FileVersion {
        fingerprint: format!(
            "fntv-v1:{}",
            hex(&Sha256::digest(serde_json::to_vec(&identity)?))
        ),
        size,
    })
}

pub async fn authorize_range(
    client: &reqwest::Client,
    source: &Source,
    size: u64,
) -> Result<HeaderMap> {
    let response = client
        .get(source.url()?)
        .headers(source.request_headers()?)
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("FN byte authorization request failed"))?;
    let span = response
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|h| h.to_str().ok())
        .and_then(parse_content_range);
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT || span != Some((0, 0, size)) {
        bail!("FN byte authorization rejected or file size changed");
    }
    let headers = response.headers().clone();
    let mut stream = response.bytes_stream();
    let mut read = 0;
    while let Some(chunk) = stream.next().await {
        read += chunk
            .map_err(|_| anyhow::anyhow!("FN byte authorization interrupted"))?
            .len();
        if read > 1 {
            bail!("FN byte authorization exceeded its one-byte limit");
        }
    }
    if read != 1 {
        bail!("FN byte authorization returned no data");
    }
    Ok(headers)
}
