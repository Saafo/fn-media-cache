use crate::support::origin::native_data;
use crate::support::*;
use reqwest::Method;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_metadata_cache() {
    let origin = Origin::new(Mode::Native);
    let mut h = Harness::new(std::slice::from_ref(&origin.url)).await;
    h.policy(json!({"retry_max_attempts":0})).await;
    let id = h.native("sample", TOKEN, true).await;
    let path = "/v/api/v1/media/range/sample";
    assert_eq!(h.job(&id).await["state"], "paused");
    assert_eq!(h.job(&id).await["downloaded"], 0);
    let sample = h
        .api(&format!("/downloads/{id}/cache-samples"), Some(json!({})))
        .await;
    assert!(sample["downloaded_bytes"].as_u64().unwrap() <= 3 * 1024 * 1024);
    assert_eq!(sample["segments"].as_array().unwrap().len(), 3);
    assert_eq!(h.job(&id).await["state"], "paused");
    assert_eq!(h.job(&id).await["downloaded"], 0);
    case("paused source capture and bounded native sample caching without HEAD/ETag");
    origin.change(|s| s.block = true);
    let payload = native_data(Mode::Native, "sample", 1);
    for offset in [17, payload.len() / 2 + 7, payload.len() - 80] {
        origin.change(|s| s.calls.clear());
        let reply = h
            .get(
                path,
                TOKEN,
                Some(&format!("bytes={offset}-{}", offset + 31)),
            )
            .await;
        assert_eq!(reply.status, 206);
        assert_eq!(reply.body, payload[offset..offset + 32]);
        assert_eq!(reply.header("x-fn-media-cache-kind"), "segment");
        assert_eq!(
            reply.header("content-range"),
            format!("bytes {offset}-{}/{}", offset + 31, payload.len())
        );
        assert_eq!(
            origin
                .snapshot()
                .calls
                .iter()
                .filter(|c| c.path == path)
                .map(|c| c.range.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["bytes=0-0"]
        );
    }
    let head = h
        .send(
            &h.proxy,
            path,
            Method::HEAD,
            &[("Authorization", TOKEN)],
            None,
        )
        .await;
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-length"), payload.len().to_string());
    assert_eq!(
        h.get(path, TOKEN, Some("bytes=2097152-2097183"))
            .await
            .status,
        503
    );
    case(
        "cached head/middle/tail reads work when all origin reads except one-byte authorization are blocked",
    );
    let reply = h
        .send(
            &h.proxy,
            path,
            Method::GET,
            &[
                ("Authorization", TOKEN),
                ("Play-Link", "new-playback-session"),
                ("Range", "bytes=0-31"),
            ],
            None,
        )
        .await;
    assert_eq!(reply.status, 206);
    assert_eq!(reply.header("x-fn-media-cache"), "HIT");
    assert_eq!(reply.body, payload[..32]);
    case("new playback session token reuses the same authorized file cache");
    assert_eq!(
        h.send(
            &h.proxy,
            path,
            Method::GET,
            &[
                ("Authorization", TOKEN),
                ("Range", "bytes=0-31"),
                ("X-Fn-Media-Cache-Only", "1")
            ],
            None
        )
        .await
        .status,
        206
    );
    origin.change(|s| s.calls.clear());
    assert_eq!(
        h.send(
            &h.proxy,
            path,
            Method::GET,
            &[
                ("Authorization", TOKEN),
                ("Range", "bytes=2097152-2097183"),
                ("X-Fn-Media-Cache-Only", "1")
            ],
            None
        )
        .await
        .status,
        504
    );
    assert!(!origin.snapshot().calls.iter().any(|c| c.path == path));
    case("cache-only requests cannot fall back to a remote video body");
    origin.change(|s| s.allowed = false);
    assert_eq!(h.get(path, TOKEN, Some("bytes=0-31")).await.status, 401);
    origin.change(|s| {
        s.allowed = true;
        s.version = 2;
    });
    assert_eq!(h.get(path, TOKEN, Some("bytes=0-31")).await.status, 503);
    assert_eq!(
        h.get(path, "another-session", Some("bytes=0-31"))
            .await
            .status,
        503
    );
    case(
        "permission revocation, metadata version changes and other sessions cannot use stale segments",
    );
    origin.change(|s| s.block = false);
    h.api(&format!("/downloads/{id}/cache-samples"), Some(json!({})))
        .await;
    h.restart(false).await;
    origin.change(|s| s.block = true);
    assert_eq!(
        h.get(path, TOKEN, Some("bytes=0-31")).await.body,
        native_data(Mode::Native, "sample", 2)[..32]
    );
    case("sample cache and version metadata survive process restart");
    origin.change(|s| s.block = false);
    origin.interrupt("small");
    let small = h.native("small", TOKEN, false).await;
    h.wait_job(&small, "failed").await;
    origin.change(|s| s.calls.clear());
    h.command(&small, "resume").await;
    h.wait_job(&small, "complete").await;
    assert!(origin.snapshot().calls.iter().any(|c| {
        c.range
            .as_deref()
            .is_some_and(|r| r.starts_with("bytes=65536-"))
    }));
    origin.change(|s| s.block = true);
    let reply = h.get("/v/api/v1/media/range/small", TOKEN, None).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("x-fn-media-cache"), "HIT");
    assert_eq!(reply.body, native_data(Mode::Native, "small", 2));
    case(
        "complete native file resumes by metadata version and plays without HEAD or HTTP validators",
    );
}
