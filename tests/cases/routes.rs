use crate::support::*;
use reqwest::Method;
use serde_json::json;
use std::{fs, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_route_selection() {
    let servers: Vec<_> = (0..4).map(|_| Origin::new(Mode::Route)).collect();
    servers[0].change(|s| s.delay = Duration::from_millis(25));
    servers[1].change(|s| s.delay = Duration::from_millis(1));
    servers[2].change(|s| s.corrupt = true);
    servers[3].change(|s| s.allowed = false);
    let origins: Vec<_> = servers.iter().map(|s| s.url.clone()).collect();
    let h = Harness::new(&origins).await;
    let path = "/v/api/v1/media/range/movie";
    assert_eq!(h.get(path, TOKEN, Some("bytes=0-15")).await.status, 206);
    until("route measurements", || async {
        let r = h.api("/routes", None).await;
        !r["measured_at_ms"].is_null() && r["measuring"] == false
    })
    .await;
    let report = h.api("/routes", None).await;
    assert_eq!(report["selected"], origins[1]);
    assert_eq!(
        report["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["status"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["ready", "ready", "mismatch", "failed"]
    );
    assert!(
        report["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["sample_bytes"].as_u64().unwrap() <= 1048576)
    );
    case(
        "first video automatically benchmarks routes; fastest valid route wins; mismatched content and denied session are excluded",
    );
    let observation = h.api("/observations", None).await;
    let id = observation
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["path"] == path)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    h.api(
        "/downloads",
        Some(json!({"observation_id":id,"paused":true})),
    )
    .await;
    let count = servers[0].snapshot().calls.len();
    let samples = h
        .api(&format!("/downloads/{id}/cache-samples"), Some(json!({})))
        .await;
    assert_eq!(samples["download_origin"], origins[1]);
    assert!(samples["downloaded_bytes"].as_u64().unwrap() <= 3 * 1024 * 1024);
    assert_eq!(h.job(&id).await["downloaded"], 0);
    assert_eq!(servers[0].snapshot().calls.len(), count);
    let reply = h
        .send(
            &h.proxy,
            path,
            Method::GET,
            &[
                ("Authorization", TOKEN),
                ("Range", "bytes=0-31"),
                ("X-Fn-Media-Cache-Only", "1"),
            ],
            None,
        )
        .await;
    assert_eq!(reply.status, 206);
    assert_eq!(reply.header("x-fn-media-cache"), "HIT");
    assert_eq!(reply.body, data(2 * 1024 * 1024, None)[..32]);
    case(
        "sample download uses fastest route, shares the primary cache identity, and leaves the full task paused",
    );
    servers[1].interrupt("movie");
    let before: Vec<_> = servers.iter().map(|s| s.snapshot().calls.len()).collect();
    h.command(&id, "resume").await;
    h.wait_job(&id, "complete").await;
    assert_eq!(
        fs::read(h.cache(&id, "bin")).unwrap(),
        data(2 * 1024 * 1024, None)
    );
    assert!(
        servers[1].snapshot().calls[before[1]..]
            .iter()
            .any(|c| c.range.as_deref() == Some("bytes=0-"))
    );
    assert!(
        servers[0].snapshot().calls[before[0]..]
            .iter()
            .any(|c| c.range.as_deref() == Some("bytes=131072-"))
    );
    assert!(
        !servers[2]
            .snapshot()
            .calls
            .iter()
            .any(|c| c.range.as_deref().is_some_and(|r| r.ends_with('-')))
    );
    assert_eq!(h.api("/downloads", None).await.as_array().unwrap().len(), 1);
    case(
        "interrupted fastest route automatically resumes on the next verified route with identical complete bytes and no duplicate task",
    );
    let progress = "{\"media_guid\":\"movie\",\"ts\":42,\"duration\":100}";
    assert_eq!(
        h.send(
            &h.proxy,
            "/v/api/v1/play/record",
            Method::POST,
            &[
                ("Authorization", TOKEN),
                ("Content-Type", "application/json")
            ],
            Some(progress)
        )
        .await
        .status,
        200
    );
    assert_eq!(
        servers[0]
            .snapshot()
            .writes
            .iter()
            .map(|w| w.2.clone())
            .collect::<Vec<_>>(),
        [progress.as_bytes()]
    );
    assert!(servers[1..].iter().all(|s| s.snapshot().writes.is_empty()));
    assert_eq!(
        h.api("/progress-events", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    case(
        "playback progress is forwarded once to the primary, never replayed by measurement or failover",
    );
    servers[1].change(|s| s.version = 2000);
    let changed = h
        .api("/routes/probe", Some(json!({"download_id":id})))
        .await;
    assert_eq!(changed["selected"], origins[0]);
    assert_eq!(changed["results"][1]["status"], "mismatch");
    case("matching bytes with a different file version cannot be selected");
    servers[1].change(|s| s.version = 1000);
    servers[0].change(|s| s.down = true);
    let available = h
        .api("/routes/probe", Some(json!({"download_id":id})))
        .await;
    assert_eq!(available["selected"], origins[1]);
    assert_eq!(available["results"][0]["status"], "failed");
    case("a previously validated alternate remains usable if the primary video route later fails");
}
