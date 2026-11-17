#[path = "../../../src/config.rs"]
mod config;
mod controller;

use controller::{AdminResponse, Controller, Settings, Status};
use std::{path::PathBuf, sync::Arc};
use tauri::{
    Manager, State,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};

#[tauri::command]
async fn service_status(state: State<'_, Arc<Controller>>) -> Result<Status, String> {
    Ok(state.status().await)
}
#[tauri::command]
async fn service_start(state: State<'_, Arc<Controller>>) -> Result<Status, String> {
    state.start().await.map_err(|e| e.to_string())
}
#[tauri::command]
async fn service_stop(state: State<'_, Arc<Controller>>) -> Result<Status, String> {
    state.stop().await.map_err(|e| e.to_string())
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

fn main() {
    let application = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            service_status,
            service_start,
            service_stop,
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
            let packaged = app.path().resolve(
                "daemon/fn-media-cache",
                tauri::path::BaseDirectory::Resource,
            )?;
            let daemon = if packaged.exists() {
                packaged
            } else {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../bin/fn-media-cache-macos-arm64")
            };
            app.manage(Arc::new(Controller::new(root, daemon, development)?));
            let items = [
                MenuItem::with_id(app, "show", "打开 FN Media Cache", true, None::<&str>)?,
                MenuItem::with_id(app, "start", "启动后台服务", true, None::<&str>)?,
                MenuItem::with_id(app, "stop", "停止后台服务", true, None::<&str>)?,
                MenuItem::with_id(app, "logs", "打开日志目录", true, None::<&str>)?,
                MenuItem::with_id(app, "quit", "退出桌面（后台继续运行）", true, None::<&str>)?,
            ];
            let menu = Menu::with_items(
                app,
                &items
                    .iter()
                    .map(|i| i as &dyn tauri::menu::IsMenuItem<_>)
                    .collect::<Vec<_>>(),
            )?;
            let mut tray = TrayIconBuilder::new()
                .tooltip("FN Media Cache")
                .menu(&menu)
                .on_menu_event(|app, event| {
                    let controller = app.state::<Arc<Controller>>().inner().clone();
                    match event.id.as_ref() {
                        "show" => show(app),
                        "start" => {
                            tauri::async_runtime::spawn(async move {
                                let _ = controller.start().await;
                            });
                        }
                        "stop" => {
                            tauri::async_runtime::spawn(async move {
                                let _ = controller.stop().await;
                            });
                        }
                        "logs" => {
                            let _ = controller.open_logs();
                        }
                        "quit" => app.exit(0),
                        _ => {}
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
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
    application.run(|app, event| {
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            show(app);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (app, event);
    });
}
