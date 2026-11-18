//! Known viewing-history writes. Playback setup and media transfer must still work.
use axum::http::Method;

pub fn endpoint(path: &str) -> Option<&str> {
    let (version, endpoint) = path
        .trim_end_matches('/')
        .strip_prefix("/v/api/")?
        .split_once('/')?;
    let version = version.strip_prefix('v')?;
    (!version.is_empty() && version.bytes().all(|c| c.is_ascii_digit())).then_some(endpoint)
}

pub fn viewing_report(method: &Method, path: &str) -> bool {
    let Some(endpoint) = endpoint(path) else {
        return false;
    };
    let write = matches!(*method, Method::POST | Method::PUT | Method::PATCH);
    if matches!(endpoint, "play/record" | "item/watched") {
        return write || *method == Method::DELETE;
    }
    if let Some(item) = endpoint
        .strip_prefix("item/")
        .and_then(|rest| rest.strip_suffix("/playing"))
    {
        return write && crate::library::valid_guid(item);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn suppresses_viewing_writes_without_breaking_playback_setup() {
        assert!(viewing_report(&Method::POST, "/v/api/v1/play/record"));
        assert!(viewing_report(&Method::POST, "/v/api/v2/play/record/"));
        assert!(viewing_report(&Method::DELETE, "/v/api/v1/item/watched"));
        assert!(viewing_report(
            &Method::POST,
            "/v/api/v1/item/episode-one/playing"
        ));
        for path in [
            "/v/api/v1/play/info",
            "/v/api/v1/play/play",
            "/v/api/v1/stream",
            "/v/api/v1/item/favorite",
        ] {
            assert!(!viewing_report(&Method::POST, path));
        }
        assert!(!viewing_report(&Method::GET, "/v/api/v1/play/list"));
    }
}
