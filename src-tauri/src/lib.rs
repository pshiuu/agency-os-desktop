use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tauri::menu::{
    AboutMetadataBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
};
use tauri::webview::DownloadEvent;
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

// A monotonic id so each preview/download window gets a unique label.
static DOC_WINDOW_SEQ: AtomicUsize = AtomicUsize::new(0);

// Frappe print/PDF endpoints the cockpit links to (proposal preview & PDF
// export). These live outside `/agency-os`, so without special handling they
// fall through to the system browser. We keep them in the app instead: the
// `/printview` preview renders in a child window, and the `download_pdf` export
// prompts for a save location and downloads there.
fn looks_like_pdf_download(url: &tauri::Url) -> bool {
    let path = url.path();
    path.contains("print_format.download_pdf") || path.ends_with(".pdf")
}

fn looks_like_print_preview(url: &tauri::Url) -> bool {
    url.path().starts_with("/printview")
}

fn is_document_url(url: &tauri::Url) -> bool {
    looks_like_pdf_download(url) || looks_like_print_preview(url)
}

// Embedded Agency OS video-call routes: the authenticated room (`/meet/<name>`)
// and the guest join page (`/join/<name>`), both under the SPA base `/agency-os`
// (see ROOM_PATH_PREFIX in agency_os/api/meet.py and the SPA router). A scheduled
// meeting's "Join" button is a `target="_blank"` link, so without this it would
// fall through to the system browser — the exact thing the desktop app exists to
// avoid. We keep the call inside the app, in its own window (below). External
// meeting_urls (Zoom/Meet/etc.) don't match and still open in the browser.
fn looks_like_meeting_room(url: &tauri::Url) -> bool {
    let path = url.path();
    path.starts_with("/agency-os/meet/") || path.starts_with("/agency-os/join/")
}

// Navigation rule shared by the document windows: stay within the server origin,
// and hand anything else (external links) to the system browser.
fn keep_in_origin(app: &tauri::AppHandle, host: &Option<String>, target: &tauri::Url) -> bool {
    if target.scheme() == "tauri" {
        return true;
    }
    match (host.as_deref(), target.host_str()) {
        (Some(h), Some(uh)) if h == uh => true,
        _ => {
            let _ = app
                .opener()
                .open_url(target.as_str().to_string(), None::<&str>);
            false
        }
    }
}

// Frappe names exported PDFs "<docname>.pdf"; the doc name rides on the `name`
// query param. Used to pre-fill the Save As dialog.
fn suggested_pdf_name(url: &tauri::Url) -> String {
    url.query_pairs()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.into_owned())
        .filter(|s| !s.is_empty())
        .map(|n| format!("{n}.pdf"))
        .unwrap_or_else(|| "document.pdf".to_string())
}

// Save locations chosen via the Save As dialog, consumed (FIFO) by the main
// window's download handler as each export arrives. `in_flight` carries the
// filename to the completion notification (macOS omits the saved path on the
// Finished event).
#[derive(Default)]
struct PendingDownloads {
    queue: Mutex<VecDeque<PathBuf>>,
    in_flight: Mutex<Option<String>>,
}

// Idempotent definition of a tiny "Preparing PDF…" toast with a spinner, shown
// in the main window while an export is being generated and downloaded. Prepended
// to the start/done scripts so it's always available regardless of page reloads.
const DL_INDICATOR_JS: &str = r#"
window.__agencyDl = window.__agencyDl || (function () {
  var n = 0, timer = null;
  function ensureToast() {
    if (!document.getElementById('agency-dl-style')) {
      var s = document.createElement('style');
      s.id = 'agency-dl-style';
      s.textContent =
        '@keyframes agencyDlSpin{to{transform:rotate(360deg)}}' +
        '#agency-dl-toast{position:fixed;right:20px;bottom:20px;z-index:2147483647;display:flex;align-items:center;gap:10px;padding:11px 15px;border-radius:10px;background:rgba(20,20,22,.92);color:#fff;font:13px/1.2 -apple-system,BlinkMacSystemFont,system-ui,sans-serif;box-shadow:0 8px 28px rgba(0,0,0,.32);opacity:0;transition:opacity .2s;pointer-events:none}' +
        '#agency-dl-toast .agency-dl-spin{width:14px;height:14px;border-radius:50%;border:2px solid rgba(255,255,255,.3);border-top-color:#fff;animation:agencyDlSpin .7s linear infinite}';
      (document.head || document.documentElement).appendChild(s);
    }
    var t = document.getElementById('agency-dl-toast');
    if (!t) {
      t = document.createElement('div');
      t.id = 'agency-dl-toast';
      t.innerHTML = '<span class="agency-dl-spin"></span><span class="agency-dl-label"></span>';
      (document.body || document.documentElement).appendChild(t);
    }
    return t;
  }
  function render() {
    var t = ensureToast();
    t.querySelector('.agency-dl-label').textContent =
      n > 1 ? ('Preparing ' + n + ' PDFs…') : 'Preparing PDF…';
    requestAnimationFrame(function () { t.style.opacity = '1'; });
  }
  function hide() {
    var t = document.getElementById('agency-dl-toast');
    if (!t) return;
    t.style.opacity = '0';
    setTimeout(function () { if (n <= 0 && t.parentNode) t.parentNode.removeChild(t); }, 250);
  }
  return {
    inc: function () {
      n++;
      render();
      if (timer) clearTimeout(timer);
      // Safety net: clear the spinner even if no finish event ever arrives.
      timer = setTimeout(function () { n = 0; hide(); }, 120000);
    },
    dec: function () {
      n = Math.max(0, n - 1);
      if (n <= 0) {
        if (timer) { clearTimeout(timer); timer = null; }
        hide();
      } else {
        render();
      }
    }
  };
})();
"#;

// Show the spinner, then force a real download in the main window. WKWebView
// happily *displays* PDFs, so wry only treats a response as a download when the
// navigation is flagged `shouldPerformDownload` — which an anchor carrying a
// `download` attribute sets, regardless of content type. Clicking it downloads
// without replacing the page.
fn download_start_js(url: &str) -> String {
    let url_lit = serde_json::to_string(url).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"{define}
try {{ window.__agencyDl.inc(); }} catch (e) {{}}
(function () {{
  try {{
    var a = document.createElement('a');
    a.href = {url};
    a.download = '';
    a.rel = 'noopener';
    a.style.display = 'none';
    document.body.appendChild(a);
    a.click();
    setTimeout(function () {{ a.remove(); }}, 0);
  }} catch (e) {{}}
}})();"#,
        define = DL_INDICATOR_JS,
        url = url_lit
    )
}

// Hide the spinner once a download finishes (fires on success and failure).
fn download_done_js() -> String {
    format!(
        "{define}\ntry {{ window.__agencyDl.dec(); }} catch (e) {{}}",
        define = DL_INDICATOR_JS
    )
}

// Route a print preview or PDF export into the app instead of the browser.
fn open_document_window(app: &tauri::AppHandle, url: tauri::Url) {
    if looks_like_pdf_download(&url) {
        prompt_and_download_pdf(app, url);
    } else {
        // Window creation is deferred to the main thread because this is invoked
        // from inside the WKWebView navigation callback.
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || build_preview_window(&handle, url));
    }
}

// Ask the user where to save the PDF, then download it there. The dialog is
// shown up front (not from the download callback, where a modal would deadlock
// the main thread); once a path is chosen we queue it and kick off a real
// download in the logged-in main window via `trigger_download_js`.
fn prompt_and_download_pdf(app: &tauri::AppHandle, url: tauri::Url) {
    let handle = app.clone();
    let mut dialog = app
        .dialog()
        .file()
        .set_title("Save PDF")
        .set_file_name(suggested_pdf_name(&url))
        .add_filter("PDF", &["pdf"]);
    if let Ok(dir) = app.path().download_dir() {
        dialog = dialog.set_directory(dir);
    }
    dialog.save_file(move |chosen| {
        let Some(target) = chosen.and_then(|p| p.into_path().ok()) else {
            return; // cancelled, or no real path
        };
        if let Some(state) = handle.try_state::<PendingDownloads>() {
            state.queue.lock().unwrap().push_back(target);
        }
        let app = handle.clone();
        let _ = handle.run_on_main_thread(move || {
            if let Some(main) = app.get_webview_window("main") {
                let _ = main.eval(download_start_js(url.as_str()));
            }
        });
    });
}

// A visible window that renders the formatted `/printview` HTML for reading and
// printing. The main cockpit window stays untouched behind it.
fn build_preview_window(app: &tauri::AppHandle, url: tauri::Url) {
    let host = url.host_str().map(|s| s.to_string());
    let fallback_url = url.as_str().to_string();
    let seq = DOC_WINDOW_SEQ.fetch_add(1, Ordering::Relaxed);
    let label = format!("doc-{seq}");

    let nav_app = app.clone();

    let built = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(url))
        .title("Preview")
        .inner_size(900.0, 1160.0)
        .min_inner_size(480.0, 480.0)
        .center()
        .on_navigation(move |u| keep_in_origin(&nav_app, &host, u))
        .build();

    if built.is_err() {
        let _ = app.opener().open_url(fallback_url, None::<&str>);
    }
}

// A stable window label per meeting, so clicking "Join" again focuses the same
// call window instead of stacking duplicates. Tauri labels only allow
// `[a-zA-Z0-9-/:_]`, so the meeting id (the last path segment) is sanitised.
fn meeting_window_label(url: &tauri::Url) -> String {
    let id: String = url
        .path()
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("call")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("call-{id}")
}

// Navigation rule for a call window. `_blank` links / `window.open` inside the
// room (e.g. a guest invite link the host shares) go to the system browser via
// the LINK_INTERCEPT_JS sentinel; same-origin pages stay in the window; anything
// else external opens in the browser. We never spawn another call window from
// here, so a room can't recurse into new windows.
fn meeting_window_nav(app: &tauri::AppHandle, host: &Option<String>, target: &tauri::Url) -> bool {
    if target.host_str() == Some("ext.agencyos.invalid") {
        if let Some(u) = target
            .query_pairs()
            .find(|(k, _)| k == "u")
            .map(|(_, v)| v.into_owned())
        {
            let _ = app.opener().open_url(u, None::<&str>);
        }
        return false;
    }
    keep_in_origin(app, host, target)
}

// Open an embedded meeting room in its own in-app window (full viewport, matching
// the SPA's own `/meet` route intent) so the cockpit window stays where it was.
// Cookies are shared across the app's windows, so the call is already
// authenticated — same as the print-preview window.
fn open_meeting_window(app: &tauri::AppHandle, url: tauri::Url) {
    // Window creation must run on the main thread (this is called from inside the
    // WKWebView navigation callback).
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || build_meeting_window(&handle, url));
}

fn build_meeting_window(app: &tauri::AppHandle, url: tauri::Url) {
    let label = meeting_window_label(&url);
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.set_focus();
        return;
    }

    let host = url.host_str().map(|s| s.to_string());
    let fallback_url = url.as_str().to_string();
    let nav_app = app.clone();

    let built = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(url))
        .title("Agency OS — Meeting")
        .inner_size(1200.0, 820.0)
        .min_inner_size(720.0, 520.0)
        .center()
        .initialization_script(LINK_INTERCEPT_JS)
        .on_navigation(move |u| meeting_window_nav(&nav_app, &host, u))
        .build();

    if built.is_err() {
        let _ = app.opener().open_url(fallback_url, None::<&str>);
    }
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
            // Holds Save As destinations until the matching download arrives.
            app.manage(PendingDownloads::default());

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
                    // Sentinel emitted by LINK_INTERCEPT_JS for `_blank` links and
                    // `window.open(...)`. A meeting room stays in the app (own call
                    // window); a print preview or PDF export stays in the app (own
                    // window / saved file); everything else opens in the system
                    // browser. Either way the cockpit doesn't navigate.
                    if url.host_str() == Some("ext.agencyos.invalid") {
                        if let Some(target) = url
                            .query_pairs()
                            .find(|(k, _)| k.as_ref() == "u")
                            .map(|(_, v)| v.into_owned())
                        {
                            match tauri::Url::parse(&target) {
                                Ok(parsed) if looks_like_meeting_room(&parsed) => {
                                    open_meeting_window(&handle, parsed);
                                }
                                Ok(parsed) if is_document_url(&parsed) => {
                                    open_document_window(&handle, parsed);
                                }
                                _ => {
                                    let _ = handle.opener().open_url(target, None::<&str>);
                                }
                            }
                        }
                        return false;
                    }
                    // Allow only the app's own pages (the SPA under /agency-os and
                    // the local connect screen). Print previews and PDF exports are
                    // kept inside the app; anything else — the ERPNext desk at /app,
                    // other sites — opens in the system browser.
                    if url.scheme() == "tauri" || url.path().starts_with("/agency-os") {
                        return true;
                    }
                    if is_document_url(url) {
                        open_document_window(&handle, url.clone());
                        return false;
                    }
                    let _ = handle
                        .opener()
                        .open_url(url.as_str().to_string(), None::<&str>);
                    false
                })
                // Save PDF exports to the location chosen in the Save As dialog
                // (see `prompt_and_download_pdf` / `trigger_download_js`); fall
                // back to Downloads for any download we didn't initiate.
                .on_download(|webview, event| match event {
                    DownloadEvent::Requested { url, destination } => {
                        let chosen = webview
                            .try_state::<PendingDownloads>()
                            .and_then(|s| s.queue.lock().unwrap().pop_front());
                        let target = chosen.unwrap_or_else(|| {
                            let name = suggested_pdf_name(&url);
                            match webview.path().download_dir() {
                                Ok(dir) => dir.join(name),
                                Err(_) => PathBuf::from(name),
                            }
                        });
                        if let Some(state) = webview.try_state::<PendingDownloads>() {
                            *state.in_flight.lock().unwrap() = target
                                .file_name()
                                .and_then(|n| n.to_str())
                                .map(|s| s.to_string());
                        }
                        *destination = target;
                        true
                    }
                    DownloadEvent::Finished { success, .. } => {
                        // Clear the "Preparing PDF…" spinner (fires on success or
                        // failure).
                        let _ = webview.eval(download_done_js());
                        if success {
                            let name = webview
                                .try_state::<PendingDownloads>()
                                .and_then(|s| s.in_flight.lock().unwrap().clone())
                                .unwrap_or_else(|| "PDF".to_string());
                            let _ = webview
                                .notification()
                                .builder()
                                .title("Download complete")
                                .body(format!("Saved {name}"))
                                .show();
                        }
                        true
                    }
                    _ => true,
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
