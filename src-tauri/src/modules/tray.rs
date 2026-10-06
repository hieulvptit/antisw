use crate::modules;
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    Emitter, Listener, Manager,
};

fn build_menu(app: &tauri::AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let config = modules::load_app_config().unwrap_or_default();
    let texts = modules::i18n::get_tray_texts(&config.language);
    let remote_terminal = MenuItem::with_id(
        app,
        "remote_terminal",
        "Remote Terminal",
        true,
        None::<&str>,
    )?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", &texts.quit, true, None::<&str>)?;
    Menu::with_items(app, &[&remote_terminal, &separator, &quit])
}

fn show_remote_terminal(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.emit("tray://open-remote-terminal", ());
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        #[cfg(target_os = "macos")]
        app.set_activation_policy(tauri::ActivationPolicy::Regular)
            .unwrap_or(());
    }
}

pub fn create_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let icon_bytes = include_bytes!("../../icons/tray-icon.png");
    let img = image::load_from_memory(icon_bytes)
        .map_err(|e| {
            tauri::Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            ))
        })?
        .to_rgba8();
    let (width, height) = img.dimensions();
    let icon = Image::new_owned(img.into_raw(), width, height);
    let menu = build_menu(app)?;

    let _ = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .icon(icon)
        .on_menu_event(move |app, event| {
            match event.id().as_ref() {
                "remote_terminal" => show_remote_terminal(app),
                "quit" => {
                    // 先停止 Admin Server，避免僵尸 socket
                    let state = app.state::<crate::commands::proxy::ProxyServiceState>();
                    let admin_server = state.admin_server.clone();
                    tauri::async_runtime::spawn(async move {
                        let mut lock = admin_server.write().await;
                        if let Some(admin) = lock.take() {
                            admin.axum_server.stop();
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    });
                    // 給一點時間讓 socket 關閉
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                ..
            } = event
            {
                show_remote_terminal(tray.app_handle());
            }
        })
        .build(app)?;

    let handle = app.clone();
    app.listen("config://updated", move |_event| {
        update_tray_menus(&handle);
    });
    Ok(())
}

/// Rebuild the Remote Terminal menu when the application language changes.
pub fn update_tray_menus(app: &tauri::AppHandle) {
    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(menu) = build_menu(app) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}
