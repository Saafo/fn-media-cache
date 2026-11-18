use crate::support::*;
use serde_json::json;
use std::{fs, process::Output, time::Duration};

async fn doctor(h: &Harness, arguments: &[&str]) -> Output {
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(binary())
            .arg("doctor")
            .args(arguments)
            .current_dir(h.root())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("doctor must finish within its timeout")
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_service_diagnostics() {
    let origin = Origin::new(Mode::Generic);
    let mut h = Harness::new(std::slice::from_ref(&origin.url)).await;
    let original = fs::read_to_string(&h.config).unwrap();
    let diagnostic = original
        .replace(&origin.url, "https://private.example.invalid")
        .replace("proxy_listen=\"127.0.0.1:", "proxy_listen=\"0.0.0.0:")
        .replace("data_dir=\"data\"", "data_dir=\"doctor-only-data\"");
    let config = h.root().join("diagnostic.toml");
    fs::write(&config, &diagnostic).unwrap();
    let token_file = h.root().join("data/admin-token");
    let token = fs::read_to_string(&token_file).unwrap();
    let held_token = token_file.with_extension("held");
    fs::rename(&token_file, &held_token).unwrap();
    let output = doctor(&h, &["--config", config.to_str().unwrap()]).await;
    fs::rename(&held_token, &token_file).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("PASS management: HTTP 200"));
    assert!(stdout.contains("PASS upstream_via_proxy: HTTP 401"));
    assert!(!stdout.contains(token.trim()) && !stdout.contains("private.example.invalid"));
    assert!(!h.root().join("doctor-only-data").exists());
    assert!(
        origin
            .snapshot()
            .calls
            .iter()
            .all(|call| call.method == "HEAD" && call.path == "/")
    );

    // HTTP errors mean the proxy is reachable; server errors do not pass diagnostics.
    for status in [200, 204, 302, 403, 404, 503] {
        origin.change(|s| s.diagnostic_status = Some(status));
        let output = doctor(&h, &["--config", config.to_str().unwrap()]).await;
        assert_eq!(output.status.success(), status < 500);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(&format!("upstream_via_proxy: HTTP {status}")));
    }
    origin.change(|s| s.diagnostic_status = Some(200));
    let health = Origin::new(Mode::Generic);
    health.change(|s| s.health_response = Some(json!({"ok":false,"secret":"not-for-output"})));
    let bad_admin = diagnostic.replace(&h.admin["http://".len()..], &health.url["http://".len()..]);
    let health_config = h.root().join("bad-health.toml");
    fs::write(&health_config, &bad_admin).unwrap();
    let output = doctor(&h, &["--config", health_config.to_str().unwrap()]).await;
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !output.status.success() && stdout.contains("FAIL management: invalid health response")
    );
    assert!(
        stdout.contains("PASS upstream_via_proxy: HTTP 200") && !stdout.contains("not-for-output")
    );
    health.change(|s| s.health_response = Some(json!({"ok":true,"padding":"x".repeat(20*1024)})));
    let output = doctor(&h, &["--config", health_config.to_str().unwrap()]).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("health response too large"));
    health.hold();
    health.change(|s| {
        s.health_response = Some(json!({"ok":true}));
        s.hold_health = true;
    });
    let output = doctor(
        &h,
        &[
            "--config",
            health_config.to_str().unwrap(),
            "--timeout-seconds",
            "1",
        ],
    )
    .await;
    health.release();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("request timed out"));

    fs::write(h.root().join("fn-media.toml"), &diagnostic).unwrap();
    assert!(doctor(&h, &[]).await.status.success());
    fs::write(h.root().join("fn-media-cache.toml"), &diagnostic).unwrap();
    fs::write(h.root().join("fn-media.toml"), "invalid TOML").unwrap();
    assert!(doctor(&h, &[]).await.status.success());
    let output = doctor(&h, &["--timeout-seconds", "0"]).await;
    assert!(!output.status.success());

    h.stop(false).await;
    let output = doctor(&h, &["--config", config.to_str().unwrap()]).await;
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!output.status.success());
    assert!(
        stdout.contains("FAIL management: connection failed")
            && stdout.contains("FAIL upstream_via_proxy: connection failed")
    );
}
