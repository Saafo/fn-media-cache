//! Retry only transport/service failures. Authentication waits for a verified session.
use std::fmt;

#[derive(Debug)]
pub struct Retryable(pub &'static str);
#[derive(Debug)]
pub struct AuthRequired;
impl fmt::Display for Retryable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl fmt::Display for AuthRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("authentication expired or access denied; log in again")
    }
}
impl std::error::Error for Retryable {}
impl std::error::Error for AuthRequired {}

pub fn http_error(status: u16) -> anyhow::Error {
    if matches!(status, 401 | 403) {
        AuthRequired.into()
    } else if matches!(status, 408 | 429 | 500..=599) {
        Retryable("upstream temporarily unavailable").into()
    } else {
        anyhow::anyhow!("upstream returned HTTP {status}")
    }
}

pub fn delay(attempt: u32, base: u64, maximum: u64) -> u64 {
    base.saturating_mul(
        1u64.checked_shl(attempt.saturating_sub(1).min(63))
            .unwrap_or(u64::MAX),
    )
    .min(maximum)
}

pub fn resumed(elapsed: u64, wall_delta: i64, threshold: u64) -> bool {
    elapsed > threshold || wall_delta.unsigned_abs().abs_diff(elapsed) > threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_backoff_and_failure_types() {
        assert_eq!(
            (1..=5).map(|n| delay(n, 2, 10)).collect::<Vec<_>>(),
            vec![2, 4, 8, 10, 10]
        );
        assert_eq!(delay(u32::MAX, 2, 10), 10);
        assert!(http_error(401).is::<AuthRequired>());
        assert!(http_error(503).is::<Retryable>());
        assert!(!http_error(404).is::<Retryable>());
        assert!(!resumed(5, 5, 30));
        assert!(resumed(5, 3605, 30));
        assert!(resumed(3605, 3605, 30));
        assert!(resumed(5, -3600, 30));
    }
}
