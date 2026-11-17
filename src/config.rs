use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::{Deserialize, Serialize};

pub fn configuration_path(root: &Path) -> PathBuf {
    let current = root.join("fn-media-cache.toml");
    let legacy = root.join("fn-media.toml");
    if !current.exists() && legacy.exists() {
        legacy
    } else {
        current
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub upstream: String,
    pub upstream_alternates: Vec<String>,
    pub proxy_listen: SocketAddr,
    pub admin_listen: SocketAddr,
    pub data_dir: PathBuf,
    pub max_cache_bytes: u64,
    pub read_timeout_seconds: u64,
    pub log_max_bytes: u64,
    pub log_keep_files: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            upstream: "https://media.example.com".into(),
            upstream_alternates: Vec::new(),
            proxy_listen: "127.0.0.1:28005".parse().unwrap(),
            admin_listen: "127.0.0.1:28006".parse().unwrap(),
            data_dir: "data".into(),
            max_cache_bytes: 20 * 1024 * 1024 * 1024,
            read_timeout_seconds: 30,
            log_max_bytes: 8 * 1024 * 1024,
            log_keep_files: 3,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let input = fs::read_to_string(path).context("cannot read configuration")?;
        // Do not echo TOML parse errors: configuration may contain private addresses.
        let mut config: Self =
            toml::from_str(&input).map_err(|_| anyhow::anyhow!("invalid configuration TOML"))?;
        config.validate()?;
        if config.data_dir.is_relative() {
            config.data_dir = path
                .parent()
                .unwrap_or(Path::new("."))
                .join(&config.data_dir);
        }
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.upstream_alternates.len() > 7 {
            bail!("at most seven alternate upstreams are supported");
        }
        for value in std::iter::once(&self.upstream).chain(&self.upstream_alternates) {
            let origin =
                Url::parse(value).map_err(|_| anyhow::anyhow!("invalid upstream origin"))?;
            if !matches!(origin.scheme(), "http" | "https")
                || origin.host_str().is_none()
                || !origin.username().is_empty()
                || origin.password().is_some()
                || origin.path() != "/"
                || origin.query().is_some()
                || origin.fragment().is_some()
            {
                bail!("upstream must be an HTTP(S) origin without a path or credentials");
            }
        }
        if !self.admin_listen.ip().is_loopback() {
            bail!("admin_listen must be a loopback address");
        }
        if self.proxy_listen == self.admin_listen {
            bail!("proxy and management listeners must use different addresses");
        }
        if self.max_cache_bytes == 0
            || self.max_cache_bytes > i64::MAX as u64
            || self.read_timeout_seconds == 0
        {
            bail!("cache size and timeout must be positive and in range");
        }
        if !(1024..=1024 * 1024 * 1024).contains(&self.log_max_bytes)
            || !(1..=20).contains(&self.log_keep_files)
        {
            bail!("invalid log size or retention");
        }
        Ok(())
    }

    pub fn download_origins(&self) -> Vec<String> {
        let mut origins = Vec::new();
        for value in std::iter::once(&self.upstream).chain(&self.upstream_alternates) {
            let origin = Url::parse(value)
                .expect("validated origin")
                .origin()
                .ascii_serialization();
            if !origins.contains(&origin) {
                origins.push(origin);
            }
        }
        origins
    }
}

pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn private_file(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
