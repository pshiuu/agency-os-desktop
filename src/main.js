const { invoke } = window.__TAURI__.core;

// Pre-filled default — replace with your own server when distributing.
const DEFAULT_URL = "https://agencyos.conradiusdesign.com";

// Turn whatever the user typed into a clean origin (scheme + host + port).
function normalizeBase(raw) {
  let v = (raw || "").trim();
  if (!v) return null;
  if (!/^https?:\/\//i.test(v)) v = "https://" + v;
  try {
    return new URL(v).origin;
  } catch {
    return null;
  }
}

// Best-effort reachability check so obvious typos are caught before we leave
// this screen. `no-cors` resolves for any reachable host and only rejects on
// DNS / connection failure, which is exactly what we want to detect.
async function reachable(base) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), 8000);
  try {
    await fetch(base, { mode: "no-cors", signal: ctrl.signal });
    return true;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

// Hand off to Rust, which navigates the window to <base>/agency-os/home.
// (Navigating to the remote origin from here via window.location does not
// reliably load in WKWebView — Rust-driven navigation does.)
function go(base) {
  return invoke("open_server", { base });
}

window.addEventListener("DOMContentLoaded", async () => {
  const changeMode = new URLSearchParams(window.location.search).has("change");
  const stored = await invoke("get_server_url");

  // Already configured and not explicitly reconfiguring → jump straight in.
  if (stored && !changeMode) {
    await go(stored);
    return;
  }

  const setup = document.querySelector("#setup");
  const form = document.querySelector("#connect-form");
  const input = document.querySelector("#server-input");
  const button = document.querySelector("#connect-btn");
  const errorEl = document.querySelector("#error");
  const forgetEl = document.querySelector("#forget");

  setup.hidden = false;
  input.value = stored || DEFAULT_URL;
  input.focus();
  input.select();

  if (stored) forgetEl.hidden = false;

  forgetEl.addEventListener("click", async (e) => {
    e.preventDefault();
    await invoke("clear_server_url");
    input.value = DEFAULT_URL;
    forgetEl.hidden = true;
    errorEl.textContent = "";
    input.focus();
    input.select();
  });

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    errorEl.textContent = "";

    const base = normalizeBase(input.value);
    if (!base) {
      errorEl.textContent =
        "Please enter a valid URL, e.g. https://agencyos.yourcompany.com";
      return;
    }

    button.disabled = true;
    button.textContent = "Connecting…";

    if (!(await reachable(base))) {
      errorEl.textContent = `Couldn't reach ${base}. Check the address and your connection.`;
      button.disabled = false;
      button.textContent = "Connect";
      return;
    }

    await invoke("set_server_url", { url: base });
    await go(base);
  });
});
