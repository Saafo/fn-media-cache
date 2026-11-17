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

pub struct Controller {
    pub root: PathBuf,
    daemon: PathBuf,
    label: String,
    gate: tokio::sync::Mutex<()>,
    error: Mutex<Option<String>>,
    client: reqwest::Client,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Settings {
    pub upstream: String,
    pub upstream_alternates: Vec<String>,
    pub proxy_port: u16,
    pub lan_access: bool,
    pub auto_start: bool,
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
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
        private_dir(&root)?;
        let label = if development {
            format!(
                "local.fn-media-cache.dev.{}",
                &digest(root.to_string_lossy().as_bytes())[..10]
            )
        } else if root.file_name().is_some_and(|name| name == "FN Media")
            && root.join("fn-media.toml").is_file()
        {
            // Adopt the existing job and data without creating a second service.
            "local.fn-media".into()
        } else {
            "local.fn-media-cache".into()
        };
        Ok(Self {
            root,
            daemon,
            label,
            gate: tokio::sync::Mutex::new(()),
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
    fn auto_start(&self) -> bool {
        fs::read(self.root.join("desktop-settings.json"))
            .ok()
            .and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok())
            .and_then(|v| v.get("auto_start").and_then(|v| v.as_bool()))
            .unwrap_or(true)
    }
    pub async fn status(&self) -> Status {
        let config = self.config().ok();
        let running = if let Some(config) = &config {
            self.client
                .get(format!("http://{}/health", config.admin_listen))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
        } else {
            false
        };
        let settings = config
            .as_ref()
            .map(|c| Settings {
                upstream: c.upstream.clone(),
                upstream_alternates: c.upstream_alternates.clone(),
                proxy_port: c.proxy_listen.port(),
                lan_access: !c.proxy_listen.ip().is_loopback(),
                auto_start: self.auto_start(),
                log_max_megabytes: c.log_max_bytes / 1048576,
                log_keep_files: c.log_keep_files,
            })
            .unwrap_or(Settings {
                upstream: String::new(),
                upstream_alternates: vec![],
                proxy_port: 28005,
                lan_access: false,
                auto_start: true,
                log_max_megabytes: 8,
                log_keep_files: 3,
            });
        Status {
            configured: config.is_some(),
            running,
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
        let _guard = self.gate.lock().await;
        let result = self.save_locked(input).await;
        if result.is_err() {
            *self.error.lock().unwrap() =
                Some("配置或后台启动失败，请检查地址、端口与日志。".into());
        }
        result
    }
    async fn save_locked(&self, input: Settings) -> Result<Status> {
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
        self.stop_locked()?;
        self.wait_stopped().await?;
        write_private(&profiles, &serde_json::to_vec_pretty(&paths)?)?;
        write_private(
            &self.config_path(),
            toml::to_string_pretty(&config)?.as_bytes(),
        )?;
        write_private(
            &self.root.join("desktop-settings.json"),
            &serde_json::to_vec(&serde_json::json!({"auto_start":input.auto_start}))?,
        )?;
        self.start_locked().await?;
        self.audit("configuration saved and service started");
        Ok(self.status().await)
    }
    pub async fn start(&self) -> Result<Status> {
        let _guard = self.gate.lock().await;
        self.audit("service start requested");
        if let Err(error) = self.start_locked().await {
            *self.error.lock().unwrap() = Some("后台启动失败，请检查配置和端口占用。".into());
            return Err(error);
        }
        Ok(self.status().await)
    }
    async fn start_locked(&self) -> Result<()> {
        self.config()?;
        private_dir(&self.root.join("bin"))?;
        private_dir(&self.root.join("logs"))?;
        let binary = self.root.join("bin/fn-media-cache");
        let bytes = fs::read(&self.daemon).context("安装包缺少后台程序")?;
        let changed = fs::read(&binary)
            .ok()
            .is_none_or(|old| digest(&old) != digest(&bytes));
        if self.status().await.running {
            if !changed {
                *self.error.lock().unwrap() = None;
                return Ok(());
            }
            self.stop_locked()?;
            self.wait_stopped().await?;
        }
        if changed {
            write_private(&binary, &bytes)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        }
        self.install_job(&binary)?;
        for _ in 0..60 {
            if self.status().await.running {
                *self.error.lock().unwrap() = None;
                self.audit("service ready");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        bail!("后台未能及时启动")
    }
    pub async fn stop(&self) -> Result<Status> {
        let _guard = self.gate.lock().await;
        self.stop_locked()?;
        self.wait_stopped().await?;
        self.audit("service stopped");
        Ok(self.status().await)
    }
    async fn wait_stopped(&self) -> Result<()> {
        for _ in 0..40 {
            if !self.status().await.running {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        bail!("服务端口仍被占用，未启动第二个进程")
    }
    #[cfg(target_os = "macos")]
    fn domain(&self) -> Result<String> {
        let output = Command::new("/usr/bin/id").arg("-u").output()?;
        let uid = String::from_utf8(output.stdout)?.trim().parse::<u32>()?;
        Ok(format!("gui/{uid}"))
    }
    #[cfg(target_os = "macos")]
    fn stop_locked(&self) -> Result<()> {
        let domain = self.domain()?;
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &format!("{domain}/{}", self.label)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    fn stop_locked(&self) -> Result<()> {
        bail!("本版本的后台托管目前支持 macOS")
    }

    #[cfg(target_os = "macos")]
    fn install_job(&self, binary: &Path) -> Result<()> {
        let domain = self.domain()?;
        let home = PathBuf::from(std::env::var_os("HOME").context("找不到用户目录")?);
        let directory = home.join("Library/LaunchAgents");
        fs::create_dir_all(&directory)?;
        let plist = directory.join(format!("{}.plist", self.label));
        let text = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array><string>{}</string><string>serve</string><string>--config</string><string>{}</string></array><key>WorkingDirectory</key><string>{}</string><key>RunAtLoad</key><{} /><key>KeepAlive</key><true/><key>ThrottleInterval</key><integer>10</integer><key>Umask</key><integer>63</integer><key>EnvironmentVariables</key><dict><key>FN_MEDIA_CACHE_MANAGED</key><string>1</string></dict><key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string></dict></plist>"#,
            xml(&self.label),
            xml(&binary.to_string_lossy()),
            xml(&self.config_path().to_string_lossy()),
            xml(&self.root.to_string_lossy()),
            if self.auto_start() { "true" } else { "false" },
            xml("/dev/null"),
            xml("/dev/null")
        );
        write_private(&plist, text.as_bytes())?;
        let loaded = Command::new("/bin/launchctl")
            .args(["print", &format!("{domain}/{}", self.label)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success();
        if !loaded
            && !Command::new("/bin/launchctl")
                .args(["bootstrap", &domain])
                .arg(&plist)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?
                .success()
        {
            bail!("无法安装用户级后台任务");
        }
        if !Command::new("/bin/launchctl")
            .args(["kickstart", &format!("{domain}/{}", self.label)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success()
        {
            bail!("无法启动用户级后台任务");
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    fn install_job(&self, _binary: &Path) -> Result<()> {
        bail!("本版本的后台托管目前支持 macOS")
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
