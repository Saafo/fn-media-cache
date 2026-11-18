//! Read-only checks against the configured local listeners; never load session credentials.
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Method, StatusCode};

use crate::config::Config;

fn local_address(mut address: SocketAddr) -> SocketAddr {
    if address.ip().is_unspecified() {
        address.set_ip(match address.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    address
}

struct Check {
    passed: bool,
    detail: String,
}

async fn check(client: &Client, address: SocketAddr, management: bool) -> Check {
    let method = if management {
        Method::GET
    } else {
        Method::HEAD
    };
    let path = if management { "/health" } else { "/" };
    let response = match client
        .request(method, format!("http://{}{path}", local_address(address)))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return Check {
                passed: false,
                detail: if error.is_timeout() {
                    "request timed out"
                } else if error.is_connect() {
                    "connection failed"
                } else {
                    "request failed"
                }
                .into(),
            };
        }
    };
    let status = response.status();
    if !management {
        return Check {
            passed: (200..500).contains(&status.as_u16()),
            detail: format!("HTTP {}", status.as_u16()),
        };
    }
    if status != StatusCode::OK {
        return Check {
            passed: false,
            detail: format!("HTTP {}", status.as_u16()),
        };
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return Check {
                passed: false,
                detail: "health response interrupted".into(),
            };
        };
        if body.len() + chunk.len() > 16 * 1024 {
            return Check {
                passed: false,
                detail: "health response too large".into(),
            };
        }
        body.extend_from_slice(&chunk);
    }
    let passed = serde_json::from_slice::<serde_json::Value>(&body)
        .is_ok_and(|value| value.get("ok").and_then(|v| v.as_bool()) == Some(true));
    Check {
        passed,
        detail: if passed {
            "HTTP 200"
        } else {
            "invalid health response"
        }
        .into(),
    }
}

pub async fn run(config: &Config, timeout: Duration) -> Result<()> {
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()?;
    let (management, proxy) = tokio::join!(
        check(&client, config.admin_listen, true),
        check(&client, config.proxy_listen, false),
    );
    for (label, result) in [("management", &management), ("upstream_via_proxy", &proxy)] {
        println!(
            "{} {label}: {}",
            if result.passed { "PASS" } else { "FAIL" },
            result.detail
        );
    }
    if !management.passed || !proxy.passed {
        bail!("service checks failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_listeners_are_checked_on_loopback() {
        for (listen, dial) in [
            ("0.0.0.0:12345", "127.0.0.1:12345"),
            ("[::]:12345", "[::1]:12345"),
            ("127.0.0.2:12345", "127.0.0.2:12345"),
        ] {
            assert_eq!(
                local_address(listen.parse().unwrap()),
                dial.parse::<SocketAddr>().unwrap()
            );
        }
    }
}
