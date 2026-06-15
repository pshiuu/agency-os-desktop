use std::fs;
use std::path::PathBuf;
use tauri::menu::{
    AboutMetadataBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;

// One native notification, as emitted by NOTIFY_JS (title `t`, body `b`).
#[derive(serde::Deserialize)]
struct NotifyItem {
    t: String,
    b: String,
}

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

// Post a native macOS notification when a new unread Agency OS notification
// arrives. We wrap fetch and watch the `notifications.get_notifications` response
// (refetched whenever the realtime `notification` socket event fires). New unread
// items are funnelled to Rust through a sentinel navigation (same trick as the
// link handler) which shows them in Notification Center. No SPA changes needed.
const NOTIFY_JS: &str = r#"
(function () {
  var seen = new Set();
  var primed = false;

  function strip(html) {
    var d = document.createElement("div");
    d.innerHTML = html || "";
    return (d.textContent || d.innerText || "").replace(/\s+/g, " ").trim();
  }

  function handle(json) {
    var list =
      (json && json.message && json.message.notifications) ||
      (json && json.notifications);
    if (!Array.isArray(list)) return;

    var fresh = [];
    for (var i = 0; i < list.length; i++) {
      var n = list[i];
      if (!n || !n.name) continue;
      var isNew = !seen.has(n.name);
      seen.add(n.name);
      if (primed && isNew && !n.read) {
        var body = strip(n.subject);
        if (body) fresh.push({ t: "Agency OS", b: body });
      }
    }
    // First response just records the backlog so we don't notify old items.
    if (!primed) {
      primed = true;
      return;
    }
    if (fresh.length) {
      try {
        var payload = encodeURIComponent(JSON.stringify(fresh.slice(0, 5)));
        window.location.href = "https://notify.agencyos.invalid/?n=" + payload;
      } catch (e) {}
    }
  }

  var origFetch = window.fetch;
  window.fetch = function () {
    var args = arguments;
    return origFetch.apply(this, args).then(function (res) {
      try {
        var url = (args[0] && args[0].url) || args[0];
        if (
          typeof url === "string" &&
          url.indexOf("notifications.get_notifications") !== -1
        ) {
          res
            .clone()
            .json()
            .then(handle)
            .catch(function () {});
        }
      } catch (e) {}
      return res;
    });
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

// First-launch welcome notification. macOS has no up-front permission API we can
// call (the plugin's desktop request_permission is a no-op), so posting one real
// notification right after launch is how we get macOS to ask/register up front —
// and it doubles as confirmation that notifications work.
fn welcome_if_first_launch(app: &tauri::AppHandle) {
    let Ok(dir) = app.path().app_config_dir() else {
        return;
    };
    let flag = dir.join("notif-welcomed");
    if flag.exists() {
        return;
    }
    let _ = fs::create_dir_all(&dir);
    let _ = app
        .notification()
        .builder()
        .title("Agency OS")
        .body("Notifications are on — you'll be alerted about new activity here.")
        .show();
    let _ = fs::write(flag, "1");
}

// Background watcher: post the first-launch welcome, then check GitHub for a newer
// version on launch and every 6 hours. Each new version is downloaded/staged
// (applies on next launch) and announced once via a native notification, so even
// long-running installs don't miss an update.
fn spawn_update_watcher(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        // Let the window settle before the first notification / permission prompt.
        std::thread::sleep(std::time::Duration::from_secs(2));
        welcome_if_first_launch(&app);

        let mut announced: Option<String> = None;
        loop {
            if let Ok(updater) = app.updater() {
                if let Ok(Some(update)) = tauri::async_runtime::block_on(updater.check()) {
                    let version = update.version.clone();
                    let staged = tauri::async_runtime::block_on(
                        update.download_and_install(|_, _| {}, || {}),
                    )
                    .is_ok();
                    if staged && announced.as_deref() != Some(version.as_str()) {
                        let _ = app
                            .notification()
                            .builder()
                            .title("Agency OS update ready")
                            .body(format!(
                                "Version {version} is downloaded — quit and reopen Agency OS to update."
                            ))
                            .show();
                        announced = Some(version);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(6 * 60 * 60));
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
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
            let about = PredefinedMenuItem::about(
                app,
                Some("About Agency OS"),
                Some(
                    AboutMetadataBuilder::new()
                        .name(Some("Agency OS"))
                        .version(Some(app.package_info().version.to_string()))
                        .build(),
                ),
            )?;

            let app_menu = SubmenuBuilder::new(app, "Agency OS")
                .item(&about)
                .separator()
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
                .initialization_script(NOTIFY_JS)
                .on_navigation(move |url| {
                    // Sentinel emitted by NOTIFY_JS: post the new notifications to
                    // macOS Notification Center, and cancel the in-app navigation.
                    if url.host_str() == Some("notify.agencyos.invalid") {
                        if let Some(raw) = url
                            .query_pairs()
                            .find(|(k, _)| k.as_ref() == "n")
                            .map(|(_, v)| v.into_owned())
                        {
                            if let Ok(items) = serde_json::from_str::<Vec<NotifyItem>>(&raw) {
                                for item in items {
                                    let _ = handle
                                        .notification()
                                        .builder()
                                        .title(item.t)
                                        .body(item.b)
                                        .show();
                                }
                            }
                        }
                        return false;
                    }
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

            // Welcome notification + background update watcher (checks on launch
            // and every 6h, announcing staged updates so they aren't missed).
            spawn_update_watcher(app.handle().clone());

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
