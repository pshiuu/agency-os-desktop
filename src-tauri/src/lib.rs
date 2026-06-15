use std::fs;
use std::path::PathBuf;
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;

// Open `target="_blank"` links and `window.open(...)` calls in the system browser
// instead of inside the app. The remote page can't call Rust directly, so the
// script funnels every external-open through a sentinel navigation that the Rust
// `on_navigation` handler below recognises, opens in the browser, and cancels —
// so the cockpit itself never navigates away.
const LINK_INTERCEPT_JS: &str = r#"
(function () {
  function ext(url) {
    try {
      var abs = new URL(url, location.href).href;
      window.location.href = "https://ext.agencyos.invalid/?u=" + encodeURIComponent(abs);
    } catch (e) {}
  }
  document.addEventListener(
    "click",
    function (e) {
      var a = e.target && e.target.closest ? e.target.closest("a") : null;
      if (a && a.target === "_blank" && a.href) {
        e.preventDefault();
        ext(a.href);
      }
    },
    true,
  );
  window.open = function (url) {
    if (url) ext(url);
    return null;
  };
})();
"#;

// The chosen server URL is persisted as a plain file in the app's config dir
// (~/Library/Application Support/<identifier>/server-url.txt on macOS).

fn server_file(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("server-url.txt"))
}

fn read_saved_url(app: &tauri::AppHandle) -> Option<String> {
    let path = server_file(app).ok()?;
    let value = fs::read_to_string(path).ok()?.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

// The cockpit entry point on any Agency OS / Frappe server. The SPA's router has
// no `/home` route — `/agency-os/` resolves to the Dashboard (or redirects to the
// login route when not authenticated), matching what the app uses after login.
fn app_target(base: &str) -> String {
    format!("{}/agency-os/", base.trim_end_matches('/'))
}

#[tauri::command]
fn get_server_url(app: tauri::AppHandle) -> Option<String> {
    read_saved_url(&app)
}

#[tauri::command]
fn set_server_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(dir.join("server-url.txt"), url.trim()).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn clear_server_url(app: tauri::AppHandle) -> Result<(), String> {
    if let Ok(path) = server_file(&app) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

// Point the main window at the server's cockpit. Driven from Rust because a JS
// `window.location` jump from the bundled `tauri://` page to an external
// `https://` origin does not reliably load in WKWebView (it leaves a blank page).
#[tauri::command]
fn open_server(app: tauri::AppHandle, base: String) -> Result<(), String> {
    let url = tauri::Url::parse(&app_target(&base)).map_err(|e| e.to_string())?;
    let win = app
        .get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())?;
    win.navigate(url).map_err(|e| e.to_string())?;
    Ok(())
}

// Check GitHub Releases for a newer version and install it.
//
// `interactive` = true  → triggered from the menu: report the outcome in a dialog
//                         and restart into the new version once installed.
// `interactive` = false → silent launch check: download + stage the update quietly;
//                         it takes effect the next time the app is opened.
fn check_for_updates(app: tauri::AppHandle, interactive: bool) {
    std::thread::spawn(move || {
        let updater = match app.updater() {
            Ok(u) => u,
            Err(e) => {
                if interactive {
                    app.dialog()
                        .message(format!("Updater unavailable:\n{e}"))
                        .title("Agency OS")
                        .blocking_show();
                }
                return;
            }
        };

        match tauri::async_runtime::block_on(updater.check()) {
            Ok(Some(update)) => {
                let proceed = if interactive {
                    app.dialog()
                        .message(format!(
                            "Agency OS {} is available.\nInstall it now and restart?",
                            update.version
                        ))
                        .title("Update available")
                        .buttons(MessageDialogButtons::OkCancelCustom(
                            "Install & Restart".to_string(),
                            "Later".to_string(),
                        ))
                        .blocking_show()
                } else {
                    true
                };

                if !proceed {
                    return;
                }

                match tauri::async_runtime::block_on(
                    update.download_and_install(|_, _| {}, || {}),
                ) {
                    Ok(_) if interactive => app.restart(),
                    Ok(_) => { /* staged silently; applies on next launch */ }
                    Err(e) => {
                        if interactive {
                            app.dialog()
                                .message(format!("Update failed:\n{e}"))
                                .title("Agency OS")
                                .blocking_show();
                        }
                    }
                }
            }
            Ok(None) => {
                if interactive {
                    app.dialog()
                        .message("You're on the latest version.")
                        .title("Agency OS")
                        .blocking_show();
                }
            }
            Err(e) => {
                if interactive {
                    app.dialog()
                        .message(format!("Couldn't check for updates:\n{e}"))
                        .title("Agency OS")
                        .blocking_show();
                }
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_server_url,
            set_server_url,
            clear_server_url,
            open_server
        ])
        .menu(|app| {
            let change_server =
                MenuItemBuilder::with_id("change-server", "Change Server…").build(app)?;
            let reload = MenuItemBuilder::with_id("reload", "Reload")
                .accelerator("CmdOrCtrl+R")
                .build(app)?;
            let devtools = MenuItemBuilder::with_id("devtools", "Toggle Developer Tools")
                .accelerator("CmdOrCtrl+Alt+I")
                .build(app)?;
            let check_updates =
                MenuItemBuilder::with_id("check-updates", "Check for Updates…").build(app)?;

            let app_menu = SubmenuBuilder::new(app, "Agency OS")
                .item(&change_server)
                .item(&check_updates)
                .separator()
                .hide()
                .hide_others()
                .show_all()
                .separator()
                .quit()
                .build()?;

            let edit_menu = SubmenuBuilder::new(app, "Edit")
                .undo()
                .redo()
                .separator()
                .cut()
                .copy()
                .paste()
                .select_all()
                .build()?;

            let view_menu = SubmenuBuilder::new(app, "View")
                .item(&reload)
                .item(&devtools)
                .separator()
                .fullscreen()
                .build()?;

            let window_menu = SubmenuBuilder::new(app, "Window")
                .minimize()
                .separator()
                .close_window()
                .build()?;

            MenuBuilder::new(app)
                .items(&[&app_menu, &edit_menu, &view_menu, &window_menu])
                .build()
        })
        .on_menu_event(|app, event| match event.id().0.as_str() {
            // Send the user back to the local connect screen, pre-filled so they
            // can point the app at a different server.
            "change-server" => {
                if let Some(win) = app.get_webview_window("main") {
                    if let Ok(url) = "tauri://localhost/index.html?change=1".parse() {
                        let _ = win.navigate(url);
                    }
                }
            }
            "reload" => {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.eval("window.location.reload()");
                }
            }
            "devtools" => {
                if let Some(win) = app.get_webview_window("main") {
                    if win.is_devtools_open() {
                        win.close_devtools();
                    } else {
                        win.open_devtools();
                    }
                }
            }
            "check-updates" => check_for_updates(app.clone(), true),
            _ => {}
        })
        .setup(|app| {
            // Saved server → load it directly as the window's initial content
            // (the reliable way to show remote content). Otherwise show the
            // local connect screen.
            let initial = match read_saved_url(app.handle()) {
                Some(base) => WebviewUrl::External(app_target(&base).parse()?),
                None => WebviewUrl::App("index.html".into()),
            };

            let handle = app.handle().clone();

            WebviewWindowBuilder::new(app, "main", initial)
                .title("Agency OS")
                .inner_size(1440.0, 900.0)
                .min_inner_size(800.0, 600.0)
                .center()
                .initialization_script(LINK_INTERCEPT_JS)
                .on_navigation(move |url| {
                    // Sentinel emitted by LINK_INTERCEPT_JS: open the wrapped URL
                    // in the system browser, and cancel the in-app navigation.
                    if url.host_str() == Some("ext.agencyos.invalid") {
                        if let Some(target) = url
                            .query_pairs()
                            .find(|(k, _)| k.as_ref() == "u")
                            .map(|(_, v)| v.into_owned())
                        {
                            let _ = handle.opener().open_url(target, None::<&str>);
                        }
                        return false;
                    }
                    // Allow only the app's own pages (the SPA under /agency-os and
                    // the local connect screen). Anything else — the ERPNext desk
                    // at /app, file downloads, other sites — opens in the browser.
                    if url.scheme() == "tauri" || url.path().starts_with("/agency-os") {
                        return true;
                    }
                    let _ = handle
                        .opener()
                        .open_url(url.as_str().to_string(), None::<&str>);
                    false
                })
                .build()?;

            // Silent background check on every launch; staged updates apply next open.
            check_for_updates(app.handle().clone(), false);

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
