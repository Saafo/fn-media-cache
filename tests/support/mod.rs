//! Black-box helpers: each test owns its origin, data directory and real service process.
pub mod origin;

pub use origin::{AUTH, Mode, Origin, TOKEN, data};
use reqwest::{Client, Method, header::HeaderMap};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    future::Future,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    time::{Duration, Instant},
};

pub struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}
impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("JSON response")
    }
    pub fn header(&self, key: &str) -> &str {
        self.headers
            .get(key)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

pub async fn until<F, Fut>(label: &str, mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if predicate().await {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn case(description: &str) {
    println!("PASS: {description}");
}

pub struct TestDir(PathBuf);
impl TestDir {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!("fn-rust-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("failed fixture retained at {}", self.0.display());
        } else {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

pub fn unused_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
pub fn binary() -> PathBuf {
    std::env::var_os("FN_MEDIA_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_fn-media-cache")))
}

pub struct Harness {
    pub dir: TestDir,
    pub config: PathBuf,
    pub proxy: String,
    pub admin: String,
    pub client: Client,
    child: Option<Child>,
}
impl Harness {
    pub async fn new(origins: &[String]) -> Self {
        Self::with_config(origins, "").await
    }
    pub async fn with_config(origins: &[String], extra: &str) -> Self {
        let dir = TestDir::new();
        let config = dir.path().join("config.toml");
        let proxy_port = unused_port();
        let mut admin_port = unused_port();
        while proxy_port == admin_port {
            admin_port = unused_port();
        }
        fs::write(&config, format!(
            "upstream={}\nupstream_alternates={}\nproxy_listen=\"127.0.0.1:{proxy_port}\"\nadmin_listen=\"127.0.0.1:{admin_port}\"\ndata_dir=\"data\"\n{extra}",
            serde_json::to_string(&origins[0]).unwrap(), serde_json::to_string(&origins[1..]).unwrap(),
        )).unwrap();
        let mut result = Self {
            dir,
            config,
            proxy: format!("http://127.0.0.1:{proxy_port}"),
            admin: format!("http://127.0.0.1:{admin_port}"),
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap(),
            child: None,
        };
        result.start().await;
        result
    }
    pub fn root(&self) -> &Path {
        self.dir.path()
    }
    pub fn cache(&self, id: &str, extension: &str) -> PathBuf {
        self.root().join(format!("data/cache/{id}.{extension}"))
    }
    pub fn db(&self) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(self.root().join("data/state.sqlite3")).unwrap();
        db.busy_timeout(Duration::from_secs(5)).unwrap();
        db
    }
    pub async fn start(&mut self) {
        assert!(self.child.is_none());
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root().join("launcher.log"))
            .unwrap();
        self.child = Some(
            Command::new(binary())
                .args(["serve", "--config"])
                .arg(&self.config)
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if self
                .client
                .get(format!("{}/health", self.admin))
                .timeout(Duration::from_millis(250))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!(
                    "service exited during startup: {status}\n{}",
                    fs::read_to_string(self.root().join("launcher.log")).unwrap_or_default()
                );
            }
            assert!(Instant::now() < deadline, "service startup timed out");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    pub async fn stop(&mut self, abrupt: bool) -> ExitStatus {
        let child = self.child.as_mut().expect("running service");
        if let Some(status) = child.try_wait().unwrap() {
            self.child.take();
            return status;
        }
        if abrupt {
            child.kill().unwrap();
        } else {
            #[cfg(unix)]
            {
                // SAFETY: this PID belongs to the still-owned, unreaped child.
                assert_eq!(
                    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
                    0
                );
            }
            #[cfg(not(unix))]
            child.kill().unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                self.child.take();
                return status;
            }
            // Keep ownership in self until reaped so Drop cleans up on every panic path.
            assert!(Instant::now() < deadline, "child did not stop");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    pub async fn restart(&mut self, abrupt: bool) {
        let status = self.stop(abrupt).await;
        if cfg!(unix) && !abrupt {
            assert!(status.success(), "unclean service exit: {status}");
        }
        self.start().await;
    }
    pub async fn send(
        &self,
        base: &str,
        path: &str,
        method: Method,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Reply {
        let mut request = self.client.request(method, format!("{base}{path}"));
        for (key, value) in headers {
            request = request.header(*key, *value);
        }
        if let Some(body) = body {
            request = request.body(body.to_owned());
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        Reply {
            status,
            headers,
            body: response.bytes().await.unwrap().to_vec(),
        }
    }
    pub async fn get(&self, path: &str, token: &str, range: Option<&str>) -> Reply {
        let mut headers = vec![("Authorization", token)];
        if let Some(range) = range {
            headers.push(("Range", range));
        }
        self.send(&self.proxy, path, Method::GET, &headers, None)
            .await
    }
    pub async fn api(&self, path: &str, body: Option<Value>) -> Value {
        let token = fs::read_to_string(self.root().join("data/admin-token")).unwrap();
        let auth = format!("Bearer {}", token.trim());
        let raw = body.as_ref().map(Value::to_string);
        let reply = self
            .send(
                &self.admin,
                &format!("/api{path}"),
                if body.is_some() {
                    Method::POST
                } else {
                    Method::GET
                },
                &[
                    ("Authorization", &auth),
                    ("Content-Type", "application/json"),
                ],
                raw.as_deref(),
            )
            .await;
        assert_eq!(
            reply.status,
            200,
            "{path}: {}",
            String::from_utf8_lossy(&reply.body)
        );
        reply.json()
    }
    pub async fn policy(&self, changes: Value) -> Value {
        let mut policy = self.api("/automation", None).await["policy"].clone();
        for (key, value) in changes.as_object().unwrap() {
            policy[key] = value.clone();
        }
        self.api("/automation/policy", Some(policy.clone())).await;
        policy
    }
    pub async fn job(&self, id: &str) -> Value {
        self.find_job(id).await.expect("download task exists")
    }
    pub async fn find_job(&self, id: &str) -> Option<Value> {
        self.api("/downloads", None)
            .await
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == id)
            .cloned()
    }
    pub async fn wait_job(&self, id: &str, state: &str) -> Value {
        until(&format!("{id} to become {state}"), || async {
            self.job(id).await["state"] == state
        })
        .await;
        self.job(id).await
    }
    pub async fn wait_job_absent(&self, id: &str) {
        until("deleted task to leave the downloads list", || async {
            self.find_job(id).await.is_none()
        })
        .await;
    }
    pub async fn command(&self, id: &str, command: &str) {
        self.api(&format!("/downloads/{id}/{command}"), Some(json!({})))
            .await;
    }
    pub async fn capture(&self, path: &str, token: &str, paused: bool) -> String {
        assert_eq!(self.get(path, token, Some("bytes=0-31")).await.status, 206);
        let observations = self.api("/observations", None).await;
        let name = path.split('?').next().unwrap();
        let id = observations
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["path"] == name)
            .unwrap()["id"]
            .as_str()
            .unwrap();
        self.api(
            "/downloads",
            Some(json!({"observation_id":id,"paused":paused})),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    pub async fn native(&self, guid: &str, token: &str, paused: bool) -> String {
        self.capture(&format!("/v/api/v1/media/range/{guid}"), token, paused)
            .await
    }
    pub async fn queue(&self, guids: &[&str]) -> Vec<String> {
        let reply = self
            .api("/library/download", Some(json!({"item_guids":guids})))
            .await;
        reply["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                assert_eq!(v["ok"], true, "{v}");
                v["id"].as_str().unwrap().to_owned()
            })
            .collect()
    }
    pub async fn managed(&self, guid: &str) -> Value {
        self.api("/automation", None).await["managed"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["item"]["guid"] == guid)
            .unwrap()
            .clone()
    }
    pub async fn retain(&self, guid: &str, change: Value) {
        let row = self.managed(guid).await;
        let mut body = json!({"session_id":row["session_id"],"item_guid":guid});
        for (key, value) in change.as_object().unwrap() {
            body[key] = value.clone();
        }
        self.api("/managed/retention", Some(body)).await;
    }
    pub async fn progress(&self, guid: &str) {
        self.progress_at(guid, 999_999).await;
    }
    pub async fn progress_at(&self, guid: &str, position: usize) {
        let body = json!({"item_guid":guid,"media_guid":origin::media(guid),"ts":position,"duration":1000000}).to_string();
        assert_eq!(
            self.send(
                &self.proxy,
                "/v/api/v1/play/record",
                Method::POST,
                &[
                    ("Authorization", TOKEN),
                    ("Content-Type", "application/json")
                ],
                Some(&body)
            )
            .await
            .status,
            200
        );
    }
    pub async fn backdate(&self, guid: &str, seconds: i64) {
        self.db()
            .execute(
                "UPDATE managed_media SET completed_at=unixepoch()-?2 WHERE item_guid=?1",
                rusqlite::params![guid, seconds],
            )
            .unwrap();
        self.policy(json!({})).await;
    }
    pub async fn login(&self, token: &str) {
        assert_eq!(
            self.get("/v/api/v1/session-context", token, None)
                .await
                .status,
            200
        );
        self.api("/sessions/refresh", Some(json!({}))).await;
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
