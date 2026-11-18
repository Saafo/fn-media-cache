use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::Instant,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use futures_util::TryStreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::{
    engine::Engine,
    local_history,
    protocol::{Range, Source, clean_headers, parse_range, strong_etag, text_header},
    store::Download,
};

pub fn proxy_router(engine: Arc<Engine>) -> Router {
    Router::new().fallback(proxy).with_state(engine)
}

pub fn admin_router(engine: Arc<Engine>) -> Router {
    let api = Router::new()
        .route("/stats", get(stats))
        .route("/observations", get(observations))
        .route("/progress-events", get(progress_events))
        .route("/routes", get(routes))
        .route("/routes/probe", post(probe_routes))
        .route("/sessions", get(sessions))
        .route("/sessions/refresh", post(refresh_sessions))
        .route("/diagnostics", get(diagnostics))
        .route("/library/search", post(library_search))
        .route("/library/browse", post(library_browse))
        .route("/library/series", post(library_series))
        .route("/library/download", post(library_download))
        .route("/automation", get(automation))
        .route("/automation/policy", post(save_policy))
        .route("/managed/retention", post(retention))
        .route("/observations/{id}/probe", post(probe))
        .route("/downloads", get(downloads).post(enqueue))
        .route("/downloads/{id}/cache-samples", post(cache_samples))
        .route("/downloads/{id}/{action}", post(control))
        .route_layer(middleware::from_fn_with_state(engine.clone(), authorize));
    Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(app_script))
        .route("/assets/app.css", get(app_stylesheet))
        .route("/licenses.txt", get(app_licenses))
        .route(
            "/health",
            get(|| async { Json(json!({"ok": true, "version": env!("CARGO_PKG_VERSION")})) }),
        )
        .nest("/api", api)
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(engine)
}

async fn authorize(
    State(engine): State<Arc<Engine>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let expected = format!("Bearer {}", engine.token);
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .map(|h| h.as_bytes())
        .unwrap_or_default();
    let matches = supplied.len() == expected.len()
        && supplied
            .iter()
            .zip(expected.bytes())
            .fold(0u8, |n, (a, b)| n | (a ^ b))
            == 0;
    if !matches {
        return (StatusCode::UNAUTHORIZED, "management token required").into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn index() -> Response {
    let mut response = Html(include_str!("../desktop/dist/index.html")).into_response();
    response.headers_mut().insert("content-security-policy", HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn app_script() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../desktop/dist/assets/app.js"),
    )
        .into_response()
}

async fn app_stylesheet() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../desktop/dist/assets/app.css"),
    )
        .into_response()
}

async fn app_licenses() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        include_str!("../desktop/dist/licenses.txt"),
    )
        .into_response()
}

async fn diagnostics(State(engine): State<Arc<Engine>>) -> ApiResult<serde_json::Value> {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for row in engine.store.list().map_err(catalog_error)? {
        *counts.entry(row.state).or_default() += 1;
    }
    let logs = engine.config.data_dir.join("logs/service.log");
    Ok(Json(
        json!({"version":env!("CARGO_PKG_VERSION"),"downloads":counts,"playing":engine.playing(),
        "log_bytes":std::fs::metadata(logs).map(|m|m.len()).unwrap_or(0),"log_max_bytes":engine.config.log_max_bytes,"log_keep_files":engine.config.log_keep_files}),
    ))
}

async fn refresh_sessions(State(engine): State<Arc<Engine>>) -> ApiResult<serde_json::Value> {
    engine
        .store
        .db
        .lock()
        .unwrap()
        .execute("UPDATE sessions SET checked_at=0", [])
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "cannot refresh sessions"))?;
    engine.refresh_sessions().await.map_err(catalog_error)?;
    Ok(Json(json!({"ok":true})))
}

async fn stats(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    let c = &engine.counters;
    Json(
        json!({ "requests": c.requests.load(Ordering::Relaxed), "cache_hits": c.cache_hits.load(Ordering::Relaxed),
        "origin_bytes": c.origin_bytes.load(Ordering::Relaxed), "cache_bytes": c.cache_bytes.load(Ordering::Relaxed),
        "external_redirects": c.external_redirects.load(Ordering::Relaxed), "max_cache_bytes": engine.cache_limit(), "playback": engine.transfers.playback() }),
    )
}

async fn observations(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(json!(*engine.observations.lock().unwrap()))
}

async fn progress_events(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(json!(*engine.progress_events.lock().unwrap()))
}

async fn routes(State(engine): State<Arc<Engine>>) -> Json<crate::routes::RouteStatus> {
    Json(engine.routes.status())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteProbe {
    download_id: Option<String>,
}

async fn probe_routes(
    State(engine): State<Arc<Engine>>,
    Json(input): Json<RouteProbe>,
) -> ApiResult<crate::routes::RouteStatus> {
    let source = if let Some(id) = input.download_id {
        engine
            .store
            .get(&id)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "cannot read source"))?
            .map(|item| item.source)
    } else {
        let observed = engine
            .observations
            .lock()
            .unwrap()
            .iter()
            .find(|item| item.cacheable && crate::fntv::media_guid(&item.source).is_some())
            .map(|item| item.source.clone());
        match observed {
            Some(source) => Some(source),
            None => engine
                .store
                .list()
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "cannot read sources"))?
                .into_iter()
                .find(|item| crate::fntv::media_guid(&item.source).is_some())
                .map(|item| item.source),
        }
    }
    .ok_or((
        StatusCode::CONFLICT,
        "play a native FN video through the proxy first",
    ))?;
    engine.refresh_routes(&source).await.map(Json).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "route measurement requires a native FN single-file source",
        )
    })
}

type ApiResult<T> = Result<Json<T>, (StatusCode, &'static str)>;

fn catalog_error(_: anyhow::Error) -> (StatusCode, &'static str) {
    (
        StatusCode::BAD_GATEWAY,
        "catalog request failed; check session and native media availability",
    )
}

async fn sessions(State(engine): State<Arc<Engine>>) -> ApiResult<Vec<crate::library::Session>> {
    let mut seen = std::collections::HashSet::new();
    engine
        .store
        .sessions()
        .map(|sessions| {
            Json(
                sessions
                    .into_iter()
                    .filter(|session| {
                        seen.insert(
                            session
                                .account_key
                                .clone()
                                .unwrap_or_else(|| session.id.clone()),
                        )
                    })
                    .collect(),
            )
        })
        .map_err(catalog_error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogQuery {
    session_id: Option<String>,
    query: Option<String>,
    item_guid: Option<String>,
    page: Option<u64>,
}

async fn library_search(
    State(engine): State<Arc<Engine>>,
    Json(input): Json<CatalogQuery>,
) -> ApiResult<crate::library::CatalogPage> {
    let session = engine
        .session(input.session_id.as_deref())
        .map_err(catalog_error)?;
    engine
        .catalog_search(
            &session,
            input.query.as_deref().unwrap_or(""),
            input.page.unwrap_or(1),
        )
        .await
        .map(Json)
        .map_err(catalog_error)
}

async fn library_browse(
    State(engine): State<Arc<Engine>>,
    Json(input): Json<CatalogQuery>,
) -> ApiResult<crate::library::CatalogPage> {
    let session = engine
        .session(input.session_id.as_deref())
        .map_err(catalog_error)?;
    let guid = input
        .item_guid
        .ok_or((StatusCode::BAD_REQUEST, "item_guid required"))?;
    engine
        .catalog_children(&session, &guid, input.page.unwrap_or(1))
        .await
        .map(Json)
        .map_err(catalog_error)
}

async fn library_series(
    State(engine): State<Arc<Engine>>,
    Json(input): Json<CatalogQuery>,
) -> ApiResult<crate::library::MediaItem> {
    let session = engine
        .session(input.session_id.as_deref())
        .map_err(catalog_error)?;
    let guid = input
        .item_guid
        .ok_or((StatusCode::BAD_REQUEST, "season item_guid required"))?;
    let season = engine
        .catalog_item(&session, &guid)
        .await
        .map_err(catalog_error)?;
    if season.kind != "Season" || season.guid != guid {
        return Err((StatusCode::BAD_REQUEST, "expected a season"));
    }
    let series = engine
        .catalog_item(&session, &season.parent_guid)
        .await
        .map_err(catalog_error)?;
    if series.kind != "TV" || series.guid != season.parent_guid {
        return Err((StatusCode::BAD_REQUEST, "expected the parent series"));
    }
    Ok(Json(series))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    session_id: Option<String>,
    item_guids: Vec<String>,
    #[serde(default)]
    paused: bool,
}

async fn library_download(
    State(engine): State<Arc<Engine>>,
    Json(input): Json<Selection>,
) -> ApiResult<serde_json::Value> {
    if input.item_guids.is_empty() || input.item_guids.len() > 20 {
        return Err((
            StatusCode::BAD_REQUEST,
            "select between 1 and 20 individual items",
        ));
    }
    let session = engine
        .session(input.session_id.as_deref())
        .map_err(catalog_error)?;
    let mut results = vec![];
    for guid in input.item_guids {
        match engine.queue_catalog_item(&session,&guid,true,input.paused).await {
            Ok(id)=>results.push(json!({"item_guid":guid,"id":id,"ok":true})),
            Err(_)=>results.push(json!({"item_guid":guid,"ok":false,"error":"无法取得可缓存的原始文件，请检查会话与权限"})),
        }
    }
    Ok(Json(json!({"results":results})))
}

async fn automation(State(engine): State<Arc<Engine>>) -> ApiResult<serde_json::Value> {
    let policy = engine.policy.read().unwrap().clone();
    let managed = engine.store.managed().map_err(catalog_error)?;
    Ok(Json(json!({
        "policy": policy,
        "managed": managed,
        "playing": engine.playing(),
        "downloads_yielding": engine.downloads_yielding(),
        "cache_limit_bytes": engine.cache_limit()
    })))
}

async fn save_policy(
    State(engine): State<Arc<Engine>>,
    Json(policy): Json<crate::scheduler::Policy>,
) -> ApiResult<serde_json::Value> {
    policy.validate().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "configuration value is out of range",
        )
    })?;
    {
        let mut active = engine.policy.write().unwrap();
        let clear = active.local_history != policy.local_history || policy.incognito;
        engine
            .store
            .save_policy(&policy, clear)
            .map_err(catalog_error)?;
        if clear {
            engine.progress_events.lock().unwrap().clear();
            engine.history_metadata.clear();
        }
        if policy.incognito {
            engine.observations.lock().unwrap().clear();
            engine.transfers.clear_playback();
        } else if policy.local_history {
            engine
                .observations
                .lock()
                .unwrap()
                .retain(|item| crate::privacy::endpoint(&item.path) != Some("play/list"));
        }
        *active = policy;
    }
    engine.scheduler_wake.notify_one();
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Retention {
    session_id: String,
    item_guid: String,
    keep: Option<bool>,
    completed: Option<bool>,
}

async fn retention(
    State(engine): State<Arc<Engine>>,
    Json(input): Json<Retention>,
) -> ApiResult<serde_json::Value> {
    if input.keep.is_none() && input.completed.is_none() {
        return Err((StatusCode::BAD_REQUEST, "retention action required"));
    }
    engine
        .store
        .set_retention(
            &input.session_id,
            &input.item_guid,
            input.keep,
            input.completed,
            crate::scheduler::now(),
        )
        .map_err(|_| {
            (
                StatusCode::CONFLICT,
                "item missing or cleanup already in progress",
            )
        })?;
    engine.scheduler_wake.notify_one();
    Ok(Json(json!({"ok":true})))
}

async fn downloads(
    State(engine): State<Arc<Engine>>,
) -> ApiResult<Vec<crate::transfers::DownloadView>> {
    engine
        .store
        .list()
        .map(|items| {
            let yielding = engine.downloads_yielding();
            Json(
                items
                    .into_iter()
                    .map(|item| engine.transfers.download_view(item, yielding))
                    .collect(),
            )
        })
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "cannot read queue"))
}

async fn probe(
    State(engine): State<Arc<Engine>>,
    Path(id): Path<String>,
) -> ApiResult<crate::probe::ProbeResult> {
    let (source, progress) = {
        let observations = engine.observations.lock().unwrap();
        let source = observations
            .iter()
            .find(|item| item.id == id && item.cacheable)
            .map(|item| item.source.clone())
            .ok_or((
                StatusCode::NOT_FOUND,
                "media observation no longer available",
            ))?;
        let progress = observations
            .iter()
            .find(|item| item.source.display_path() == "/v/api/v1/play/list")
            .map(|item| item.source.clone());
        (source, progress)
    };
    engine
        .probe(&source, progress.as_ref())
        .await
        .map(Json)
        .map_err(|error| {
            crate::event!("bounded probe {} failed: {}", &id[..12], error);
            (
                StatusCode::BAD_GATEWAY,
                "bounded range probe failed; no complete download was started",
            )
        })
}

async fn cache_samples(
    State(engine): State<Arc<Engine>>,
    Path(id): Path<String>,
) -> ApiResult<crate::segments::SampleCacheResult> {
    let _guard = engine.transfers.file_gate(&id).read_owned().await;
    let item = engine
        .store
        .get(&id)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "cannot read source"))?
        .ok_or((StatusCode::NOT_FOUND, "download source not found"))?;
    if matches!(item.state.as_str(), "deleting" | "removed") {
        return Err((
            StatusCode::CONFLICT,
            "cache is being deleted; enqueue it again first",
        ));
    }
    engine
        .cache_samples(&item.source)
        .await
        .map(Json)
        .map_err(|error| {
            crate::event!("sample cache {} failed: {}", &id[..12], error);
            (StatusCode::BAD_GATEWAY, "bounded sample caching failed")
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enqueue {
    observation_id: String,
    #[serde(default)]
    paused: bool,
}

async fn enqueue(
    State(engine): State<Arc<Engine>>,
    Json(input): Json<Enqueue>,
) -> ApiResult<serde_json::Value> {
    let source = engine
        .observations
        .lock()
        .unwrap()
        .iter()
        .find(|o| o.id == input.observation_id && o.cacheable)
        .map(|o| o.source.clone())
        .ok_or((
            StatusCode::BAD_REQUEST,
            "observe a directly served media request first",
        ))?;
    let id = engine
        .store
        .enqueue(&source, input.paused)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "cannot save download"))?;
    crate::event!("download {} saved paused={}", &id[..12], input.paused);
    engine.wake.notify_one();
    Ok(Json(json!({"id": id})))
}

async fn control(
    State(engine): State<Arc<Engine>>,
    Path((id, action)): Path<(String, String)>,
) -> ApiResult<serde_json::Value> {
    if ["cancel", "delete", "up", "down", "first"].contains(&action.as_str()) {
        let changed = match action.as_str() {
            "cancel" => engine.store.cancel(&id),
            "delete" => engine.store.request_delete(&id),
            _ => engine.store.reorder(&id, &action),
        }
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "cannot update task"))?;
        if !changed {
            return Err((
                StatusCode::CONFLICT,
                "task is missing or cannot enter that state",
            ));
        }
        engine.wake.notify_one();
        engine.scheduler_wake.notify_one();
        crate::event!("download {} command={}", &id[..id.len().min(12)], action);
        return Ok(Json(json!({"ok":true})));
    }
    let resume = match action.as_str() {
        "resume" => true,
        "pause" => false,
        _ => return Err((StatusCode::NOT_FOUND, "unknown action")),
    };
    let changed = engine
        .store
        .command(&id, resume)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "cannot update download"))?;
    if !changed {
        return Err((
            StatusCode::CONFLICT,
            "download does not exist or cannot enter that state",
        ));
    }
    crate::event!("download {} command={}", &id[..id.len().min(12)], action);
    engine.wake.notify_one();
    Ok(Json(json!({"ok": true})))
}

async fn proxy(State(engine): State<Arc<Engine>>, request: Request<Body>) -> Response {
    let target = request
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or("/");
    let source = Source::new(&engine.config.upstream, target, request.headers());
    let is_get = request.method() == Method::GET;
    let lease = is_get.then(|| {
        let mut file_source = source.clone();
        file_source.target = file_source
            .target
            .split('?')
            .next()
            .unwrap_or("/")
            .to_owned();
        let playback = target.starts_with("/v/api/v1/media/")
            || crate::protocol::media_candidate(&HeaderMap::new(), target);
        engine.cache_lease(file_source.cache_id(), playback)
    });
    let response = proxy_inner(engine.clone(), request).await;
    if target_is_api(&source)
        && response.status().is_success()
        && !response.headers().contains_key("x-fn-media-cache-privacy")
    {
        let _ = engine.store.save_session(&source);
        engine.scheduler_wake.notify_one();
    }
    if let Some(lease) = lease {
        let media = response.status().is_success()
            && crate::protocol::media_candidate(response.headers(), &source.target);
        let policy = engine.policy.read().unwrap();
        let reading = (media && !policy.incognito).then(|| {
            let download = engine
                .store
                .get_for_source(&source)
                .ok()
                .flatten()
                .map(|d| d.id);
            let item = engine.history_metadata.display_item(&source);
            engine
                .transfers
                .start(source.cache_id(), download, source.display_path(), item)
        });
        drop(policy);
        let mut local_left = response
            .extensions()
            .get::<crate::transfers::LocalPrefix>()
            .map(|p| p.0)
            .unwrap_or_else(|| {
                if response
                    .headers()
                    .get("x-fn-media-cache")
                    .is_some_and(|v| v == "HIT")
                {
                    u64::MAX
                } else {
                    0
                }
            });
        let (parts, body) = response.into_parts();
        let stream = body.into_data_stream().map_ok(move |chunk| {
            let _ = &lease;
            if let Some(reading) = &reading {
                let local = local_left.min(chunk.len() as u64);
                local_left = local_left.saturating_sub(local);
                reading.bytes(local, chunk.len() as u64 - local);
            }
            chunk
        });
        Response::from_parts(parts, Body::from_stream(stream))
    } else {
        response
    }
}

fn target_is_api(source: &Source) -> bool {
    source.target.starts_with("/v/api/v1/")
}

async fn proxy_inner(engine: Arc<Engine>, request: Request<Body>) -> Response {
    let started = Instant::now();
    engine.counters.requests.fetch_add(1, Ordering::Relaxed);
    let (parts, body) = request.into_parts();
    let policy = engine.policy.read().unwrap().clone();
    let private = policy.incognito;
    if private && crate::privacy::viewing_report(&parts.method, parts.uri.path()) {
        let mut response = Json(json!({"code":0,"msg":"","data":true})).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response.headers_mut().insert(
            "x-fn-media-cache-privacy",
            HeaderValue::from_static("suppressed"),
        );
        return response;
    }
    let target = parts
        .uri
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or("/");
    let source = Source::new(&engine.config.upstream, target, &parts.headers);
    if policy.local_history
        && (local_history::list_request(&parts.method, parts.uri.path())
            || crate::privacy::viewing_report(&parts.method, parts.uri.path()))
    {
        return local_history::handle(&engine, &source, &parts.method, parts.uri.path(), body)
            .await;
    }
    let play_info = !private && local_history::info_request(&parts.method, parts.uri.path());
    let local_info = policy.local_history && play_info;
    let id = source.id();
    if matches!(parts.method, Method::GET | Method::HEAD)
        && let Some(response) =
            cached_segment(&engine, &source, &parts.method, &parts.headers).await
    {
        engine.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
        if !private && !engine.policy.read().unwrap().incognito {
            crate::event!(
                "request {} {} segment-cache={}",
                &id[..12],
                parts.method,
                response.status().as_u16()
            );
        }
        return response;
    }
    if matches!(parts.method, Method::GET | Method::HEAD)
        && let Ok(Some(item)) = engine.store.get_for_source(&source)
        && item.state == "complete"
        && let Some(response) = cached(&engine, &source, &item, &parts.method, &parts.headers).await
    {
        engine.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
        if !private && !engine.policy.read().unwrap().incognito {
            crate::event!(
                "request {} {} cache={} elapsed_ms={}",
                &id[..12],
                parts.method,
                response.status().as_u16(),
                started.elapsed().as_millis()
            );
        }
        return response;
    }
    if matches!(parts.method, Method::GET | Method::HEAD)
        && let Some(response) =
            crate::partial_cache::cached_prefix(&engine, &source, &parts.method, &parts.headers)
                .await
    {
        engine.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
        if !private && !engine.policy.read().unwrap().incognito {
            crate::event!(
                "request {} {} partial-cache={}",
                &id[..12],
                parts.method,
                response.status().as_u16()
            );
        }
        return response;
    }
    if parts
        .headers
        .get("x-fn-media-cache-only")
        .is_some_and(|value| value == "1")
    {
        return (
            StatusCode::GATEWAY_TIMEOUT,
            "cache-only request missed; origin body was not fetched",
        )
            .into_response();
    }
    let url = match source.url() {
        Ok(url) => url,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid proxy path").into_response(),
    };
    let progress_body =
        if parts.method == Method::POST && parts.uri.path() == "/v/api/v1/play/record" {
            Some(Arc::new(Mutex::new(Some(Vec::new()))))
        } else {
            None
        };
    let capture = progress_body.clone();
    let stream = body.into_data_stream().inspect_ok(move |chunk| {
        if let Some(capture) = &capture {
            let mut guard = capture.lock().unwrap();
            if let Some(buffer) = guard.as_mut() {
                if buffer.len() + chunk.len() <= 64 * 1024 {
                    buffer.extend_from_slice(chunk);
                } else {
                    *guard = None;
                }
            }
        }
    });
    let mut outgoing_headers = clean_headers(&parts.headers);
    if play_info {
        outgoing_headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
    }
    let result = engine
        .client
        .request(parts.method.clone(), url)
        .headers(outgoing_headers)
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await;
    if let Some(capture) = progress_body {
        let status = result
            .as_ref()
            .map(|response| response.status().as_u16())
            .unwrap_or(502);
        engine.record_progress(&source, capture.lock().unwrap().take(), status);
    }
    let upstream = match result {
        Ok(response) => response,
        Err(_) => return (StatusCode::BAD_GATEWAY, "upstream request failed").into_response(),
    };
    let status = upstream.status();
    if parts.method == Method::GET {
        engine.observe(source.clone(), status.as_u16(), upstream.headers());
    }
    let mut headers = clean_headers(upstream.headers());
    if let Some(location) = text_header(&headers, "location")
        && let Ok(destination) = source.url().and_then(|url| Ok(url.join(&location)?))
    {
        if destination.origin() == source.url().unwrap().origin() {
            let mut relative = destination.path().to_owned();
            if let Some(query) = destination.query() {
                relative.push('?');
                relative.push_str(query);
            }
            if let Some(fragment) = destination.fragment() {
                relative.push('#');
                relative.push_str(fragment);
            }
            if let Ok(value) = HeaderValue::from_str(&relative) {
                headers.insert(header::LOCATION, value);
            }
        } else {
            engine
                .counters
                .external_redirects
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    headers.insert("x-fn-media-cache", HeaderValue::from_static("MISS"));
    if !private && !engine.policy.read().unwrap().incognito {
        crate::event!(
            "request {} {} origin={} elapsed_ms={}",
            &id[..12],
            parts.method,
            status.as_u16(),
            started.elapsed().as_millis()
        );
    }
    let playback = (parts.method == Method::GET
        && matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT)
        && crate::protocol::media_candidate(upstream.headers(), &source.target))
    .then(|| engine.origin_playback());
    let capture_info = play_info
        && !local_info
        && status.is_success()
        && upstream
            .content_length()
            .is_none_or(|length| length <= local_history::INFO_LIMIT as u64)
        && text_header(&headers, "content-encoding")
            .is_none_or(|value| value.eq_ignore_ascii_case("identity"));
    let mut capture_info = capture_info.then(|| (source.clone(), Vec::new()));
    let state = engine.clone();
    let stream = upstream.bytes_stream().map_ok(move |chunk| {
        let _ = &playback;
        if let Some((source, buffer)) = &mut capture_info {
            if buffer.len() + chunk.len() > local_history::INFO_LIMIT {
                capture_info = None;
            } else {
                buffer.extend_from_slice(&chunk);
                if buffer.iter().rev().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'}')
                    && let Ok(value) = serde_json::from_slice(buffer)
                {
                    local_history::remember_display_info(&state, source, &value);
                    capture_info = None;
                }
            }
        }
        state
            .counters
            .origin_bytes
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
        chunk
    });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    if local_info && status.is_success() {
        return local_play_info(&engine, &source, response).await;
    }
    response
}

async fn local_play_info(engine: &Engine, source: &Source, response: Response) -> Response {
    let (mut parts, body) = response.into_parts();
    let error = || {
        (
            StatusCode::BAD_GATEWAY,
            "cannot read local playback information",
        )
            .into_response()
    };
    let Ok(bytes) = to_bytes(body, local_history::INFO_LIMIT).await else {
        return error();
    };
    let Ok(mut value) = serde_json::from_slice(&bytes) else {
        return error();
    };
    if local_history::overlay_play_info(engine, source, &mut value).is_err() {
        return error();
    }
    for header in [
        header::CONTENT_LENGTH,
        header::CONTENT_ENCODING,
        header::ETAG,
    ] {
        parts.headers.remove(header);
    }
    parts
        .headers
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Response::from_parts(parts, Json(value).into_response().into_body())
}

async fn cached_segment(
    engine: &Arc<Engine>,
    source: &Source,
    method: &Method,
    incoming: &HeaderMap,
) -> Option<Response> {
    if [
        "if-range",
        "if-none-match",
        "if-modified-since",
        "if-match",
        "if-unmodified-since",
    ]
    .iter()
    .any(|name| incoming.contains_key(*name))
    {
        return None;
    }
    let segments = engine.store.segments(&source.cache_id()).ok()?;
    let first = segments.first()?;
    let requested = if method == Method::HEAD {
        Range::Full
    } else {
        parse_range(
            incoming.get(header::RANGE).and_then(|v| v.to_str().ok()),
            first.total,
        )
    };
    let (segment, start, length, status) = match requested {
        Range::Partial(start, end) => {
            let segment = segments
                .iter()
                .find(|s| s.start <= start && end < s.start + s.length)?;
            (segment, start, end - start + 1, StatusCode::PARTIAL_CONTENT)
        }
        Range::Full if method == Method::HEAD => (first, 0, first.total, StatusCode::OK),
        Range::Full => {
            let segment = segments
                .iter()
                .find(|s| s.start == 0 && s.length == s.total)?;
            (segment, 0, segment.total, StatusCode::OK)
        }
        _ => return None,
    };
    engine
        .revalidate_native(source, &segment.file_version, segment.total)
        .await?;
    let mut file = tokio::fs::File::open(engine.store.segment_file(segment))
        .await
        .ok()?;
    if file.metadata().await.ok()?.len() != segment.length {
        return None;
    }
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).ok()?,
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&segment.content_type).ok()?,
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert("x-fn-media-cache", HeaderValue::from_static("HIT"));
    headers.insert("x-fn-media-cache-kind", HeaderValue::from_static("segment"));
    if status == StatusCode::PARTIAL_CONTENT {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!(
                "bytes {start}-{}/{total}",
                start + length - 1,
                total = segment.total
            ))
            .ok()?,
        );
    }
    if method != Method::HEAD && length > 0 {
        file.seek(std::io::SeekFrom::Start(start - segment.start))
            .await
            .ok()?;
        let state = engine.clone();
        let stream = ReaderStream::new(file.take(length)).map_ok(move |chunk| {
            state
                .counters
                .cache_bytes
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            chunk
        });
        *response.body_mut() = Body::from_stream(stream);
    }
    Some(response)
}

async fn cached(
    engine: &Arc<Engine>,
    source: &Source,
    item: &Download,
    method: &Method,
    incoming: &HeaderMap,
) -> Option<Response> {
    let file_guard = engine.transfers.file_gate(&item.id).read_owned().await;
    // Complex conditional requests and multipart ranges retain upstream semantics.
    if [
        "if-none-match",
        "if-modified-since",
        "if-match",
        "if-unmodified-since",
    ]
    .iter()
    .any(|name| incoming.contains_key(*name))
    {
        return None;
    }
    let size = item.total?;
    let mut range = parse_range(
        incoming.get(header::RANGE).and_then(|v| v.to_str().ok()),
        size,
    );
    if range == Range::Unsupported {
        return None;
    }
    if let Some(if_range) = incoming.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) {
        let matches = strong_etag(&item.etag).is_some_and(|e| e == if_range)
            || item.last_modified.as_deref() == Some(if_range);
        if !matches {
            range = Range::Full;
        }
    }
    // Revalidate access and identity on every hit. Offline authentication is out of scope.
    let representation = engine.revalidate(source, item).await?;
    let mut file = tokio::fs::File::open(engine.store.file(&item.id, false))
        .await
        .ok()?;
    if file.metadata().await.ok()?.len() != size {
        return None;
    }
    let (status, start, length) = match range {
        Range::Full => (StatusCode::OK, 0, size),
        Range::Partial(start, end) => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        Range::Unsatisfiable => (StatusCode::RANGE_NOT_SATISFIABLE, 0, 0),
        Range::Unsupported => return None,
    };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).ok()?,
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert("x-fn-media-cache", HeaderValue::from_static("HIT"));
    // Preserve end-to-end representation headers without cookies or redirect metadata.
    for name in [
        header::CONTENT_TYPE,
        header::ETAG,
        header::LAST_MODIFIED,
        header::CACHE_CONTROL,
        header::VARY,
        header::CONTENT_DISPOSITION,
    ] {
        if let Some(value) = representation.get(&name) {
            headers.insert(name, value.clone());
        }
    }
    if status == StatusCode::PARTIAL_CONTENT {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + length - 1)).ok()?,
        );
    } else if status == StatusCode::RANGE_NOT_SATISFIABLE {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes */{size}")).ok()?,
        );
    }
    if method != Method::HEAD && length > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await.ok()?;
        let state = engine.clone();
        let stream = ReaderStream::new(file.take(length)).map_ok(move |chunk| {
            let _ = &file_guard;
            state
                .counters
                .cache_bytes
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            chunk
        });
        *response.body_mut() = Body::from_stream(stream);
    }
    Some(response)
}
