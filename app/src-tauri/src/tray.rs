use std::sync::Mutex;

use tauri::menu::MenuItem;
use tauri::{
    AppHandle, Manager, Wry,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

const MENU_OPEN: &str = "tray-open";
const MENU_TOGGLE_INDEXING: &str = "tray-toggle-indexing";
const MENU_QUIT: &str = "tray-quit";

pub struct IndexingTrayItem(pub Mutex<MenuItem<Wry>>);

fn indexing_menu_label(enabled: bool) -> &'static str {
    if enabled {
        "검사 일시정지"
    } else {
        "검사 재개"
    }
}

pub fn sync_indexing_label(app: &AppHandle, enabled: bool) {
    if let Some(item) = app.try_state::<IndexingTrayItem>() {
        if let Ok(menu_item) = item.0.lock() {
            let _ = menu_item.set_text(indexing_menu_label(enabled));
        }
    }
}

fn toggle_indexing(app: &AppHandle) {
    use vidcull_ipc::{Action, Request, Response};
    let Some(conn) = app.try_state::<crate::DaemonConn>() else {
        return;
    };
    let current = match tauri::async_runtime::block_on(conn.request(&Request::GetSettings)) {
        Ok(Response::Settings(settings)) => settings,
        _ => return,
    };
    let mut next = current.clone();
    next.indexing_enabled = !current.indexing_enabled;
    let enabled = next.indexing_enabled;
    if let Ok(Response::Settings(_)) =
        tauri::async_runtime::block_on(conn.request(&Request::Action(Action::SetSettings(next))))
    {
        sync_indexing_label(app, enabled);
    }
}

pub fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItemBuilder::with_id(MENU_OPEN, "열기").build(app)?;
    let indexing =
        MenuItemBuilder::with_id(MENU_TOGGLE_INDEXING, indexing_menu_label(true)).build(app)?;
    let quit = MenuItemBuilder::with_id(MENU_QUIT, "종료").build(app)?;
    let menu = MenuBuilder::new(app)
        .items(&[&open, &indexing, &quit])
        .build()?;

    app.manage(IndexingTrayItem(Mutex::new(indexing)));

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".to_owned()))?;

    TrayIconBuilder::with_id("vidcull-tray")
        .tooltip("vidcull")
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            MENU_OPEN => show_main_window(app),
            MENU_TOGGLE_INDEXING => toggle_indexing(app),
            MENU_QUIT => quit_with_daemon(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

pub(crate) fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub(crate) fn shutdown_daemon(app: &AppHandle) {
    use vidcull_ipc::{Action, Request};
    let conn = app.state::<crate::DaemonConn>();
    let _ = tauri::async_runtime::block_on(conn.request(&Request::Action(Action::Shutdown)));
    #[cfg(windows)]
    {
        let spawned_pid = app.state::<crate::SpawnedDaemonPid>();
        if let Some(pid) = spawned_pid.get() {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        }
    }
}

fn quit_with_daemon(app: &AppHandle) {
    shutdown_daemon(app);
    app.exit(0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    Hide,
    Quit,
}

#[must_use]
pub fn on_close(background_enabled: bool) -> CloseAction {
    if background_enabled {
        CloseAction::Hide
    } else {
        CloseAction::Quit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_to_tray_when_background_running_is_enabled() {
        assert_eq!(on_close(true), CloseAction::Hide);
    }

    #[test]
    fn quits_when_background_running_is_disabled() {
        assert_eq!(on_close(false), CloseAction::Quit);
    }
}
