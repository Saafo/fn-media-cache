use crate::support::*;
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

const RECORD: &str = "/v/api/v1/play/record";
const LIST: &str = "/v/api/v1/play/list";
const OLD: &str = "old-session";

async fn write(h: &Harness, method: Method, path: &str, token: &str, body: Value) -> Reply {
    h.send(
        &h.proxy,
        path,
        method,
        &[
            ("Authorization", token),
            ("Content-Type", "application/json"),
        ],
        Some(&body.to_string()),
    )
    .await
}

async fn info(h: &Harness, item: &str, media: &str) -> Value {
    let reply = write(
        h,
        Method::POST,
        "/v/api/v1/play/info",
        OLD,
        json!({"item_guid":item,"media_guid":media}),
    )
    .await;
    assert_eq!(reply.status, 200);
    reply.json()["data"].clone()
}

async fn record(h: &Harness, token: &str, guid: &str, position: u64) {
    let reply = write(h, Method::POST, RECORD, token, json!({"item_guid":guid,"media_guid":guid,"ts":position,"duration":1000,"private":"must-not-store"})).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("x-fn-media-cache-privacy"), "local");
    assert_eq!(reply.json()["code"], 0);
}

async fn list(h: &Harness, token: &str) -> Vec<Value> {
    let reply = h.get(LIST, token, None).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("cache-control"), "no-store");
    reply.json()["data"].as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playback_readings_use_the_clients_catalog_metadata() {
    let origin = Origin::new(Mode::Recovery);
    let h = Harness::new(std::slice::from_ref(&origin.url)).await;
    assert_eq!(info(&h, "one", "one").await["ts"], 900);
    assert_eq!(h.api("/stats", None).await["playback"], json!([]));
    assert_eq!(
        h.get(
            "/v/api/v1/media/range/one",
            "other-account",
            Some("bytes=0-15")
        )
        .await
        .status,
        206
    );
    assert!(h.api("/stats", None).await["playback"][0]["item"].is_null());
    assert_eq!(
        h.get(
            "/v/api/v1/media/range/one?variant=direct",
            OLD,
            Some("bytes=0-15")
        )
        .await
        .status,
        206
    );
    let stats = h.api("/stats", None).await;
    let row = stats["playback"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["item"]["guid"] == "one")
        .unwrap();
    assert!(row["download_id"].is_null());
    assert_eq!(row["item"]["tv_title"], "Fixture series");
    assert_eq!(row["item"]["season_number"], 2);
    assert_eq!(row["item"]["episode_number"], 5);
    assert!(!stats.to_string().contains("must-not-store"));

    h.policy(json!({"incognito":true})).await;
    info(&h, "one", "one").await;
    assert_eq!(
        h.get("/v/api/v1/media/range/one", OLD, Some("bytes=0-15"))
            .await
            .status,
        206
    );
    assert_eq!(h.api("/stats", None).await["playback"], json!([]));
    h.policy(json!({"incognito":false})).await;
    assert_eq!(
        h.get("/v/api/v1/media/range/one", OLD, Some("bytes=0-15"))
            .await
            .status,
        206
    );
    assert!(h.api("/stats", None).await["playback"][0]["item"].is_null());
    case(
        "playback labels are captured from normal setup without download or history writes, and never in incognito mode",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_is_local_per_account_and_survives_restart() {
    let origin = Origin::new(Mode::Recovery);
    let mut h = Harness::new(std::slice::from_ref(&origin.url)).await;
    assert_eq!(
        h.get(LIST, OLD, None).await.json()["data"][0]["guid"],
        "cloud-only"
    );
    h.policy(json!({"local_history":true})).await;
    assert!(list(&h, OLD).await.is_empty());
    assert_eq!(
        h.send(&h.proxy, LIST, Method::GET, &[], None).await.status,
        401
    );
    let initial = info(&h, "one", "one").await;
    assert_eq!(initial["ts"], 0);
    assert_eq!(initial["item"]["watched_ts"], 0);
    assert_eq!(initial["play_config"]["skip_opening"], 5);
    assert!(
        list(&h, OLD).await.is_empty(),
        "opening playback info is not a play report"
    );
    record(&h, OLD, "one", 42).await;
    info(&h, "two", "two").await;
    record(&h, OLD, "two", 73).await;
    let updated = write(
        &h,
        Method::POST,
        RECORD,
        OLD,
        json!({"media_guid":"one","ts":98}),
    )
    .await;
    assert_eq!(updated.status, 200);
    h.login(OLD).await;

    let rows = list(&h, OLD).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["guid"], "one");
    assert_eq!(rows[0]["ts"], 98);
    assert_eq!(rows[0]["title"], "Synthetic episode");
    assert_eq!(rows[0]["poster"], "fixture-poster");
    assert_eq!(rows[0]["media_guid"], "one");
    assert_eq!(rows[0]["video_guid"], "video-one");
    assert_eq!(rows[0]["duration"], 1000);
    assert_eq!(rows[1]["guid"], "two");
    assert!(!json!(rows).to_string().contains("must-not-store"));
    assert_eq!(info(&h, "one", "one").await["ts"], 98);
    assert_eq!(info(&h, "one", "alternate-file").await["ts"], 0);
    h.login("other-account").await;
    assert!(list(&h, "other-account").await.is_empty());

    assert_eq!(
        write(
            &h,
            Method::POST,
            "/v/api/v1/item/watched",
            OLD,
            json!({"item_guid":"one"})
        )
        .await
        .status,
        200
    );
    assert_eq!(
        write(
            &h,
            Method::POST,
            "/v/api/v1/item/one/playing",
            OLD,
            json!({})
        )
        .await
        .status,
        200
    );
    assert_eq!(
        h.get("/v/api/v1/media/range/one", OLD, Some("bytes=0-31"))
            .await
            .status,
        206
    );
    h.restart(false).await;
    assert_eq!(
        h.api("/automation", None).await["policy"]["local_history"],
        true
    );
    assert_eq!(list(&h, OLD).await[0]["ts"], 98);
    assert_eq!(info(&h, "one", "one").await["ts"], 98);

    origin.change(|state| {
        state.revoked_tokens.insert(OLD.into());
    });
    h.login("renewed-session").await;
    assert!(
        list(&h, OLD).await.is_empty(),
        "superseded credentials cannot read the account record"
    );
    assert_eq!(list(&h, "renewed-session").await[0]["ts"], 98);
    let removed = write(
        &h,
        Method::DELETE,
        "/v/api/v2/play/record",
        "renewed-session",
        json!({"item_guid":"one"}),
    )
    .await;
    assert_eq!(removed.status, 200);
    assert_eq!(removed.json()["data"], true);
    assert_eq!(list(&h, "renewed-session").await.len(), 1);
    assert_eq!(list(&h, "renewed-session").await[0]["guid"], "two");
    assert!(
        origin.snapshot().writes.is_empty(),
        "local history must never send writes upstream"
    );
    assert_eq!(
        origin
            .snapshot()
            .calls
            .iter()
            .filter(|call| call.path == LIST)
            .count(),
        1,
        "only the initial sync-mode list request should reach upstream"
    );

    h.policy(json!({"local_history":false,"incognito":true}))
        .await;
    assert_eq!(
        write(
            &h,
            Method::POST,
            RECORD,
            "renewed-session",
            json!({"item_guid":"incognito-item","ts":10})
        )
        .await
        .header("x-fn-media-cache-privacy"),
        "suppressed"
    );
    h.policy(json!({"incognito":false})).await;
    assert_eq!(
        h.get(LIST, "renewed-session", None).await.json()["data"][0]["guid"],
        "cloud-only"
    );
    assert!(
        origin.snapshot().writes.is_empty(),
        "switching modes must never replay local entries"
    );
    h.policy(json!({"local_history":true})).await;
    assert_eq!(list(&h, "renewed-session").await[0]["guid"], "two");
    let removed = h
        .send(
            &h.proxy,
            "/v/api/v1/play/record?item_guid=two",
            Method::DELETE,
            &[("Authorization", "renewed-session")],
            None,
        )
        .await;
    assert_eq!(removed.status, 200);
    assert!(list(&h, "renewed-session").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_reports_trigger_prefetch_without_using_cloud_completion() {
    let origin = Origin::new(Mode::Catalog);
    let h = Harness::new(std::slice::from_ref(&origin.url)).await;
    h.login(TOKEN).await;
    h.policy(json!({
        "local_history":true,"bytes_per_second":0,
        "scheduler_interval_seconds":1,"playback_cooldown_seconds":0
    }))
    .await;
    origin.change(|state| {
        state.watched.insert("one".into());
        state.watched.insert("two".into());
    });
    let first = h.queue(&["one"]).await.remove(0);
    h.wait_job(&first, "complete").await;
    assert!(h.managed("one").await["completed_at"].is_null());

    // Simulate a completion saved by the old sync policy before switching.
    h.db().execute(
        "UPDATE managed_media SET completed_at=unixepoch()-5,upstream_watched=1 WHERE item_guid='one'",
        [],
    ).unwrap();
    h.policy(json!({"retention_seconds":0,"auto_prefetch":true,"prefetch_after_seconds":1}))
        .await;
    record(&h, TOKEN, "one", 42).await;
    assert!(
        !h.api("/automation", None).await["managed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["item"]["guid"] == "two")
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    record(&h, TOKEN, "one", 44).await;
    until("local play triggers prefetch", || async {
        h.api("/automation", None).await["managed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["item"]["guid"] == "two")
    })
    .await;
    let second = h.managed("two").await;
    assert!(
        second["completed_at"].is_null(),
        "ignore cloud completion on enqueue"
    );
    h.wait_job(second["download_id"].as_str().unwrap(), "complete")
        .await;
    assert_eq!(
        h.job(&first).await["state"],
        "complete",
        "cloud completion must not remove the cache"
    );
    h.retain("one", json!({"completed":true})).await;
    h.wait_job_absent(&first).await;
    assert!(list(&h, TOKEN).await.iter().any(|row| row["guid"] == "one"));
    assert!(origin.snapshot().writes.is_empty());
}
