#[path = "../../../src/config.rs"]
mod config;
mod controller;

use controller::{AdminResponse, Controller, Settings, Status};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
#[cfg(target_os = "macos")]
use tauri::menu::CheckMenuItem;
use tauri::{
    Manager, State,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};
#[cfg(target_os = "macos")]
use tokio::sync::watch;

#[tauri::command]
async fn service_status(state: State<'_, Arc<Controller>>) -> Result<Status, String> {
    Ok(state.status().await)
}
#[tauri::command]
async fn save_server(
    settings: Settings,
    state: State<'_, Arc<Controller>>,
) -> Result<Status, String> {
    state.save(settings).await.map_err(|e| e.to_string())
}
#[tauri::command]
async fn admin_request(
    path: String,
    body: Option<serde_json::Value>,
    state: State<'_, Arc<Controller>>,
) -> Result<AdminResponse, String> {
    state.admin(path, body).await.map_err(|e| e.to_string())
}
#[tauri::command]
fn open_logs(state: State<'_, Arc<Controller>>) -> Result<(), String> {
    state.open_logs().map_err(|e| e.to_string())
}

fn show(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg(target_os = "macos")]
async fn sync_sleep_menu(
    controller: &Controller,
    item: CheckMenuItem<tauri::Wry>,
    mut changes: watch::Receiver<Option<bool>>,
) {
    let mut ticks = tokio::time::interval(std::time::Duration::from_secs(2));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let change = tokio::select! {
            biased;
            result = changes.changed() => {
                if result.is_err() {
                    break;
                }
                *changes.borrow_and_update()
            }
            _ = ticks.tick() => None,
        };
        let enabled = match controller.prevent_idle_sleep(change).await {
            Ok(enabled) => Some(enabled),
            Err(_) if change.is_some() => controller.prevent_idle_sleep(None).await.ok(),
            Err(_) => None,
        };
        let _ = item.set_checked(enabled.unwrap_or(false));
        let _ = item.set_enabled(enabled.is_some());
    }
}

fn main() {
    let application = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            service_status,
            save_server,
            admin_request,
            open_logs
        ])
        .setup(|app| {
            let custom = if cfg!(debug_assertions) {
                std::env::var_os("FN_MEDIA_CACHE_HOME")
                    .or_else(|| std::env::var_os("FN_MEDIA_HOME"))
                    .map(PathBuf::from)
            } else {
                None
            };
            let development = custom.is_some();
            let root = custom.unwrap_or_else(|| {
                let support = PathBuf::from(std::env::var_os("HOME").expect("user directory"))
                    .join("Library/Application Support");
                let current = support.join("FN Media Cache");
                let legacy = support.join("FN Media");
                if !current.exists() && legacy.join("fn-media.toml").is_file() {
                    legacy
                } else {
                    current
                }
            });
            let daemon = if cfg!(dev) {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../bin/fn-media-cache-macos-arm64")
            } else {
                app.path().resolve(
                    "daemon/fn-media-cache",
                    tauri::path::BaseDirectory::Resource,
                )?
            };
            let controller = Arc::new(Controller::new(root, daemon, development)?);
            app.manage(Arc::clone(&controller));
            #[cfg(target_os = "macos")]
            let sleep_item = CheckMenuItem::with_id(
                app,
                "prevent-idle-sleep",
                "播放或下载时防止休眠",
                false,
                false,
                None::<&str>,
            )?;
            #[cfg(target_os = "macos")]
            let (change_sleep, sleep_changes) = watch::channel(None);
            let menu = Menu::with_items(
                app,
                &[
                    #[cfg(target_os = "macos")]
                    &sleep_item,
                    &MenuItem::with_id(app, "logs", "打开日志目录", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "show", "打开 FN Media Cache", true, None::<&str>)?,
                    &MenuItem::with_id(app, "quit", "退出 FN Media Cache", true, None::<&str>)?,
                ],
            )?;
            #[cfg(target_os = "macos")]
            let clicked_sleep_item = sleep_item.clone();
            let tray = TrayIconBuilder::new()
                .tooltip("FN Media Cache")
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "show" => show(app),
                    #[cfg(target_os = "macos")]
                    "prevent-idle-sleep" => {
                        if let Ok(enabled) = clicked_sleep_item.is_checked() {
                            change_sleep.send_replace(Some(enabled));
                        }
                    }
                    "logs" => {
                        let _ = app.state::<Arc<Controller>>().open_logs();
                    }
                    "quit" => app.exit(0),
                    _ => {}
                });
            #[cfg(target_os = "macos")]
            let tray = tray
                .icon(tauri::include_image!("icons/menu-bar-template@2x.png"))
                .icon_as_template(true);
            #[cfg(not(target_os = "macos"))]
            let tray = if let Some(icon) = app.default_window_icon() {
                tray.icon(icon.clone())
            } else {
                tray
            };
            tray.build(app)?;
            tauri::async_runtime::spawn(async move {
                controller.start_on_launch().await;
                #[cfg(target_os = "macos")]
                sync_sleep_menu(&controller, sleep_item, sleep_changes).await;
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("desktop application failed");
    let shutdown_complete = Arc::new(AtomicBool::new(false));
    application.run(move |app, event| match event {
        tauri::RunEvent::ExitRequested { api, code, .. }
            if !shutdown_complete.load(Ordering::Acquire) =>
        {
            api.prevent_exit();
            let controller = app.state::<Arc<Controller>>().inner().clone();
            if controller.begin_quit() {
                let app = app.clone();
                let shutdown_complete = Arc::clone(&shutdown_complete);
                tauri::async_runtime::spawn(async move {
                    controller.shutdown().await;
                    shutdown_complete.store(true, Ordering::Release);
                    app.exit(code.unwrap_or(0));
                });
            }
        }
        tauri::RunEvent::Exit if !shutdown_complete.load(Ordering::Acquire) => {
            // macOS's native Quit can go directly to the event loop's final exit event.
            let controller = app.state::<Arc<Controller>>();
            controller.begin_quit();
            tauri::async_runtime::block_on(controller.shutdown());
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { .. } => show(app),
        _ => {}
    });
}
