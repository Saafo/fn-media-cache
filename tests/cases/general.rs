use crate::support::*;
use reqwest::Method;
use serde_json::json;
use std::{fs, time::Duration};

const TARGET: &str = "/movie.mp4?signature=fixture-secret-query";
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_proxy_and_cache() {
    let origin = Origin::new(Mode::Generic);
    let mut h = Harness::with_config(
        std::slice::from_ref(&origin.url),
        "max_cache_bytes=67108864\nread_timeout_seconds=5\n",
    )
    .await;
    h.policy(json!({"retry_max_attempts":0})).await;
    assert_eq!(
        h.send(&h.admin, "/api/stats", Method::GET, &[], None)
            .await
            .status,
        401
    );
    assert_eq!(
        h.send(&h.admin, "/", Method::GET, &[], None).await.status,
        200
    );
    let reply = h.get("/library", AUTH, None).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["title"], "Fixture library");
    let reply = h.get("/redirect", AUTH, None).await;
    assert_eq!(reply.status, 302);
    assert_eq!(reply.header("location"), TARGET);
    assert_eq!(h.get("/external", AUTH, None).await.status, 302);
    assert_eq!(h.api("/stats", None).await["external_redirects"], 1);
    case("transparent control requests, redirect visibility, management authentication");

    let raw=json!({"media_guid":"fixture-media","ts":123,"duration":456,"private_field":"must-not-be-recorded"}).to_string();
    assert_eq!(
        h.send(
            &h.proxy,
            "/v/api/v1/play/record",
            Method::POST,
            &[
                ("Authorization", AUTH),
                ("Content-Type", "application/json")
            ],
            Some(&raw)
        )
        .await
        .status,
        200
    );
    assert_eq!(origin.snapshot().writes[0].2, raw.as_bytes());
    let event = h.api("/progress-events", None).await[0].clone();
    assert_eq!(event["origin"], "client");
    assert_eq!(event["position"], 123.0);
    assert_eq!(event["duration"], 456.0);
    assert!(!event.to_string().contains("must-not-be-recorded"));
    assert!(event.get("source").is_none());
    case("client progress requests are forwarded unchanged and observed without private fields");

    h.get("/nohead.mp4", AUTH, Some("bytes=0-31")).await;
    let observations = h.api("/observations", None).await;
    let id = observations
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["path"] == "/nohead.mp4")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let probe = h
        .api(&format!("/observations/{id}/probe"), Some(json!({})))
        .await;
    assert_eq!(probe["total_bytes"], 8 * 1024 * 1024);
    assert_eq!(probe["head_status"], 501);
    assert!(probe["upstream_sample_bytes"].as_u64().unwrap() <= 4 * 1024 * 1024);
    assert_eq!(probe["repeat_matches"], true);
    assert!(
        probe["samples"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["local_read_matches"] == true)
    );
    use sha2::{Digest, Sha256};
    assert_eq!(
        probe["samples"][0]["sha256"],
        format!("{:x}", Sha256::digest(data(1024 * 1024, None)))
    );
    assert_eq!(h.api("/downloads", None).await, json!([]));
    case("bounded probe reads at most 4 MiB and leaves the full-download queue empty");

    origin.interrupt("/movie.mp4");
    let id = h.capture(TARGET, AUTH, false).await;
    h.wait_job(&id, "failed").await;
    assert!(fs::metadata(h.cache(&id, "part")).unwrap().len() > 0);
    h.command(&id, "resume").await;
    h.wait_job(&id, "complete").await;
    assert_eq!(
        Sha256::digest(fs::read(h.cache(&id, "bin")).unwrap()),
        Sha256::digest(data(8 * 1024 * 1024, None))
    );
    assert!(
        origin
            .snapshot()
            .calls
            .iter()
            .any(|c| c.path == "/movie.mp4"
                && c.range.as_deref().is_some_and(|r| r != "bytes=0-31")
                && c.if_range.as_deref() == Some("\"v1\""))
    );
    case("interrupted download resumes with Range/If-Range and matching SHA-256");

    origin.change(|s| s.block = true);
    let payload = data(8 * 1024 * 1024, None);
    let spans = [
        (Some("bytes=512-1023"), payload[512..1024].to_vec(), 206),
        (
            Some("bytes=-37"),
            payload[payload.len() - 37..].to_vec(),
            206,
        ),
        (
            Some("bytes=8388597-"),
            payload[payload.len() - 11..].to_vec(),
            206,
        ),
        (Some("bytes=8388608-"), vec![], 416),
        (None, payload.clone(), 200),
    ];
    for (range, expected, status) in spans {
        let before = origin
            .snapshot()
            .calls
            .iter()
            .filter(|c| c.method == "GET")
            .count();
        let reply = h.get(TARGET, AUTH, range).await;
        assert_eq!(reply.status, status);
        assert_eq!(reply.body, expected);
        assert_eq!(reply.header("x-fn-media-cache"), "HIT");
        assert_eq!(
            origin
                .snapshot()
                .calls
                .iter()
                .filter(|c| c.method == "GET")
                .count(),
            before
        );
    }
    let head = h
        .send(
            &h.proxy,
            TARGET,
            Method::HEAD,
            &[("Authorization", AUTH)],
            None,
        )
        .await;
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-length"), "8388608");
    assert!(head.body.is_empty());
    let reply = h
        .send(
            &h.proxy,
            TARGET,
            Method::GET,
            &[
                ("Authorization", AUTH),
                ("Range", "bytes=1-5"),
                ("If-Range", "\"old\""),
            ],
            None,
        )
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, payload);
    case(
        "cached full/HEAD/suffix/open/unsatisfiable ranges work with upstream video reads blocked",
    );

    let replies =
        futures_util::future::join_all((0..6).map(|_| h.get(TARGET, AUTH, Some("bytes=0-65535"))))
            .await;
    assert!(
        replies
            .iter()
            .all(|r| r.status == 206 && r.body == payload[..65536])
    );
    case("six concurrent local cache readers");
    assert_eq!(
        h.get(TARGET, "Bearer another-account", None).await.status,
        503
    );
    assert_eq!(
        h.get(
            &TARGET.replace("fixture-secret-query", "different-token"),
            AUTH,
            None
        )
        .await
        .status,
        503
    );
    origin.change(|s| s.revoke = true);
    assert_eq!(h.get(TARGET, AUTH, None).await.status, 401);
    origin.change(|s| {
        s.revoke = false;
        s.etag = "\"v2\"".into();
    });
    assert_eq!(h.get(TARGET, AUTH, None).await.status, 503);
    origin.change(|s| s.etag = "\"v1\"".into());
    case("account/query isolation, revoked access and changed ETag never serve stale cache");

    origin.change(|s| s.block = false);
    origin.interrupt("/ignore.mp4");
    let ignore = h.capture("/ignore.mp4", AUTH, false).await;
    h.wait_job(&ignore, "failed").await;
    origin.change(|s| {
        s.ignore_range.insert("/ignore.mp4".into());
    });
    h.command(&ignore, "resume").await;
    h.wait_job(&ignore, "complete").await;
    assert_eq!(fs::read(h.cache(&ignore, "bin")).unwrap(), payload);
    origin.change(|s| s.ignore_range.clear());
    case("server ignoring resume range safely restarts the partial file");

    origin.change(|s| {
        s.slow.insert("/restart.mp4".into());
    });
    let restart = h.capture("/restart.mp4", AUTH, false).await;
    until("first MiB", || async {
        h.job(&restart).await["downloaded"].as_u64().unwrap() >= 1024 * 1024
    })
    .await;
    h.command(&restart, "pause").await;
    h.wait_job(&restart, "paused").await;
    h.command(&restart, "resume").await;
    h.wait_job(&restart, "downloading").await;
    h.restart(true).await;
    h.wait_job(&restart, "complete").await;
    assert_eq!(fs::read(h.cache(&restart, "bin")).unwrap(), payload);
    origin.change(|s| s.slow.clear());
    case("pause/resume and abrupt process restart preserve partial download state");
    let invalid = h.capture("/novalidator.mp4", AUTH, false).await;
    assert!(
        h.wait_job(&invalid, "failed").await["error"]
            .as_str()
            .unwrap()
            .contains("validator")
    );
    let large = h.capture("/oversize.mp4", AUTH, false).await;
    assert!(
        h.wait_job(&large, "waiting_space").await["error"]
            .as_str()
            .unwrap()
            .contains("quota")
    );
    case(
        "unverifiable media fails and quota overflow waits for space without publishing a cache file",
    );

    assert!(h.stop(false).await.success());
    fs::rename(h.cache(&id, "bin"), h.cache(&id, "part")).unwrap();
    h.db()
        .execute(
            "UPDATE downloads SET state='downloading' WHERE id=?1",
            [&id],
        )
        .unwrap();
    origin.change(|s| s.block = true);
    h.start().await;
    h.wait_job(&id, "complete").await;
    assert_eq!(fs::read(h.cache(&id, "bin")).unwrap(), payload);
    case("crash after final partial-file write recovers without an invalid end-of-file Range");

    let other = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(binary())
            .args(["serve", "--config"])
            .arg(&h.config)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("second instance must exit")
    .unwrap();
    assert!(!other.status.success());
    assert!(String::from_utf8_lossy(&other.stderr).contains("another instance"));
    let raw = format!(
        "{}{}",
        h.api("/downloads", None).await,
        h.api("/observations", None).await
    );
    assert!(
        !raw.contains("fixture-secret-query") && !raw.contains(AUTH) && !raw.contains("\"source\"")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            h.root().join("data"),
            h.root().join("data/state.sqlite3"),
            h.root().join("data/admin-token"),
        ] {
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        }
    }
    case("single-instance lock, private local files, redacted management responses");
    h.stop(false).await;
    let log = fs::read_to_string(h.root().join("launcher.log")).unwrap();
    assert!(!log.contains(AUTH) && !log.contains("fixture-secret-query"));
    case("request logs contain neither credentials nor signed query strings");
}
