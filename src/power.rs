use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::engine::Engine;

pub const SUPPORTED: bool = cfg!(target_os = "macos");
// Observed playback reports arrive about every 15 seconds. Allow for a missed report.
const REPORT_GRACE: Duration = Duration::from_secs(45);

#[derive(Default)]
pub struct Activity {
    downloading: AtomicBool,
    playback: Mutex<PlaybackReports>,
}

#[derive(Default)]
struct PlaybackReports {
    last: HashMap<(String, String), (f64, Instant)>,
    last_advance: Option<Instant>,
}

impl Activity {
    pub fn download(&self) -> DownloadActivity<'_> {
        self.downloading.store(true, Ordering::Relaxed);
        DownloadActivity(&self.downloading)
    }

    pub fn report(&self, session: &str, item: &str, position: f64) {
        if !SUPPORTED || !position.is_finite() || position < 0.0 {
            return;
        }
        let mut playback = self.playback.lock().unwrap();
        let now = Instant::now();
        playback.last.retain(|_, (_, at)| now - *at < REPORT_GRACE);
        if playback.last.len() >= 100 {
            playback.last.clear();
        }
        let previous = playback
            .last
            .insert((session.to_owned(), item.to_owned()), (position, now));
        if previous.is_some_and(|(last, _)| position > last) {
            playback.last_advance = Some(now);
        }
    }

    pub fn clear_reports(&self) {
        *self.playback.lock().unwrap() = PlaybackReports::default();
    }

    fn recent_playback(&self) -> bool {
        self.playback
            .lock()
            .unwrap()
            .last_advance
            .is_some_and(|at| at.elapsed() < REPORT_GRACE)
    }
}

pub struct DownloadActivity<'a>(&'a AtomicBool);

impl Drop for DownloadActivity<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

pub async fn monitor(engine: Arc<Engine>) {
    if !SUPPORTED {
        return;
    }
    let mut assertion = None;
    let mut retry_at = Instant::now();
    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = engine.shutdown.cancelled() => break,
            _ = ticks.tick() => {}
        }
        let enabled = engine.policy.read().unwrap().prevent_idle_sleep;
        let needed = enabled
            && (engine.playing()
                || engine.power.downloading.load(Ordering::Relaxed)
                || engine.power.recent_playback());
        if !needed {
            drop(assertion.take());
            retry_at = Instant::now();
        } else if assertion.is_none() && Instant::now() >= retry_at {
            match platform::Assertion::acquire() {
                Ok(held) => assertion = Some(held),
                Err(code) => {
                    if !engine.policy.read().unwrap().incognito {
                        crate::event!("idle sleep assertion failed code={code}");
                    }
                    retry_at = Instant::now() + Duration::from_secs(30);
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{c_char, c_void};

    type CFString = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            text: *const c_char,
            encoding: u32,
        ) -> CFString;
        fn CFRelease(value: CFString);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: CFString,
            level: u32,
            name: CFString,
            assertion: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(assertion: u32) -> i32;
    }

    pub struct Assertion(u32);

    impl Assertion {
        pub fn acquire() -> Result<Self, i32> {
            const UTF8: u32 = 0x0800_0100;
            const ASSERTION_ON: u32 = 255;
            // Both C strings are static and each created CFString is released below.
            unsafe {
                let kind = CFStringCreateWithCString(
                    std::ptr::null(),
                    c"PreventUserIdleSystemSleep".as_ptr(),
                    UTF8,
                );
                let name = CFStringCreateWithCString(
                    std::ptr::null(),
                    c"FN Media Cache media transfer".as_ptr(),
                    UTF8,
                );
                let mut id = 0;
                let status = if kind.is_null() || name.is_null() {
                    -1
                } else {
                    IOPMAssertionCreateWithName(kind, ASSERTION_ON, name, &mut id)
                };
                for value in [kind, name].into_iter().filter(|value| !value.is_null()) {
                    CFRelease(value);
                }
                if status == 0 {
                    Ok(Self(id))
                } else {
                    Err(status)
                }
            }
        }
    }

    impl Drop for Assertion {
        fn drop(&mut self) {
            // A successfully created assertion is owned by this guard.
            let _ = unsafe { IOPMAssertionRelease(self.0) };
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub struct Assertion;

    impl Assertion {
        pub fn acquire() -> Result<Self, i32> {
            Err(-1)
        }
    }
}
