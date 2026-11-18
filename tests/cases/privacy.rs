use crate::support::*;
use reqwest::Method;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incognito_reports_and_history() {
    let origin = Origin::new(Mode::Privacy);
    let mut h = Harness::new(std::slice::from_ref(&origin.url)).await;
    let headers = [
        ("Authorization", TOKEN),
        ("Content-Type", "application/json"),
    ];
    let path = "/v/api/v1/play/record";
    assert_eq!(
        h.send(
            &h.proxy,
            path,
            Method::POST,
            &headers,
            Some("{\"item_guid\":\"one\",\"media_guid\":\"one\",\"ts\":7}")
        )
        .await
        .status,
        200
    );
    assert_eq!(origin.snapshot().writes.len(), 1);
    assert_eq!(
        h.api("/progress-events", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.policy(json!({"incognito":true,"auto_prefetch":true}))
        .await;
    assert_eq!(origin.snapshot().writes.len(), 1);
    assert_eq!(h.api("/progress-events", None).await, json!([]));
    for (method, path) in [
        (Method::POST, "/v/api/v1/play/record"),
        (Method::POST, "/v/api/v2/play/record"),
        (Method::POST, "/v/api/v1/item/watched"),
        (Method::DELETE, "/v/api/v1/item/watched"),
        (Method::POST, "/v/api/v1/item/one/playing"),
    ] {
        let reply = h
            .send(
                &h.proxy,
                path,
                method,
                &headers,
                Some("{\"item_guid\":\"one\",\"media_guid\":\"one\",\"ts\":123456789}"),
            )
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.json()["code"], 0);
        assert_eq!(reply.header("x-fn-media-cache-privacy"), "suppressed");
    }
    assert_eq!(origin.snapshot().writes.len(), 1);
    assert_eq!(h.api("/progress-events", None).await, json!([]));
    case(
        "known progress, watched and playing writes get local success responses and never reach the upstream",
    );
    let reply = h
        .get("/v/api/v1/media/range/one", TOKEN, Some("bytes=0-31"))
        .await;
    assert_eq!(reply.status, 206);
    assert_eq!(reply.body.len(), 32);
    assert_eq!(
        h.send(
            &h.proxy,
            "/v/api/v1/play/play",
            Method::POST,
            &headers,
            Some("{}")
        )
        .await
        .status,
        200
    );
    assert_eq!(origin.snapshot().writes.len(), 2);
    assert!(origin.snapshot().writes[1].1.ends_with("/play/play"));
    assert_eq!(h.api("/observations", None).await, json!([]));
    assert_eq!(h.api("/progress-events", None).await, json!([]));
    assert_eq!(h.api("/downloads", None).await, json!([]));
    case(
        "media bytes and playback setup still work; incognito requests create no observations, progress events or prefetch tasks",
    );
    h.restart(false).await;
    assert_eq!(
        h.api("/automation", None).await["policy"]["incognito"],
        true
    );
    assert_eq!(
        h.send(&h.proxy, path, Method::POST, &headers, Some("{}"))
            .await
            .status,
        200
    );
    assert_eq!(origin.snapshot().writes.len(), 2);
    h.policy(json!({"incognito":false,"auto_prefetch":false}))
        .await;
    h.send(
        &h.proxy,
        path,
        Method::POST,
        &headers,
        Some("{\"item_guid\":\"one\",\"media_guid\":\"one\",\"ts\":8}"),
    )
    .await;
    assert_eq!(origin.snapshot().writes.len(), 3);
    assert_eq!(
        h.api("/progress-events", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    case(
        "mode survives restart; turning it off forwards only new reports and does not replay suppressed ones",
    );
    assert!(
        !std::fs::read_to_string(h.root().join("launcher.log"))
            .unwrap()
            .contains("123456789")
    );
    let original: serde_json::Value =
        serde_json::from_slice(&origin.snapshot().writes[0].2).unwrap();
    assert_eq!(original["ts"], 7);
    case("old upstream history stays untouched and hidden progress is absent from local logs");
}
