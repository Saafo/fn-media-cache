use crate::support::origin::native_data;
use crate::support::*;
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_sleep_preference_survives_other_policy_changes_and_restart() {
    let origin = Origin::new(Mode::Catalog);
    let mut h = Harness::new(std::slice::from_ref(&origin.url)).await;
    let path = "/automation/prevent-idle-sleep";
    let mut policy = h.policy(json!({ "bytes_per_second": 1234 })).await;
    assert_eq!(h.api(path, None).await, json!({ "enabled": false }));
    let enabled = json!({ "enabled": true });
    assert_eq!(h.api(path, Some(enabled.clone())).await, enabled);
    policy["prevent_idle_sleep"] = json!(true);
    assert_eq!(h.api("/automation", None).await["policy"], policy);

    policy["cleanup_enabled"] = json!(false);
    let mut other_preferences = policy.as_object().unwrap().clone();
    other_preferences.remove("prevent_idle_sleep");
    h.api("/automation/policy", Some(other_preferences.into()))
        .await;
    assert_eq!(h.api("/automation", None).await["policy"], policy);
    h.restart(false).await;
    assert_eq!(h.api(path, None).await, enabled);

    policy["prevent_idle_sleep"] = json!(false);
    h.api("/automation/policy", Some(policy)).await;
    assert_eq!(h.api(path, None).await, json!({ "enabled": false }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn later_episode_playback_shares_the_prefetch_threshold_and_keeps_normal_retention() {
    let origin = Origin::new(Mode::Catalog);
    let mut h = Harness::new(std::slice::from_ref(&origin.url)).await;
    h.login(TOKEN).await;
    let jobs = h.queue(&["one", "two", "four"]).await;
    let [first, second, other_season] = jobs.as_slice() else {
        panic!("expected three downloads");
    };
    for id in &jobs {
        h.wait_job(id, "complete").await;
    }
    assert!(h.managed("one").await["completed_at"].is_null());
    h.retain("four", json!({ "completed": true })).await;
    let policy = h
        .policy(json!({
            "auto_prefetch": true,
            "prefetch_count": 1,
            "prefetch_after_seconds": 1,
            "scheduler_interval_seconds": 1
        }))
        .await;
    assert_eq!(policy["cleanup_previous_episodes"], true);

    h.progress_at("two", 500).await;
    h.progress_at("two", 900).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    h.progress_at("two", 900).await;
    assert_eq!(h.job(first).await["state"], "complete");
    assert!(
        !h.api("/automation", None).await["managed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["item"]["guid"] == "three")
    );

    tokio::time::sleep(Duration::from_millis(1100)).await;
    h.progress_at("two", 902).await;
    h.wait_job_absent(first).await;
    let third = h.managed("three").await["download_id"]
        .as_str()
        .unwrap()
        .to_owned();
    h.wait_job(&third, "complete").await;
    assert_eq!(h.job(second).await["state"], "complete");
    assert_eq!(h.job(other_season).await["state"], "complete");

    h.policy(json!({ "auto_prefetch": false, "local_history": true, "retention_seconds": null }))
        .await;
    h.retain("two", json!({ "keep": true })).await;
    h.progress_at("three", 0).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    h.progress_at("three", 2).await;
    until(
        "later episode playback remembered in local history mode",
        || async {
            h.db()
                .query_row(
                    "SELECT MAX(episode_number) FROM episode_playback",
                    [],
                    |r| r.get::<_, Option<u64>>(0),
                )
                .unwrap()
                == Some(3)
        },
    )
    .await;
    assert_eq!(h.job(second).await["state"], "complete");
    h.restart(false).await;
    h.retain("two", json!({ "keep": false })).await;
    h.wait_job_absent(second).await;
    assert_eq!(h.job(other_season).await["state"], "complete");

    h.retain("three", json!({ "completed": true })).await;
    h.backdate("three", 48 * 60 * 60 + 1).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(h.job(&third).await["state"], "complete");
    h.policy(json!({ "retention_seconds": 48 * 60 * 60 })).await;
    h.wait_job_absent(&third).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_follows_playback_across_verified_seasons_of_the_same_series() {
    let origin = Origin::new(Mode::Catalog);
    let h = Harness::new(std::slice::from_ref(&origin.url)).await;
    h.login(TOKEN).await;
    let jobs = h
        .queue(&["one", "three", "four", "other-one", "special"])
        .await;
    let [first, third, next_season, other_series, special] = jobs.as_slice() else {
        panic!("expected five downloads");
    };
    for id in &jobs {
        h.wait_job(id, "complete").await;
    }
    h.policy(json!({ "prefetch_after_seconds": 1 })).await;

    h.progress_at("four", 0).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(h.job(first).await["state"], "complete");
    assert_eq!(h.job(third).await["state"], "complete");
    h.progress_at("four", 2).await;
    h.wait_job_absent(first).await;
    h.wait_job_absent(third).await;
    for id in [next_season, other_series, special] {
        assert_eq!(h.job(id).await["state"], "complete");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_prefetch_and_cleanup() {
    let origin = Origin::new(Mode::Catalog);
    let mut h = Harness::new(std::slice::from_ref(&origin.url)).await;
    assert_eq!(
        h.get("/v/api/v1/session-context", TOKEN, None).await.status,
        200
    );
    h.policy(
        json!({"bytes_per_second":0,"scheduler_interval_seconds":1,"playback_cooldown_seconds":0,"cleanup_previous_episodes":false}),
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
    for season in ["season-one", "season-two"] {
        let series = h
            .api("/library/series", Some(json!({"item_guid":season})))
            .await;
        assert_eq!(series["guid"], "show");
        assert_eq!(series["kind"], "TV");
    }
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

    assert_eq!(
        h.api("/automation", None).await["policy"]["prefetch_after_seconds"],
        60
    );
    assert_eq!(
        h.api("/automation", None).await["policy"]["retention_seconds"],
        172_800
    );
    h.policy(json!({"auto_prefetch":true,"prefetch_after_seconds":1,"bytes_per_second":0}))
        .await;
    h.progress_at("one", 500).await;
    h.progress_at("one", 900).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    h.progress_at("one", 900).await;
    assert!(
        !h.api("/automation", None).await["managed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["item"]["guid"] == "three")
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    h.progress_at("one", 902).await;
    until("next episode prefetch", || async {
        h.api("/automation", None).await["managed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["item"]["guid"] == "three")
    })
    .await;
    assert!(h.managed("one").await["completed_at"].is_null());
    h.progress_at("three", 10).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    h.progress_at("three", 12).await;
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
    h.wait_job_absent(&second).await;
    assert!(!h.cache(&second, "bin").exists());
    h.progress("one").await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !h.api("/automation", None).await["managed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["item"]["guid"] == "two")
    );
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
    h.wait_job_absent(&shared[0]).await;
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
    assert_eq!(
        h.db()
            .query_row(
                "SELECT COUNT(*) FROM prefetch_suppressions WHERE item_guid='two'",
                [],
                |row| row.get::<_, u32>(0),
            )
            .unwrap(),
        1
    );
    let replacement = h.queue(&["two"]).await.remove(0);
    assert_ne!(replacement, second);
    assert_eq!(h.managed("two").await["suppressed"], false);
    h.wait_job(&replacement, "complete").await;
    assert_eq!(
        h.db()
            .query_row(
                "SELECT COUNT(*) FROM prefetch_suppressions WHERE item_guid='two'",
                [],
                |row| row.get::<_, u32>(0),
            )
            .unwrap(),
        0
    );
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
    let legacy = h.queue(&["alias-one"]).await.remove(0);
    h.wait_job(&legacy, "complete").await;
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
    // Simulate one legacy task whose cache is gone and another with a file still present.
    fs::remove_file(h.cache(&legacy, "bin")).unwrap();
    for id in [&legacy, &space] {
        h.db()
            .execute(
                "UPDATE downloads SET state='removed',downloaded=0 WHERE id=?1",
                [id],
            )
            .unwrap();
        h.db()
            .execute(
                "UPDATE managed_media SET suppressed=1 WHERE download_id=?1",
                [id],
            )
            .unwrap();
    }
    h.start().await;
    h.wait_job_absent(&first).await;
    assert!(!h.cache(&first, "bin").exists());
    assert!(h.find_job(&legacy).await.is_none());
    assert!(h.find_job(&space).await.is_some());
    assert!(h.cache(&space, "bin").exists());
    assert_eq!(
        h.db()
            .query_row(
                "SELECT COUNT(*) FROM prefetch_suppressions WHERE item_guid='alias-one'",
                [],
                |row| row.get::<_, u32>(0),
            )
            .unwrap(),
        1
    );
    case(
        "restart finishes pending deletion and purges empty legacy tasks while preserving existing files",
    );
}
