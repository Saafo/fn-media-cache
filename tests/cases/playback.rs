use crate::support::origin::native_data;
use crate::support::*;
use reqwest::Method;
use serde_json::json;
use std::{fs, sync::atomic::Ordering, time::Duration};

async fn prefix(
    h: &Harness,
    origin: &Origin,
    guid: &str,
    generic: bool,
) -> (String, String, String) {
    let path = if generic {
        format!("/{guid}.mp4")
    } else {
        format!("/v/api/v1/media/range/{guid}")
    };
    let token = if generic { AUTH } else { TOKEN };
    origin.interrupt(if generic { &path } else { guid });
    let id = h.capture(&path, token, false).await;
    let job = h.wait_job(&id, "failed").await;
    assert!(
        job["downloaded"].as_u64().unwrap() > 0
            && job["downloaded"].as_u64().unwrap() < 8 * 1024 * 1024
    );
    h.command(&id, "cancel").await;
    (id, path, token.into())
}
async fn cache_only(h: &Harness, path: &str, token: &str, range: &str) -> Reply {
    h.send(
        &h.proxy,
        path,
        Method::GET,
        &[
            ("Authorization", token),
            ("Range", range),
            ("X-Fn-Media-Cache-Only", "1"),
        ],
        None,
    )
    .await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_controls_and_partial_playback() {
    let origin = Origin::new(Mode::Task);
    let mut h = Harness::new(std::slice::from_ref(&origin.url)).await;
    let mut policy=h.policy(json!({"bytes_per_second":0,"retry_max_attempts":0,"pause_during_playback":false,"scheduler_interval_seconds":1})).await;
    let (id, path, token) = prefix(&h, &origin, "prefix", false).await;
    let size = h.job(&id).await["downloaded"].as_u64().unwrap();
    origin.change(|s| s.block = true);
    let reply = cache_only(&h, &path, &token, "bytes=0-31").await;
    assert_eq!(reply.status, 206);
    assert_eq!(reply.header("x-fn-media-cache-kind"), "partial-file");
    assert_eq!(reply.body, native_data(Mode::Task, "prefix", 1)[..32]);
    assert_eq!(
        cache_only(
            &h,
            &path,
            &token,
            &format!("bytes={}-{}", size - 1, size + 31)
        )
        .await
        .status,
        504
    );
    origin.change(|s| s.block = false);
    let before = origin.snapshot().calls.len();
    let reply = h
        .get(
            &path,
            &token,
            Some(&format!("bytes={}-{}", size - 32, size + 31)),
        )
        .await;
    assert_eq!(reply.status, 206);
    assert_eq!(reply.header("x-fn-media-cache"), "PARTIAL");
    assert_eq!(
        reply.body,
        native_data(Mode::Task, "prefix", 1)[size as usize - 32..size as usize + 32]
    );
    assert!(
        origin.snapshot().calls[before..]
            .iter()
            .any(|c| c.path == path
                && c.range.as_deref() == Some(&format!("bytes={}-{}", size, size + 31)))
    );
    let reply = h.get(&path, &token, Some("bytes=0-")).await;
    assert_eq!(reply.status, 206);
    assert_eq!(reply.body, native_data(Mode::Task, "prefix", 1));
    assert_eq!(reply.header("x-fn-media-cache"), "PARTIAL");
    let stats = h.api("/stats", None).await;
    let row = stats["playback"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["download_id"] == id)
        .unwrap();
    assert!(
        row["local_bytes"].as_u64().unwrap() >= size && row["upstream_bytes"].as_u64().unwrap() > 0
    );
    assert!(!stats.to_string().contains(TOKEN));
    case(
        "native cached prefix, cross-boundary and open-ended ranges return exact bytes; only the missing suffix is fetched",
    );
    origin.change(|s| s.version = 2);
    assert_eq!(
        cache_only(&h, &path, &token, "bytes=0-31").await.status,
        504
    );
    origin.change(|s| {
        s.version = 1;
        s.allowed = false;
    });
    assert_eq!(
        cache_only(&h, &path, &token, "bytes=0-31").await.status,
        504
    );
    origin.change(|s| s.allowed = true);
    case("partial cache retains file-version and authorization checks");

    let (generic, gpath, gtoken) = prefix(&h, &origin, "generic", true).await;
    let size = h.job(&generic).await["downloaded"].as_u64().unwrap() as usize;
    let reply = h
        .get(
            &gpath,
            &gtoken,
            Some(&format!("bytes={}-{}", size - 8, size + 7)),
        )
        .await;
    assert_eq!(reply.status, 206);
    assert_eq!(reply.header("x-fn-media-cache"), "PARTIAL");
    assert_eq!(reply.body, data(8 * 1024 * 1024, None)[size - 8..size + 8]);
    origin.change(|s| s.etag = "\"changed\"".into());
    assert_eq!(
        cache_only(&h, &gpath, &gtoken, "bytes=0-7").await.status,
        504
    );
    origin.change(|s| s.etag = "\"v1\"".into());
    case("generic ETag-backed partial files follow the same range rules");

    let a = h.native("queue-a", TOKEN, true).await;
    let b = h.native("queue-b", TOKEN, true).await;
    let c = h.native("queue-c", TOKEN, true).await;
    h.command(&c, "first").await;
    h.command(&a, "down").await;
    let expected = vec![c.clone(), b.clone(), a.clone()];
    let order = h
        .api("/downloads", None)
        .await
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| {
            p["id"]
                .as_str()
                .filter(|id| expected.iter().any(|v| v == id))
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(order, expected);
    h.stop(false).await;
    h.db()
        .execute(
            "UPDATE downloads SET state='queued' WHERE id IN (?1,?2,?3)",
            rusqlite::params![c, b, a],
        )
        .unwrap();
    policy["bytes_per_second"] = json!(128 * 1024);
    h.db()
        .execute(
            "UPDATE settings SET value=?1 WHERE key='automation_policy'",
            [policy.to_string()],
        )
        .unwrap();
    h.start().await;
    until("ordered download progress", || async {
        let j = h.job(&c).await;
        j["state"] == "downloading" && j["downloaded"].as_u64().unwrap() > 0
    })
    .await;
    let j = h.job(&c).await;
    assert!(j["bytes_per_second"].as_u64().unwrap() > 0 && !j["eta_seconds"].is_null());
    h.command(&c, "cancel").await;
    h.wait_job(&b, "downloading").await;
    h.command(&b, "cancel").await;
    h.command(&a, "cancel").await;
    h.restart(false).await;
    for id in &expected {
        assert_eq!(h.job(id).await["state"], "cancelled");
    }
    case(
        "queue order survives restart; speed and ETA are reported; cancellation does not auto-resume",
    );

    let (reset, reset_path, reset_token) = prefix(&h, &origin, "generation", false).await;
    origin.hold();
    let mut held = h
        .client
        .get(format!("{}{reset_path}", h.proxy))
        .header("Authorization", &reset_token)
        .header("Range", "bytes=0-1048575")
        .send()
        .await
        .unwrap();
    let first = held.chunk().await.unwrap().unwrap();
    assert!(origin.snapshot().held.load(Ordering::Relaxed));
    let original = native_data(Mode::Task, "generation", 1);
    origin.change(|s| s.version = 2);
    h.policy(json!({"bytes_per_second":0})).await;
    h.command(&reset, "resume").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(fs::read(h.cache(&reset, "part")).unwrap()[0], original[0]);
    origin.release();
    let mut result = first.to_vec();
    result.extend(held.bytes().await.unwrap());
    assert_eq!(result, original[..1048576]);
    h.wait_job(&reset, "complete").await;
    assert_eq!(
        fs::read(h.cache(&reset, "bin")).unwrap(),
        native_data(Mode::Task, "generation", 2)
    );
    origin.change(|s| s.version = 1);
    case("resuming a replaced file waits for prior readers and never mixes file generations");

    origin.hold();
    let mut held = h
        .client
        .get(format!("{}{path}", h.proxy))
        .header("Authorization", &token)
        .header("Range", "bytes=0-1048575")
        .send()
        .await
        .unwrap();
    let first = held.chunk().await.unwrap().unwrap();
    assert!(origin.snapshot().held.load(Ordering::Relaxed));
    h.command(&id, "delete").await;
    assert_eq!(h.job(&id).await["state"], "deleting");
    assert!(h.cache(&id, "part").exists());
    h.policy(json!({"incognito":true})).await;
    h.policy(json!({"incognito":false})).await;
    origin.release();
    let mut result = first.to_vec();
    result.extend(held.bytes().await.unwrap());
    assert_eq!(result, native_data(Mode::Task, "prefix", 1)[..1048576]);
    assert!(
        !h.api("/stats", None).await["playback"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["download_id"] == id)
    );
    h.wait_job(&id, "removed").await;
    assert!(!h.cache(&id, "part").exists());
    case(
        "deleting during playback waits for readers, then removes the local bytes without corrupting the response",
    );
    h.policy(json!({"incognito":true})).await;
    assert_eq!(h.api("/stats", None).await["playback"], json!([]));
    h.get(&gpath, &gtoken, Some("bytes=0-15")).await;
    assert_eq!(h.api("/stats", None).await["playback"], json!([]));
    case("incognito clears and suppresses per-media playback measurements");
}
