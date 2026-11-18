//! Account-scoped catalog access and creation of native downloads without playback.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{engine::Engine, fntv, protocol::Source};

pub fn valid_guid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

#[derive(Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub origin: String,
    pub status: String,
    #[serde(skip)]
    pub account_key: Option<String>,
    #[serde(skip)]
    pub checked_at: i64,
    #[serde(skip)]
    pub source: Source,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaItem {
    pub guid: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tv_title: String,
    pub kind: String,
    pub parent_guid: String,
    pub season_number: u64,
    pub episode_number: u64,
    pub watched: Option<bool>,
}

impl MediaItem {
    pub fn parse(value: &Value) -> Result<Self> {
        let guid = value
            .get("guid")
            .and_then(Value::as_str)
            .filter(|s| valid_guid(s))
            .context("invalid catalog identity")?;
        let text = |key| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let watched = value
            .get("is_watched")
            .or_else(|| value.get("watched"))
            .and_then(|v| {
                v.as_bool()
                    .or_else(|| v.as_i64().filter(|n| matches!(n, 0 | 1)).map(|n| n == 1))
            });
        let tv_title = text("tv_title");
        let title = if text("title").is_empty() {
            tv_title.clone()
        } else {
            text("title")
        };
        Ok(Self {
            guid: guid.into(),
            title,
            tv_title,
            kind: text("type"),
            parent_guid: text("parent_guid"),
            season_number: value
                .get("season_number")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            episode_number: value
                .get("episode_number")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            watched,
        })
    }
    pub fn playable(&self) -> bool {
        matches!(self.kind.as_str(), "Episode" | "Movie" | "Video")
    }
}

#[derive(Serialize)]
pub struct CatalogPage {
    pub items: Vec<MediaItem>,
    pub total: u64,
    pub has_more: bool,
}

fn items(value: &Value) -> Result<Vec<MediaItem>> {
    let rows = value.as_array().context("catalog response is not a list")?;
    if rows.len() > 2000 {
        bail!("catalog list exceeds limit");
    }
    rows.iter().map(MediaItem::parse).collect()
}

impl Engine {
    pub fn session(&self, id: Option<&str>) -> Result<Session> {
        self.store
            .sessions()?
            .into_iter()
            .find(|s| id.is_none_or(|id| s.id == id))
            .context("log in through the proxy first")
    }

    pub async fn catalog_item(&self, session: &Session, guid: &str) -> Result<MediaItem> {
        if !valid_guid(guid) {
            bail!("invalid item identity");
        }
        MediaItem::parse(
            &fntv::api(
                &self.client,
                &session.source,
                &format!("/v/api/v1/item/{guid}"),
                None,
            )
            .await?,
        )
    }

    pub async fn catalog_search(
        &self,
        session: &Session,
        query: &str,
        page: u64,
    ) -> Result<CatalogPage> {
        if query.len() > 200 || page == 0 || page > 1000 {
            bail!("invalid catalog query");
        }
        if query.trim().is_empty() {
            let value = fntv::api(
                &self.client,
                &session.source,
                "/v/api/v1/mediadb/list",
                None,
            )
            .await?;
            let rows = items(&value)?;
            return Ok(CatalogPage {
                total: rows.len() as u64,
                items: rows,
                has_more: false,
            });
        }
        let mut url = reqwest::Url::parse("http://catalog.invalid/v/api/v1/search/list")?;
        url.query_pairs_mut()
            .append_pair("q", query.trim())
            .append_pair("page", &page.to_string())
            .append_pair("page_size", "30");
        let path = format!("{}?{}", url.path(), url.query().unwrap());
        let value = fntv::api(&self.client, &session.source, &path, None).await?;
        let all_rows = items(&value)?;
        let has_more = all_rows.len() >= 30;
        let rows = all_rows
            .into_iter()
            .filter(|item| {
                matches!(
                    item.kind.as_str(),
                    "TV" | "Season" | "Episode" | "Movie" | "Video" | "Directory"
                )
            })
            .collect::<Vec<_>>();
        Ok(CatalogPage {
            total: rows.len() as u64,
            items: rows,
            has_more,
        })
    }

    pub async fn catalog_children(
        &self,
        session: &Session,
        guid: &str,
        page: u64,
    ) -> Result<CatalogPage> {
        let parent = self.catalog_item(session, guid).await?;
        let value = match parent.kind.as_str() {
            "TV" => {
                fntv::api(
                    &self.client,
                    &session.source,
                    &format!("/v/api/v1/season/list/{guid}"),
                    None,
                )
                .await?
            }
            "Season" => {
                fntv::api(
                    &self.client,
                    &session.source,
                    &format!("/v/api/v1/episode/list/{guid}"),
                    None,
                )
                .await?
            }
            "MediaDB" | "Directory" => {
                if page == 0 || page > 1000 {
                    bail!("invalid catalog page");
                }
                let mut body =
                    json!({"page":page,"page_size":30,"sort_column":"title","sort_type":"ASC"});
                body[if parent.kind == "MediaDB" {
                    "ancestor_guid"
                } else {
                    "parent_guid"
                }] = json!(guid);
                let data = fntv::api(
                    &self.client,
                    &session.source,
                    "/v/api/v1/item/list",
                    Some(body),
                )
                .await?;
                let rows = items(data.get("list").context("catalog list missing")?)?;
                return Ok(CatalogPage {
                    total: data
                        .get("total")
                        .and_then(Value::as_u64)
                        .unwrap_or(rows.len() as u64),
                    items: rows,
                    has_more: data
                        .get("total")
                        .and_then(Value::as_u64)
                        .is_some_and(|n| n > page * 30),
                });
            }
            _ => {
                return Ok(CatalogPage {
                    total: 1,
                    items: vec![parent],
                    has_more: false,
                });
            }
        };
        let mut rows = items(&value)?;
        rows.sort_by_key(|item| (item.season_number, item.episode_number, item.guid.clone()));
        Ok(CatalogPage {
            total: rows.len() as u64,
            items: rows,
            has_more: false,
        })
    }

    pub async fn queue_catalog_item(
        &self,
        session: &Session,
        guid: &str,
        prefetch_from: Option<&str>,
        paused: bool,
    ) -> Result<String> {
        let explicit = prefetch_from.is_none();
        self.verify_session(session).await?;
        if !explicit && self.store.suppressed(&session.id, guid)? {
            bail!("automatic download suppressed");
        }
        let mut item = self.catalog_item(session, guid).await?;
        if !item.playable() {
            bail!("select an individual episode or movie");
        }
        let info = fntv::api(
            &self.client,
            &session.source,
            "/v/api/v1/play/info",
            Some(json!({"item_guid":guid})),
        )
        .await?;
        if info.get("guid").and_then(Value::as_str) != Some(guid) {
            bail!("play information resolved a different item");
        }
        let media = info
            .get("media_guid")
            .and_then(Value::as_str)
            .filter(|s| valid_guid(s))
            .context("no native media file")?;
        let source = Source {
            target: format!("/v/api/v1/media/range/{media}"),
            ..session.source.clone()
        };
        fntv::file_version(&self.client, &source).await?;
        let policy = self.policy.read().unwrap();
        if let Some(current) = prefetch_from
            && !self.prefetch_watch.lock().unwrap().eligible(
                &session.id,
                current,
                &policy,
                std::time::Instant::now(),
            )
        {
            bail!("automatic prefetch is no longer eligible");
        }
        if policy.local_history {
            item.watched = None;
        }
        let id = self
            .store
            .enqueue_catalog_media(session, &item, media, &source, explicit, paused)?;
        if explicit && !paused {
            self.store.command(&id, true)?;
        }
        drop(policy);
        crate::event!(
            "download {} enqueued by={} paused={}",
            &id[..12],
            if explicit { "manual" } else { "prefetch" },
            paused
        );
        self.wake.notify_one();
        Ok(id)
    }
}
