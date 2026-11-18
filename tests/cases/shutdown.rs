use crate::support::*;
use std::{fs, process::Stdio, time::Duration};
use tokio::{io::AsyncBufReadExt, process::Command, time::timeout};

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_exits_cleanly() {
    let mut h = Harness::new(&["http://127.0.0.1:1".into()]).await;
    assert!(h.stop(false).await.success());
    case("SIGTERM produces a clean exit");
}

#[tokio::test]
async fn desktop_pipe_eof_exits_cleanly() {
    let directory = TestDir::new();
    let config = directory.path().join("config.toml");
    let proxy = unused_port();
    let mut admin = unused_port();
    while admin == proxy {
        admin = unused_port();
    }
    fs::write(
        &config,
        format!(
            "upstream = \"http://127.0.0.1:1\"\nproxy_listen = \"127.0.0.1:{proxy}\"\nadmin_listen = \"127.0.0.1:{admin}\"\n"
        ),
    )
    .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for _ in 0..2 {
        let mut child = Command::new(binary())
            .args(["serve", "--parent-pipe", "--config"])
            .arg(&config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let ready = timeout(Duration::from_secs(5), lines.next_line())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready.as_deref(), Some("ready"));
        assert!(
            client
                .get(format!("http://127.0.0.1:{admin}/health"))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        drop(child.stdin.take());
        let status = timeout(Duration::from_secs(7), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "unclean service exit: {status}");
    }
    case("closing the desktop pipe exits cleanly and releases ports and data for restart");
}
