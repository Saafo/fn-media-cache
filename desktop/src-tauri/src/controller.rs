use crate::config::{Config, private_dir, private_file};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Child,
    sync::watch,
};

pub struct Controller {
    root: PathBuf,
    daemon: PathBuf,
    development: bool,
    child: tokio::sync::Mutex<Option<Child>>,
    quitting: watch::Sender<bool>,
    error: Mutex<Option<String>>,
    client: reqwest::Client,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Settings {
    pub upstream: String,
    pub upstream_alternates: Vec<String>,
    pub proxy_port: u16,
    pub lan_access: bool,
    pub log_max_megabytes: u64,
    pub log_keep_files: usize,
}

#[derive(Serialize)]
pub struct Status {
    pub configured: bool,
    pub running: bool,
    pub settings: Settings,
    pub proxy_address: String,
    pub admin_address: String,
    pub last_error: Option<String>,
}

#[derive(Serialize)]
pub struct AdminResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

fn digest(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("new");
    fs::write(&temp, bytes)?;
    private_file(&temp)?;
    fs::rename(temp, path)?;
    Ok(())
}

impl Controller {
    fn audit(&self, message: &str) {
        let config = self.config().ok();
        let directory = config
            .as_ref()
            .map(|c| c.data_dir.join("logs"))
            .unwrap_or_else(|| self.root.join("logs"));
        if private_dir(&directory).is_err() {
            return;
        }
        let maximum = config
            .as_ref()
            .map(|c| c.log_max_bytes)
            .unwrap_or(8 * 1048576);
        let path = directory.join("desktop.log");
        if fs::metadata(&path).is_ok_and(|m| m.len() > maximum) {
            let _ = fs::rename(&path, directory.join("desktop.log.1"));
        }
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = private_file(&path);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let _ = writeln!(file, "{timestamp} {message}");
        }
    }
    pub fn new(root: PathBuf, daemon: PathBuf, development: bool) -> Result<Self> {
        let root = std::path::absolute(root)?;
        private_dir(&root)?;
        Ok(Self {
            root,
            daemon,
            development,
            child: tokio::sync::Mutex::new(None),
            quitting: watch::Sender::new(false),
            error: Mutex::new(None),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }
    fn config_path(&self) -> PathBuf {
        crate::config::configuration_path(&self.root)
    }
    fn config(&self) -> Result<Config> {
        Config::load(&self.config_path())
    }
    pub async fn status(&self) -> Status {
        self.status_locked(&mut *self.child.lock().await)
    }
    fn status_locked(&self, child: &mut Option<Child>) -> Status {
        if let Some(process) = child
            && let Ok(Some(status)) = process.try_wait()
        {
            self.audit(&format!("service exited unexpectedly: {status}"));
            *self.error.lock().unwrap() = Some("后台已退出，请检查日志后重新打开应用。".into());
            *child = None;
        }
        let config = self.config().ok();
        let settings = config
            .as_ref()
            .map(|c| Settings {
                upstream: c.upstream.clone(),
                upstream_alternates: c.upstream_alternates.clone(),
                proxy_port: c.proxy_listen.port(),
                lan_access: !c.proxy_listen.ip().is_loopback(),
                log_max_megabytes: c.log_max_bytes / 1048576,
                log_keep_files: c.log_keep_files,
            })
            .unwrap_or(Settings {
                upstream: String::new(),
                upstream_alternates: vec![],
                proxy_port: 28005,
                lan_access: false,
                log_max_megabytes: 8,
                log_keep_files: 3,
            });
        Status {
            configured: config.is_some(),
            running: child.is_some(),
            proxy_address: config
                .as_ref()
                .map(|c| format!("http://{}", c.proxy_listen))
                .unwrap_or_default(),
            admin_address: config
                .as_ref()
                .map(|c| format!("http://{}", c.admin_listen))
                .unwrap_or_default(),
            settings,
            last_error: self.error.lock().unwrap().clone(),
        }
    }
    pub async fn save(&self, input: Settings) -> Result<Status> {
        let mut child = self.child.lock().await;
        if *self.quitting.borrow() {
            bail!("应用正在退出");
        }
        let result = self.save_locked(input, &mut child).await;
        if result.is_err() {
            *self.error.lock().unwrap() =
                Some("配置或后台启动失败，请检查地址、端口与日志。".into());
        }
        result
    }
    async fn save_locked(&self, input: Settings, child: &mut Option<Child>) -> Result<Status> {
        if input.proxy_port == 0 || input.proxy_port == 28006 {
            bail!("代理端口无效或与管理端口相同");
        }
        let mut config = self.config().unwrap_or_default();
        let old = config.upstream.clone();
        config.upstream = input.upstream.trim().trim_end_matches('/').to_owned();
        config.upstream_alternates = input
            .upstream_alternates
            .into_iter()
            .map(|s| s.trim().trim_end_matches('/').to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        config.proxy_listen = format!(
            "{}:{}",
            if input.lan_access {
                "0.0.0.0"
            } else {
                "127.0.0.1"
            },
            input.proxy_port
        )
        .parse()?;
        config.log_max_bytes = input
            .log_max_megabytes
            .checked_mul(1048576)
            .context("日志大小无效")?;
        config.log_keep_files = input.log_keep_files;
        config.validate()?;
        let origins = config.download_origins();
        config.upstream = origins[0].clone();
        config.upstream_alternates = origins[1..].to_vec();
        let profiles = self.root.join("data-profiles.json");
        let mut paths: BTreeMap<String, PathBuf> = fs::read(&profiles)
            .ok()
            .and_then(|s| serde_json::from_slice(&s).ok())
            .unwrap_or_default();
        if self.config_path().exists() {
            paths.entry(old.clone()).or_insert(config.data_dir.clone());
        }
        if config.upstream != old || !self.config_path().exists() {
            config.data_dir = paths.get(&config.upstream).cloned().unwrap_or_else(|| {
                self.root
                    .join("servers")
                    .join(&digest(config.upstream.as_bytes())[..16])
                    .join("data")
            });
        }
        paths.insert(config.upstream.clone(), config.data_dir.clone());
        self.stop_child(child).await?;
        write_private(&profiles, &serde_json::to_vec_pretty(&paths)?)?;
        write_private(
            &self.config_path(),
            toml::to_string_pretty(&config)?.as_bytes(),
        )?;
        self.start_child(child).await?;
        self.audit("configuration saved and service started");
        Ok(self.status_locked(child))
    }
    pub async fn start_on_launch(&self) {
        let mut child = self.child.lock().await;
        if !self.config_path().exists() || child.is_some() || *self.quitting.borrow() {
            return;
        }
        if let Err(error) = self.start_child(&mut child).await {
            self.audit(&format!("service startup failed: {error}"));
            *self.error.lock().unwrap() = Some(format!("后台启动失败：{error}"));
        }
    }
    async fn start_child(&self, child: &mut Option<Child>) -> Result<()> {
        let mut quitting = self.quitting.subscribe();
        if *quitting.borrow() {
            bail!("应用正在退出");
        }
        self.config()?;
        if !self.daemon.is_file() {
            bail!("安装包缺少后台程序");
        }
        #[cfg(target_os = "macos")]
        if !self.development {
            self.retire_launch_agent().await?;
        }
        let mut command = tokio::process::Command::new(&self.daemon);
        command
            .args(["serve", "--parent-pipe", "--config"])
            .arg(self.config_path())
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        let mut process = command.spawn().context("无法启动安装包内的后台程序")?;
        let mut stdout = BufReader::new(process.stdout.take().expect("child stdout pipe"));
        let mut line = String::new();
        let ready = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(8), stdout.read_line(&mut line)) => {
                result.is_ok_and(|read| read.is_ok()) && line.trim_end() == "ready"
            }
            _ = quitting.wait_for(|quitting| *quitting) => false,
        };
        *child = Some(process);
        if !ready {
            self.stop_child(child).await?;
            bail!("后台未能启动，请检查配置、端口占用与日志");
        }
        *self.error.lock().unwrap() = None;
        self.audit("service ready");
        Ok(())
    }

    pub fn begin_quit(&self) -> bool {
        !self.quitting.send_replace(true)
    }

    pub async fn shutdown(&self) {
        if let Err(error) = self.stop_child(&mut *self.child.lock().await).await {
            self.audit(&format!("service shutdown failed: {error}"));
        }
    }

    async fn stop_child(&self, child: &mut Option<Child>) -> Result<()> {
        let Some(mut process) = child.take() else {
            return Ok(());
        };
        drop(process.stdin.take());
        match tokio::time::timeout(Duration::from_secs(7), process.wait()).await {
            Ok(status) => self.audit(&format!("service stopped: {}", status?)),
            Err(_) => {
                process.kill().await?;
                self.audit("service exceeded shutdown deadline and was terminated");
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    async fn retire_launch_agent(&self) -> Result<()> {
        let label = if self.root.file_name().is_some_and(|name| name == "FN Media") {
            "local.fn-media"
        } else {
            "local.fn-media-cache"
        };
        let home = PathBuf::from(std::env::var_os("HOME").context("找不到用户目录")?);
        let plist = home
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"));
        if !plist.try_exists()? {
            return Ok(());
        }
        let output = tokio::process::Command::new("/usr/bin/id")
            .arg("-u")
            .output()
            .await?;
        let uid = String::from_utf8(output.stdout)?.trim().parse::<u32>()?;
        let service = format!("gui/{uid}/{label}");
        let launchctl = |action: &str| {
            let mut command = tokio::process::Command::new("/bin/launchctl");
            command
                .args([action, &service])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            command
        };
        if launchctl("print").status().await?.success()
            && !launchctl("bootout").status().await?.success()
        {
            bail!("无法停止旧版后台任务");
        }
        fs::rename(&plist, self.root.join("previous-launch-agent.plist"))
            .context("无法移出旧版后台的启动项")?;
        self.audit("legacy launch agent retired");
        Ok(())
    }

    pub async fn admin(
        &self,
        path: String,
        body: Option<serde_json::Value>,
    ) -> Result<AdminResponse> {
        let group = path.trim_start_matches('/').split('/').next().unwrap_or("");
        if !path.starts_with('/')
            || path.contains(['?', '#'])
            || path.len() > 1024
            || ![
                "stats",
                "observations",
                "proxy-requests",
                "downloads",
                "routes",
                "sessions",
                "library",
                "automation",
                "managed",
                "diagnostics",
            ]
            .contains(&group)
        {
            bail!("不支持的管理请求");
        }
        let config = self.config()?;
        let token = fs::read_to_string(config.data_dir.join("admin-token"))?;
        let url = format!("http://{}/api{path}", config.admin_listen);
        let request = if let Some(body) = body {
            self.client.post(url).json(&body)
        } else {
            self.client.get(url)
        };
        let response = request
            .bearer_auth(token.trim())
            .timeout(Duration::from_secs(180))
            .send()
            .await
            .context("后台连接失败")?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await?;
        let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
        });
        Ok(AdminResponse { status, body })
    }
    #[cfg(target_os = "macos")]
    pub async fn prevent_idle_sleep(&self, enabled: Option<bool>) -> Result<bool> {
        let child = self.child.lock().await;
        if child.is_none() || *self.quitting.borrow() {
            bail!("后台未运行");
        }
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            self.admin(
                "/automation/prevent-idle-sleep".into(),
                enabled.map(|enabled| serde_json::json!({ "enabled": enabled })),
            ),
        )
        .await??;
        if response.status != 200 {
            bail!("无法获取或保存防止休眠偏好");
        }
        response.body["enabled"]
            .as_bool()
            .context("后台未返回防止休眠状态")
    }

    pub fn open_logs(&self) -> Result<()> {
        let directory = self.config()?.data_dir.join("logs");
        private_dir(&directory)?;
        #[cfg(target_os = "macos")]
        {
            Command::new("/usr/bin/open").arg(directory).spawn()?;
        }
        Ok(())
    }
}
