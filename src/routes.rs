//! Download-only routing. A source keeps its primary origin as its cache identity.
//! Alternate origins are explicitly configured as paths to that same server.
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::{
    config::Config,
    engine::Engine,
    fntv,
    protocol::{Source, hex},
};

#[derive(Debug)]
pub struct RouteFailure(pub String);

impl std::fmt::Display for RouteFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RouteFailure {}

pub fn transfer_error(message: impl Into<String>) -> anyhow::Error {
    RouteFailure(message.into()).into()
}

#[derive(Clone, Serialize)]
pub struct RouteResult {
    pub origin: String,
    pub status: &'static str,
    pub sample_bytes: u64,
    pub elapsed_ms: Option<u64>,
    pub mib_per_second: Option<f64>,
    pub error: Option<&'static str>,
}

#[derive(Clone, Serialize)]
pub struct RouteStatus {
    pub measuring: bool,
    pub measured_at_ms: Option<u64>,
    pub selected: Option<String>,
    pub results: Vec<RouteResult>,
}

#[derive(Clone)]
struct Reference {
    version: String,
    size: u64,
    checksum: String,
}

struct State {
    scope: Option<String>,
    measured: Option<Instant>,
    reference: Option<Reference>,
    status: RouteStatus,
}

pub struct Routes {
    origins: Vec<String>,
    gate: Arc<AsyncMutex<()>>,
    state: Mutex<State>,
}

pub struct DownloadRoute {
    pub origin: String,
    pub expected_version: Option<String>,
}

impl DownloadRoute {
    pub fn source(&self, logical: &Source) -> Source {
        Source {
            origin: self.origin.clone(),
            ..logical.clone()
        }
    }
}

impl Routes {
    pub fn new(config: &Config) -> Self {
        let origins = config.download_origins();
        let results = origins
            .iter()
            .map(|origin| RouteResult {
                origin: origin.clone(),
                status: "unmeasured",
                sample_bytes: 0,
                elapsed_ms: None,
                mib_per_second: None,
                error: None,
            })
            .collect();
        Self {
            origins,
            gate: Arc::new(AsyncMutex::new(())),
            state: Mutex::new(State {
                scope: None,
                measured: None,
                reference: None,
                status: RouteStatus {
                    measuring: false,
                    measured_at_ms: None,
                    selected: None,
                    results,
                },
            }),
        }
    }

    pub fn status(&self) -> RouteStatus {
        self.state.lock().unwrap().status.clone()
    }

    fn fresh(&self, source: &Source, ttl: u64) -> bool {
        let state = self.state.lock().unwrap();
        state.scope.as_deref() == Some(&source.cache_id())
            && state
                .measured
                .is_some_and(|time| time.elapsed() < Duration::from_secs(ttl))
    }

    fn candidates(&self, source: &Source) -> Vec<DownloadRoute> {
        let state = self.state.lock().unwrap();
        let mut ready: Vec<_> = state
            .status
            .results
            .iter()
            .filter(|r| r.status == "ready" && state.scope.as_deref() == Some(&source.cache_id()))
            .collect();
        ready.sort_by(|a, b| {
            b.mib_per_second
                .unwrap_or(0.0)
                .total_cmp(&a.mib_per_second.unwrap_or(0.0))
        });
        let mut routes: Vec<_> = ready
            .iter()
            .map(|r| DownloadRoute {
                origin: r.origin.clone(),
                expected_version: if r.origin == self.origins[0] {
                    None
                } else {
                    state.reference.as_ref().map(|r| r.version.clone())
                },
            })
            .collect();
        if !routes.iter().any(|r| r.origin == self.origins[0]) {
            routes.push(DownloadRoute {
                origin: self.origins[0].clone(),
                expected_version: None,
            });
        }
        routes
    }

    pub fn failed(&self, origin: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(route) = state.status.results.iter_mut().find(|r| r.origin == origin) {
            route.status = "failed";
            route.error = Some("download failed; another verified route will be tried");
        }
        state.status.selected = state
            .status
            .results
            .iter()
            .filter(|r| r.status == "ready")
            .max_by(|a, b| {
                a.mib_per_second
                    .unwrap_or(0.0)
                    .total_cmp(&b.mib_per_second.unwrap_or(0.0))
            })
            .map(|r| r.origin.clone());
    }
}

struct Measurement {
    reference: Reference,
    bytes: u64,
    elapsed: Duration,
}

impl Engine {
    pub fn warm_routes(self: &Arc<Self>, source: &Source) {
        if self.routes.origins.len() < 2
            || fntv::media_guid(source).is_none()
            || self
                .routes
                .fresh(source, self.policy.read().unwrap().route_result_ttl_seconds)
        {
            return;
        }
        // Never queue unbounded measurements behind playback requests.
        let Ok(permit) = self.routes.gate.clone().try_lock_owned() else {
            return;
        };
        let engine = self.clone();
        let source = source.clone();
        tokio::spawn(async move {
            if !engine.wait_background(None).await.unwrap_or(false) {
                return;
            }
            tokio::select! {
                _ = engine.shutdown.cancelled() => {},
                _ = engine.measure_routes(&source, permit) => {},
            }
        });
    }

    pub async fn refresh_routes(&self, source: &Source) -> Result<RouteStatus> {
        if fntv::media_guid(source).is_none() {
            bail!("route measurement requires a native FN single-file source");
        }
        let permit = self.routes.gate.clone().lock_owned().await;
        self.measure_routes(source, permit).await;
        Ok(self.routes.status())
    }

    pub async fn download_routes(&self, source: &Source) -> Vec<DownloadRoute> {
        if self.routes.origins.len() < 2 || fntv::media_guid(source).is_none() {
            return vec![DownloadRoute {
                origin: source.origin.clone(),
                expected_version: None,
            }];
        }
        let permit = self.routes.gate.clone().lock_owned().await;
        if !self
            .routes
            .fresh(source, self.policy.read().unwrap().route_result_ttl_seconds)
        {
            self.measure_routes(source, permit).await;
        }
        self.routes.candidates(source)
    }

    async fn measure_routes(&self, source: &Source, _permit: OwnedMutexGuard<()>) {
        let scope = source.cache_id();
        let mut reference = {
            let mut state = self.routes.state.lock().unwrap();
            state.status.measuring = true;
            if state.scope.as_deref() == Some(&scope) {
                state.reference.clone()
            } else {
                None
            }
        };
        let mut results = Vec::new();
        // Sequential probes avoid making the routes compete for this machine's bandwidth.
        for (index, origin) in self.routes.origins.iter().enumerate() {
            let routed = Source {
                origin: origin.clone(),
                ..source.clone()
            };
            let timeout = self.policy.read().unwrap().route_timeout_seconds;
            let measured =
                tokio::time::timeout(Duration::from_secs(timeout), self.measure_route(&routed))
                    .await;
            let mut result = RouteResult {
                origin: origin.clone(),
                status: "failed",
                sample_bytes: 0,
                elapsed_ms: None,
                mib_per_second: None,
                error: None,
            };
            match measured {
                Ok(Ok(measured)) => {
                    result.sample_bytes = measured.bytes;
                    result.elapsed_ms = Some(measured.elapsed.as_millis() as u64);
                    result.mib_per_second = Some(
                        measured.bytes as f64
                            / 1048576.0
                            / measured.elapsed.as_secs_f64().max(0.000001),
                    );
                    if index == 0 {
                        reference = Some(measured.reference.clone());
                    }
                    if reference.as_ref().is_some_and(|expected| {
                        expected.version == measured.reference.version
                            && expected.size == measured.reference.size
                            && expected.checksum == measured.reference.checksum
                    }) {
                        result.status = "ready";
                    } else {
                        result.status = "mismatch";
                        result.error = Some(
                            "file version or sample differs from primary, or primary is unverified",
                        );
                    }
                }
                Ok(Err(_)) => {
                    result.error = Some("metadata, authorization or range request failed")
                }
                Err(_) => result.error = Some("measurement timed out"),
            }
            results.push(result);
        }
        let selected = results
            .iter()
            .filter(|r| r.status == "ready")
            .max_by(|a, b| {
                a.mib_per_second
                    .unwrap_or(0.0)
                    .total_cmp(&b.mib_per_second.unwrap_or(0.0))
            })
            .map(|r| r.origin.clone());
        let status = RouteStatus {
            measuring: false,
            measured_at_ms: Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            ),
            selected,
            results,
        };
        *self.routes.state.lock().unwrap() = State {
            scope: Some(scope),
            measured: Some(Instant::now()),
            reference,
            status,
        };
    }

    async fn measure_route(&self, source: &Source) -> Result<Measurement> {
        let version = fntv::file_version(&self.client, source).await?;
        let sample_bytes = self.policy.read().unwrap().route_sample_bytes;
        let started = Instant::now();
        let sample = self
            .probe_range(
                source,
                0,
                (version.size - 1).min(sample_bytes - 1),
                Some(version.size),
            )
            .await?;
        let elapsed = started.elapsed();
        let after = fntv::file_version(&self.client, source).await?;
        if after.fingerprint != version.fingerprint {
            bail!("file version changed during route measurement");
        }
        Ok(Measurement {
            reference: Reference {
                version: version.fingerprint,
                size: version.size,
                checksum: hex(&Sha256::digest(&sample.bytes)),
            },
            bytes: sample.bytes.len() as u64,
            elapsed,
        })
    }
}
