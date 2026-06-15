# Agency OS — macOS desktop app

A [Tauri v2](https://tauri.app) desktop client for [Agency OS](https://github.com/pshiuu/agency_os).
On first launch it shows a small **"Connect to your server"** screen; the user
enters their Frappe server URL (e.g. `https://agencyos.yourcompany.com`) and the
app loads that server's cockpit (`<url>/agency-os/`) in a native window.

Because it loads the real server directly, login/cookies/CSRF behave exactly like
a browser, and the app always reflects whatever is deployed — nothing to re-ship
when the server-side frontend changes. The chosen URL is remembered between
launches; **Agency OS → Change Server…** in the menu bar switches or resets it.

The app has **no code dependency on the Frappe backend** — it just loads a URL —
so it lives in its own repo and ships its own releases. Anyone can install it and
point it at their own Agency OS server; end users never need build tools.

## How it works

- `src/` — the local "connect" screen (plain HTML/CSS/JS, bundled into the app).
- `src-tauri/src/lib.rs` — the Rust shell:
  - persists the server URL to `~/Library/Application Support/com.conradiusdesign.agencyos/server-url.txt`
    (`get/set/clear_server_url` + `open_server` commands);
  - builds the window in `setup` — loading the saved server (`WebviewUrl::External`)
    or the connect screen (`WebviewUrl::App`). The entry path is `/agency-os/`
    (the SPA has no `/home` route);
  - **external links open in the system browser** — `LINK_INTERCEPT_JS` catches
    `target="_blank"` / `window.open(...)`, and `on_navigation` sends anything that
    isn't an in-app `/agency-os` page (the ERPNext desk `/app/...`, file/PDF
    downloads, websites) to the browser;
  - **native macOS notifications** — `NOTIFY_JS` watches the SPA's
    `notifications.get_notifications` responses (refetched on the realtime
    `notification` socket event) and posts new unread items to Notification
    Center via the sentinel → `tauri-plugin-notification`. No SPA changes needed;
    the first notification may prompt for permission.
  - **auto-updates** from GitHub Releases (see below).

## Build a `.app` + `.dmg` locally (for dev/testing)

Prerequisites (one-time): Node + pnpm, Xcode Command Line Tools
(`xcode-select --install`), and Rust
(`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`).

```bash
pnpm install            # first time only
pnpm tauri build        # → src-tauri/target/release/bundle/{macos,dmg}/
pnpm tauri dev          # live window, no packaging
```

> Local builds are **unsigned** and have **no updater signature**, so they're for
> testing only. Real releases are built + signed by CI (below).

## Releases & auto-update

Updates are served from **GitHub Releases** and verified with a signing key
(public key baked into `tauri.conf.json`; private key in repo secrets).

- A background watcher checks **on launch and every 6 hours**. A newer version is
  downloaded/staged (applies on next open) and announced **once** via a native
  notification, so updates aren't missed.
- **Agency OS → Check for Updates…** checks on demand and installs + restarts now.
- The installed version is shown in **Agency OS → About Agency OS**.
- On first launch the app posts a welcome notification (also how macOS registers
  it for notifications up front).

**Cutting a release:**

1. Bump `version` in `src-tauri/tauri.conf.json` (and `package.json`) — e.g. `0.1.1`.
2. Commit, then tag and push:
   ```bash
   git commit -am "release v0.1.1"
   git tag v0.1.1 && git push origin main v0.1.1
   ```
3. The `Release` GitHub Action builds a universal `.app`, signs it, and publishes
   the GitHub Release with the `.dmg`, `.app.tar.gz`, `.sig`, and `latest.json`.

That's it — every existing install updates itself from the new release.

**Required repo secrets** (set once):

- `TAURI_SIGNING_PRIVATE_KEY` — contents of the updater private key.
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — its password (empty here).

> ⚠️ Back up the private key (`~/.tauri/agency-os-desktop.key`). If it's lost, you
> can't publish updates that existing installs will accept.

## Changing things

- **Default pre-filled URL** → `DEFAULT_URL` in `src/main.js`.
- **Entry path / window title / size** → `app_target()` and the
  `WebviewWindowBuilder` in `src-tauri/src/lib.rs`.
- **App version** → `version` in `src-tauri/tauri.conf.json` and `package.json`.
- **App icon** → regenerate from a square PNG: `pnpm tauri icon path/to/logo.png`.

## Gatekeeper (unsigned builds)

The app is **not** Apple-code-signed, so a downloaded `.dmg` shows *"Apple cannot
check it for malware."* on first open — **right-click → Open → Open** once, or:

```bash
xattr -dr com.apple.quarantine "/Applications/Agency OS.app"
```

To remove the warning entirely, sign + notarize with an Apple Developer ID
(~$99/yr): set the identity under `bundle.macOS` in `tauri.conf.json` plus the
notarization secrets, and the CI build will sign automatically.
