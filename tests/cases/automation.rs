use crate::support::origin::native_data;
use crate::support::*;
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_prefetch_and_cleanup() {
    let origin = Origin::new(Mode::Catalog);
    let mut h = Harness::new(std::slice::from_ref(&origin.url)).await;
    assert_eq!(
        h.get("/v/api/v1/session-context", TOKEN, None).await.status,
        200
    );
    h.policy(
        json!({"bytes_per_second":0,"scheduler_interval_seconds":1,"playback_cooldown_seconds":0}),
    )
    .await;
    assert_eq!(
        h.api("/library/search", Some(json!({"query":"剧集"})))
            .await["items"][0]["guid"],
        "show"
    );
    assert_eq!(
        h.api("/library/browse", Some(json!({"item_guid":"season-one"})))
            .await["items"][0]["guid"],
        "one"
    );
    let first = h.queue(&["one"]).await.remove(0);
    h.wait_job(&first, "complete").await;
    assert_eq!(
        fs::read(h.cache(&first, "bin")).unwrap(),
        native_data(Mode::Catalog, "one", 1)
    );
    assert!(!h.api("/sessions", None).await.to_string().contains(TOKEN));
    case(
        "search with signed Unicode query and download an episode without any prior playback request",
    );

    origin.hold();
    let held = h
        .client
        .get(format!("{}/v/api/v1/media/range/hold?hold=1", h.proxy))
        .header("Authorization", TOKEN)
        .header("Range", "bytes=0-31")
        .send()
        .await
        .unwrap();
    until("active playback", || async {
        h.api("/automation", None).await["playing"] == true
    })
    .await;
    h.policy(json!({"bytes_per_second":1024*1024})).await;
    let second = h.queue(&["two"]).await.remove(0);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(h.job(&second).await["downloaded"], 0);
    let started = Instant::now();
    origin.release();
    assert_eq!(held.bytes().await.unwrap().len(), 32);
    h.wait_job(&second, "complete").await;
    assert!(started.elapsed() >= Duration::from_millis(1800));
    case(
        "an active player stops background transfer; release resumes it with the configured byte rate",
    );

    h.policy(json!({"auto_prefetch":true,"bytes_per_second":0}))
        .await;
    h.progress("one").await;
    until("next episode prefetch", || async {
        h.api("/automation", None).await["managed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["item"]["guid"] == "three")
    })
    .await;
    assert!(h.managed("one").await["completed_at"].is_null());
    h.progress("three").await;
    until("next season prefetch", || async {
        h.api("/automation", None).await["managed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["item"]["guid"] == "four")
    })
    .await;
    assert!(
        !h.api("/automation", None).await["managed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["item"]["guid"] == "special")
    );
    case(
        "prefetch follows explicit episode and season ordering, skips specials, and near-end progress never marks completion",
    );

    h.policy(json!({"retention_seconds":3600})).await;
    h.retain("two", json!({"completed":true,"keep":true})).await;
    let completed = h.managed("two").await["completed_at"].clone();
    h.progress("one").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.managed("two").await["completed_at"], completed);
    h.backdate("two", 4000).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(h.job(&second).await["state"], "complete");
    origin.hold();
    let held = h
        .client
        .get(format!("{}/v/api/v1/media/range/two?hold=1", h.proxy))
        .header("Authorization", TOKEN)
        .header("Range", "bytes=0-31")
        .send()
        .await
        .unwrap();
    until("held cached media", || async {
        h.api("/automation", None).await["playing"] == true
    })
    .await;
    h.retain("two", json!({"keep":false})).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(h.job(&second).await["state"], "complete");
    origin.release();
    held.bytes().await.unwrap();
    h.wait_job(&second, "removed").await;
    assert!(!h.cache(&second, "bin").exists());
    assert_eq!(h.managed("two").await["suppressed"], true);
    h.progress("one").await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(h.job(&second).await["state"], "removed");
    case(
        "configurable retention honors keep and active readers, preserves first completion time, and suppresses automatic redownload",
    );

    let shared = h.queue(&["alias-one", "alias-two"]).await;
    assert_eq!(shared[0], shared[1]);
    h.wait_job(&shared[0], "complete").await;
    h.retain("alias-one", json!({"completed":true})).await;
    h.backdate("alias-one", 4000).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(h.job(&shared[0]).await["state"], "complete");
    h.retain("alias-two", json!({"completed":true})).await;
    h.backdate("alias-two", 4000).await;
    h.wait_job(&shared[0], "removed").await;
    case("a shared media file is retained until every managed item referencing it is eligible");

    h.policy(json!({"cache_limit_bytes":1024})).await;
    let space = h.queue(&["space"]).await.remove(0);
    h.wait_job(&space, "waiting_space").await;
    h.policy(json!({"cache_limit_bytes":100*1024*1024})).await;
    h.wait_job(&space, "complete").await;
    case(
        "insufficient cache space waits and resumes automatically after raising the configured limit",
    );
    let policy = h.api("/automation", None).await["policy"].clone();
    h.restart(false).await;
    assert_eq!(h.api("/automation", None).await["policy"], policy);
    assert_eq!(h.managed("two").await["suppressed"], true);
    h.retain("two", json!({"completed":false})).await;
    assert_eq!(h.managed("two").await["suppressed"], false);
    assert!(h.managed("two").await["completed_at"].is_null());
    assert_eq!(h.queue(&["two"]).await[0], second);
    h.wait_job(&second, "complete").await;
    assert!(
        origin
            .snapshot()
            .writes
            .iter()
            .all(|w| w.1 == "/v/api/v1/play/record")
    );
    case(
        "policy and tombstones survive restart; explicit re-download works; local completion and cleanup never write upstream state",
    );
    h.policy(json!({"cleanup_enabled":false})).await;
    h.stop(false).await;
    h.db()
        .execute(
            "UPDATE downloads SET state='deleting' WHERE id=?1",
            [&first],
        )
        .unwrap();
    h.db()
        .execute(
            "UPDATE managed_media SET suppressed=1 WHERE download_id=?1",
            [&first],
        )
        .unwrap();
    h.start().await;
    h.wait_job(&first, "removed").await;
    assert!(!h.cache(&first, "bin").exists());
    case("restart finishes an already committed local deletion even after cleanup policy changes");
}
