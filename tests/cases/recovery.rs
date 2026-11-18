use crate::support::*;
use reqwest::Method;
use serde_json::json;
use std::{fs, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_and_session_recovery() {
    let origin = Origin::new(Mode::Recovery);
    let mut h = Harness::with_config(
        std::slice::from_ref(&origin.url),
        "log_max_bytes=1024\nlog_keep_files=2\n",
    )
    .await;
    h.login("old-session").await;
    h.policy(json!({"bytes_per_second":0,"retry_base_seconds":1,"retry_max_seconds":3,"retry_max_attempts":3,"session_check_seconds":1,"scheduler_interval_seconds":1,"playback_cooldown_seconds":0})).await;
    origin.interrupt("resume");
    let id = h.native("resume", "old-session", false).await;
    let job = h.wait_job(&id, "retry_wait").await;
    assert_eq!(job["attempts"], 1);
    assert!(!job["retry_at"].is_null());
    h.restart(false).await;
    h.wait_job(&id, "complete").await;
    assert_eq!(
        fs::read(h.cache(&id, "bin")).unwrap(),
        data(1024 * 1024, None)
    );
    assert!(
        origin
            .snapshot()
            .calls
            .iter()
            .any(|c| c.path.ends_with("/resume") && c.range.as_deref() == Some("bytes=65536-"))
    );
    case(
        "interrupted body is retried automatically, with persisted retry timing and exact resume bytes after restart",
    );
    origin.change(|s| {
        s.blocked.insert("outage".into());
    });
    let outage = h.native("outage", "old-session", false).await;
    h.wait_job(&outage, "retry_wait").await;
    origin.change(|s| {
        s.blocked.remove("outage");
    });
    h.wait_job(&outage, "complete").await;
    case("temporary HTTP outage recovers without a manual resume");
    origin.change(|s| {
        s.permanent.insert("missing".into());
    });
    let missing = h.native("missing", "old-session", false).await;
    h.wait_job(&missing, "failed").await;
    let count = origin
        .snapshot()
        .calls
        .iter()
        .filter(|c| {
            c.path.ends_with("/missing") && c.range.as_deref().is_some_and(|r| r.ends_with('-'))
        })
        .count();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        origin
            .snapshot()
            .calls
            .iter()
            .filter(|c| c.path.ends_with("/missing")
                && c.range.as_deref().is_some_and(|r| r.ends_with('-')))
            .count(),
        count
    );
    case("permanent HTTP failures do not enter an endless retry loop");
    let auth = h.native("auth", "old-session", true).await;
    h.api(&format!("/downloads/{auth}/cache-samples"), Some(json!({})))
        .await;
    origin.change(|s| {
        s.revoked_tokens.insert("old-session".into());
    });
    h.command(&auth, "resume").await;
    h.wait_job(&auth, "waiting_login").await;
    origin.change(|s| {
        s.blocked.insert("auth".into());
    });
    h.login("other-account").await;
    assert_eq!(h.job(&auth).await["state"], "waiting_login");
    let path = "/v/api/v1/media/range/auth";
    assert_eq!(
        h.send(
            &h.proxy,
            path,
            Method::GET,
            &[
                ("Authorization", "other-account"),
                ("Range", "bytes=0-31"),
                ("X-Fn-Media-Cache-Only", "1")
            ],
            None
        )
        .await
        .status,
        504
    );
    h.login("renewed-session").await;
    until("same account recovery", || async {
        let state = h.job(&auth).await;
        ["retry_wait", "downloading"]
            .iter()
            .any(|s| state["state"] == *s)
    })
    .await;
    let reply = h
        .send(
            &h.proxy,
            path,
            Method::GET,
            &[
                ("Authorization", "renewed-session"),
                ("Range", "bytes=0-31"),
                ("X-Fn-Media-Cache-Only", "1"),
            ],
            None,
        )
        .await;
    assert_eq!(reply.status, 206);
    assert_eq!(reply.header("x-fn-media-cache"), "HIT");
    assert_eq!(reply.body, data(1024 * 1024, None)[..32]);
    origin.change(|s| {
        s.blocked.remove("auth");
    });
    h.wait_job(&auth, "complete").await;
    assert_eq!(h.api("/downloads", None).await.as_array().unwrap().len(), 4);
    case(
        "only a verified same-account renewal resumes old jobs and reuses old sample files; another account stays isolated",
    );
    for _ in 0..35 {
        h.get("/v/api/v1/session-context", "renewed-session", None)
            .await;
    }
    let logs: Vec<_> = fs::read_dir(h.root().join("data/logs"))
        .unwrap()
        .map(Result::unwrap)
        .filter(|p| p.file_name().to_string_lossy().starts_with("service.log"))
        .collect();
    assert!(logs.len() <= 3);
    let mut content = String::new();
    for path in logs {
        assert!(path.metadata().unwrap().len() <= 1024);
        content.push_str(&fs::read_to_string(path.path()).unwrap());
    }
    assert!(
        !["old-session", "renewed-session", "other-account"]
            .iter()
            .any(|token| content.contains(token))
    );
    case("operational logs rotate within configured limits and contain no session credentials");
}
