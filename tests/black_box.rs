#[path = "cases/automation.rs"]
mod automation;
#[path = "cases/doctor.rs"]
mod doctor;
#[path = "cases/general.rs"]
mod general;
#[path = "cases/native.rs"]
mod native;
#[path = "cases/playback.rs"]
mod playback;
#[path = "cases/privacy.rs"]
mod privacy;
#[path = "cases/recovery.rs"]
mod recovery;
#[path = "cases/routes.rs"]
mod routes;
#[cfg(unix)]
#[path = "cases/shutdown.rs"]
mod shutdown;
mod support;
