use crate::support::*;
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_exits_cleanly() {
    let mut h = Harness::new(&["http://127.0.0.1:1".into()]).await;
    assert!(h.stop(false).await.success());
    case("SIGTERM produces a clean exit");
}
