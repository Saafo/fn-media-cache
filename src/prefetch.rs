use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant},
};

use crate::scheduler::Policy;

// The player reports about every 15 seconds. A longer silence starts a new watch.
const MAX_REPORT_GAP: Duration = Duration::from_secs(45);
// Reports round the media position and can arrive with some network jitter.
const POSITION_TOLERANCE_SECONDS: f64 = 2.0;
const MAX_CLIENTS: usize = 100;

#[derive(Default)]
pub struct WatchTime {
    watches: HashMap<(String, IpAddr), Watch>,
}

struct Watch {
    item: String,
    position: f64,
    reported_at: Instant,
    last_advance: Instant,
    seconds: f64,
}

impl WatchTime {
    pub fn report(&mut self, session: &str, peer: IpAddr, item: &str, position: f64, at: Instant) {
        if !position.is_finite() || position < 0.0 {
            return;
        }
        self.watches
            .retain(|_, watch| at.saturating_duration_since(watch.last_advance) <= MAX_REPORT_GAP);
        let key = (session.to_owned(), peer);
        if let Some(watch) = self.watches.get_mut(&key) {
            if at <= watch.reported_at {
                return;
            }
            if watch.item == item {
                let elapsed = (at - watch.reported_at).as_secs_f64();
                let advance = position - watch.position;
                if advance > 0.0 && advance <= elapsed + POSITION_TOLERANCE_SECONDS {
                    watch.seconds += elapsed.min(advance);
                    watch.last_advance = at;
                }
                // A seek or a paused report supplies a new baseline without adding watch time.
                watch.position = position;
                watch.reported_at = at;
                return;
            }
        } else if self.watches.len() >= MAX_CLIENTS
            && let Some(oldest) = self
                .watches
                .iter()
                .min_by_key(|(_, watch)| watch.reported_at)
                .map(|(key, _)| key.clone())
        {
            self.watches.remove(&oldest);
        }
        self.watches.insert(
            key,
            Watch {
                item: item.to_owned(),
                position,
                reported_at: at,
                last_advance: at,
                seconds: 0.0,
            },
        );
    }

    pub fn eligible(&self, session: &str, item: &str, policy: &Policy, now: Instant) -> bool {
        policy.auto_prefetch
            && !policy.incognito
            && (policy.prefetch_after_seconds == 0
                || self.watches.iter().any(|((id, _), watch)| {
                    id == session
                        && watch.item == item
                        && watch.seconds >= policy.prefetch_after_seconds as f64
                        && now.saturating_duration_since(watch.last_advance) <= MAX_REPORT_GAP
                }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn resumed_position_needs_sixty_seconds_of_new_playback() {
        let policy = Policy {
            auto_prefetch: true,
            ..Policy::default()
        };
        let mut watch = WatchTime::default();
        let start = Instant::now();
        let peer = Ipv4Addr::LOCALHOST.into();
        for elapsed in [0, 15, 30, 45, 60] {
            let at = start + Duration::from_secs(elapsed);
            watch.report("session", peer, "episode", 731.0 + elapsed as f64, at);
            assert_eq!(
                watch.eligible("session", "episode", &policy, at),
                elapsed >= 60
            );
        }
        assert!(!watch.eligible(
            "session",
            "episode",
            &policy,
            start + Duration::from_secs(106)
        ));
    }

    #[test]
    fn pauses_seeks_and_other_devices_do_not_add_time() {
        let policy = Policy {
            auto_prefetch: true,
            prefetch_after_seconds: 30,
            ..Policy::default()
        };
        let mut watch = WatchTime::default();
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let a = Ipv4Addr::new(192, 0, 2, 1).into();
        let b = Ipv4Addr::new(192, 0, 2, 2).into();
        watch.report("session", a, "one", 40.0, at(0));
        watch.report("session", a, "one", 800.0, at(1));
        watch.report("session", a, "one", 800.0, at(16));
        watch.report("session", b, "one", 30.0, at(16));
        watch.report("session", a, "one", 815.0, at(31));
        watch.report("session", b, "one", 45.0, at(31));
        watch.report("session", a, "one", 800.0, at(25)); // late response for an earlier request
        assert!(!watch.eligible("session", "one", &policy, at(31)));
        watch.report("session", a, "one", 830.0, at(46));
        assert!(watch.eligible("session", "one", &policy, at(46)));
        watch.report("session", a, "two", 14.0, at(47));
        assert!(!watch.eligible("session", "one", &policy, at(47)));
        assert!(!watch.eligible("session", "two", &policy, at(47)));
    }
}
