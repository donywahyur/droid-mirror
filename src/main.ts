import { invoke, Channel } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, currentMonitor } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { Player } from "./player";
import { InputHandler } from "./input";

// ---------------------------------------------------------------- types ----

interface Device {
  serial: string;
  state: string;
  model: string | null;
  wireless: boolean;
}

interface StartInfo {
  deviceName: string;
  serial: string;
}

interface VideoOptions {
  maxSize: number;
  bitRate: number;
  maxFps: number;
  codecOptions?: string;
}

interface PhoneState {
  locked: boolean;
  awake: boolean;
  /** A visible window blocks capture (FLAG_SECURE): it mirrors as black. */
  secure: boolean;
}

type Kind = "usb" | "wifi" | "tailscale";

// --------------------------------------------------------------- quality ----

const QUALITY: Record<string, VideoOptions> = {
  minimal: { maxSize: 800, bitRate: 1_000_000, maxFps: 30 },
  low: { maxSize: 1024, bitRate: 2_000_000, maxFps: 30 },
  medium: { maxSize: 1280, bitRate: 4_000_000, maxFps: 60 },
  high: { maxSize: 1920, bitRate: 8_000_000, maxFps: 60 },
  max: { maxSize: 0, bitRate: 16_000_000, maxFps: 0 },
};

/** Auto: Wi-Fi rarely sustains the 8 Mbps USB default, and a stream above
 * the link's throughput queues up in network buffers as ever-growing lag. */
function videoOptions(dev: Device): VideoOptions {
  const base = QUALITY[qualitySelect.value] ?? (dev.wireless ? QUALITY.low : QUALITY.high);
  // Over Wi-Fi, constant bitrate (MediaCodec BITRATE_MODE_CBR = 2) avoids the
  // spikes VBR produces on full-screen motion (camera, video), which a slow
  // link turns into seconds of backlog. USB has headroom, so keep VBR quality.
  return dev.wireless ? { ...base, codecOptions: "bitrate-mode=2" } : base;
}

// ------------------------------------------------------------- elements ----

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const connectView = $<HTMLElement>("connect-view");
const mirrorView = $<HTMLElement>("mirror-view");
const deviceList = $<HTMLUListElement>("device-list");
const emptyEl = $<HTMLElement>("empty");
const qualitySelect = $<HTMLSelectElement>("quality");
const screenOffPref = $<HTMLInputElement>("screen-off-pref");
const stopOnUsePref = $<HTMLInputElement>("stop-on-use-pref");
const statusEl = $<HTMLElement>("status");
const canvas = $<HTMLCanvasElement>("screen");
const stage = $<HTMLElement>("stage");
const controls = $<HTMLElement>("controls");
const mirrorTitle = $<HTMLElement>("mirror-title");
const fpsEl = $<HTMLElement>("fps");
const stageHint = $<HTMLElement>("stage-hint");
const toastEl = $<HTMLElement>("toast");
const screenOffBtn = $<HTMLButtonElement>("screen-off");
const pinBtn = $<HTMLButtonElement>("pin");

const appWindow = getCurrentWindow();
const player = new Player(canvas);
const input = new InputHandler(canvas, player);

// ---------------------------------------------------------------- state ----

let devices: Device[] = [];
let devicesKey = "";
let mirroring = false;
let busySerial: string | null = null;
let deviceName = "";
let lastReset = 0;
/** Whether the running session was started in screen-off mode (server flags
 * like the extended screen timeout can only be set at start). */
let sessionScreenOffMode = false;
/** Window size of the connect view, restored when mirroring stops. */
let connectSize: { w: number; h: number } | null = null;

function loadPref(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function savePref(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* storage unavailable: preference lasts for this run only */
  }
}

let screenOff = loadPref("screenOff") === "1";
let pinned = loadPref("pinned") === "1";
let stopOnUse = loadPref("stopOnUse") !== "0"; // on unless turned off

// ------------------------------------------------------- saved devices ----
// Fixed-port addresses (adb tcpip 5555, e.g. over Tailscale) stay valid
// across network changes until the phone reboots, so they're remembered and
// reconnected automatically. Wireless-debugging ports change every time, and
// mDNS finds those phones on the local network anyway: not saved.

interface Saved {
  addr: string;
  model: string | null;
}

const isFixedPort = (addr: string) => /:5555$/.test(addr);

let saved: Saved[] = (() => {
  try {
    const list = JSON.parse(loadPref("savedDevices") ?? "[]");
    return Array.isArray(list) ? list.filter((x) => typeof x?.addr === "string") : [];
  } catch {
    return [];
  }
})();

function persistSaved() {
  savePref("savedDevices", JSON.stringify(saved));
}

function remember(addr: string, model: string | null) {
  if (!isFixedPort(addr)) return;
  const i = saved.findIndex((x) => x.addr === addr);
  if (i >= 0 && (saved[i].model === model || model === null)) return;
  const entry = { addr, model: model ?? null };
  if (i >= 0) saved[i] = entry;
  else saved.push(entry);
  persistSaved();
}

function forget(addr: string) {
  saved = saved.filter((x) => x.addr !== addr);
  persistSaved();
}
stopOnUsePref.checked = stopOnUse;
qualitySelect.value = loadPref("quality") ?? "auto";

// --------------------------------------------------------------- status ----

let toastTimer = 0;

function toast(text: string, error: boolean) {
  toastEl.textContent = text;
  toastEl.classList.toggle("error", error);
  toastEl.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (toastEl.hidden = true), error ? 7000 : 3500);
}

/** Status line in the connect view; a toast over the phone while mirroring. */
function setStatus(text: string, error = false, showToast = true) {
  statusEl.textContent = text;
  statusEl.title = text;
  statusEl.classList.toggle("error", error);
  if (mirroring && showToast) toast(text, error);
}

function errorText(e: unknown) {
  const msg = typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
  // adb only says "Connection refused"; for a phone that answers on the network
  // this nearly always means Wireless debugging is off or got a new port.
  if (/connection refused/i.test(msg) && !/hint:/.test(msg)) {
    return (
      `${msg} – hint: Wireless debugging is off on the phone, or its port changed. ` +
      "Turn it on (Android turns it off when Wi-Fi changes) and use the port shown on its screen."
    );
  }
  return msg;
}

// -------------------------------------------------------------- devices ----

/** Tailscale hands out addresses from 100.64.0.0/10 (CGNAT range), so the IP
 * alone tells a Tailscale route from a LAN one. */
function kindOf(d: Device): Kind {
  if (!d.wireless) return "usb";
  const m = /^100\.(\d+)\./.exec(d.serial);
  return m && Number(m[1]) >= 64 && Number(m[1]) <= 127 ? "tailscale" : "wifi";
}

const KIND_LABEL: Record<Kind, string> = { usb: "USB", wifi: "Wi-Fi", tailscale: "Tailscale" };
const KIND_ICON: Record<Kind, string> = { usb: "🔌", wifi: "📶", tailscale: "🌐" };

function address(d: Device): string {
  if (!d.wireless) return d.serial;
  const ip = d.serial.split(":")[0];
  return /^\d+\.\d+\.\d+\.\d+$/.test(ip) ? d.serial : d.serial.replace(/\._adb-tls-connect\._tcp.*$/, "");
}

function stateText(d: Device): { text: string; warn: boolean } {
  switch (d.state) {
    case "device":
      return { text: "", warn: false };
    case "available":
      return { text: "Found on network", warn: false };
    case "saved":
      return { text: "Saved · not connected", warn: false };
    case "unauthorized":
      return { text: "Allow USB debugging on the phone", warn: true };
    case "offline":
      return { text: "Offline", warn: true };
    default:
      return { text: d.state, warn: true };
  }
}

const canMirror = (d: Device) => d.state === "device" || d.state === "available" || d.state === "saved";

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

function renderDevices() {
  const items = devices.map((d) => {
    const kind = kindOf(d);
    const li = el("li", "device");
    li.classList.toggle("busy", busySerial === d.serial);

    const main = el("button", "device-main");
    main.disabled = !canMirror(d) || busySerial !== null;
    main.title = canMirror(d) ? `Mirror ${d.model ?? d.serial}` : stateText(d).text;
    main.addEventListener("click", () => startMirror(d));

    const icon = el("span", "device-icon", KIND_ICON[kind]);
    const meta = el("span", "device-meta");
    meta.append(el("span", "device-name", d.model ?? "Android device"));
    const sub = el("span", "device-sub");
    sub.append(el("span", `badge ${kind}`, KIND_LABEL[kind]));
    sub.append(el("span", "", address(d)));
    meta.append(sub);
    // State on its own line so long messages wrap instead of being cut off.
    const st = busySerial === d.serial ? { text: "Connecting…", warn: false } : stateText(d);
    if (st.text) meta.append(el("span", `device-state${st.warn ? " warn" : ""}`, st.text));
    main.append(icon, meta);

    const actions = el("span", "device-actions");
    if (kind === "usb" && d.state === "device") {
      const wifi = el("button", "", "Wi-Fi");
      wifi.title = "Switch this phone to adb over the network on port 5555 (adb tcpip 5555), then connect";
      wifi.disabled = busySerial !== null;
      wifi.addEventListener("click", () => runAdb("Switching to Wi-Fi", "enable_wifi", { serial: d.serial }));
      actions.append(wifi);
    }
    if (d.wireless && d.state !== "available") {
      const isSaved = saved.some((x) => x.addr === d.serial);
      const x = el("button", "", "✕");
      x.title = isSaved ? "Disconnect and forget" : "Disconnect";
      x.setAttribute("aria-label", `${x.title} ${d.serial}`);
      x.disabled = busySerial !== null;
      x.addEventListener("click", () => {
        // Forget first, or auto-reconnect would bring it straight back.
        if (isSaved) forget(d.serial);
        if (d.state === "saved") refreshDevices(true);
        else runAdb("Disconnecting", "adb_disconnect", { addr: d.serial });
      });
      actions.append(x);
    }

    li.append(main, actions);
    return li;
  });
  deviceList.replaceChildren(...items);
  emptyEl.hidden = devices.length > 0;
}

async function refreshDevices(force = false) {
  try {
    devices = await invoke<Device[]>("list_devices");
  } catch (e) {
    devices = [];
    setStatus(errorText(e), true);
  }
  // Any fixed-port device seen connected is remembered (Wi-Fi button,
  // Connect form, Tailscale...); saved ones not connected get a card too.
  for (const d of devices) if (d.wireless && d.state === "device") remember(d.serial, d.model);
  for (const x of saved) {
    if (!devices.some((d) => d.serial === x.addr)) {
      devices.push({ serial: x.addr, state: "saved", model: x.model, wireless: true });
    }
  }
  const key = JSON.stringify(devices) + busySerial;
  if (force || key !== devicesKey) {
    devicesKey = key;
    renderDevices();
  }
  if (!mirroring && !busySerial && !statusEl.classList.contains("error")) {
    const ready = devices.filter(canMirror).length;
    setStatus(ready ? "Click a device to mirror it" : "No device ready", false, false);
  }
}

async function runAdb(label: string, cmd: string, args: Record<string, unknown>) {
  setStatus(`${label}…`);
  try {
    const msg = await invoke<string>(cmd, args);
    setStatus(msg || `${label}: done`);
  } catch (e) {
    setStatus(errorText(e), true);
  }
  await refreshDevices(true);
}

// --------------------------------------------------------------- window ----

async function logicalInnerSize() {
  const [size, factor] = await Promise.all([appWindow.innerSize(), appWindow.scaleFactor()]);
  return { w: size.width / factor, h: size.height / factor };
}

/** Size of the mirror view's top bar and controls column (CSS --bar-h / --side-w). */
function chromeSize() {
  const css = getComputedStyle(document.documentElement);
  return {
    bar: parseFloat(css.getPropertyValue("--bar-h")) || 28,
    side: parseFloat(css.getPropertyValue("--side-w")) || 44,
  };
}

/** Resize the window so the phone area matches the phone's shape (plus the top
 * bar and controls column), keeping the phone's long side, so a rotation swaps
 * width and height. */
async function fitWindowToVideo(vw: number, vh: number) {
  try {
    const { bar, side } = chromeSize();
    const cur = await logicalInnerSize();
    const mon = await currentMonitor();
    const f = mon?.scaleFactor ?? 1;
    // Largest phone area that still fits on screen.
    const maxW = (mon ? (mon.size.width / f) * 0.92 : 1600) - side;
    const maxH = (mon ? (mon.size.height / f) * 0.88 : 1000) - bar;
    const long = Math.max(cur.w - side, cur.h - bar, 560);
    let w: number;
    let h: number;
    if (vh >= vw) {
      h = Math.min(long, maxH);
      w = (h * vw) / vh;
    } else {
      w = Math.min(long, maxW);
      h = (w * vh) / vw;
    }
    if (w > maxW) [w, h] = [maxW, (maxW * vh) / vw];
    if (h > maxH) [w, h] = [(maxH * vw) / vh, maxH];
    await appWindow.setSize(new LogicalSize(Math.round(w + side), Math.round(h + bar)));
  } catch (e) {
    console.warn("window fit failed", e);
  }
}

async function restoreConnectSize() {
  if (!connectSize) return;
  try {
    await appWindow.setSize(new LogicalSize(Math.round(connectSize.w), Math.round(connectSize.h)));
  } catch (e) {
    console.warn(e);
  }
}

function applyPinned() {
  pinBtn.setAttribute("aria-pressed", String(pinned));
  pinBtn.title = pinned ? "Window stays on top (click to release)" : "Keep window on top";
  appWindow.setAlwaysOnTop(pinned).catch((e) => console.warn(e));
}

/** Scale the canvas to fit the phone area while keeping the phone's aspect ratio. */
function fitCanvas() {
  const box = stage.getBoundingClientRect();
  const w = player.width || canvas.width;
  const h = player.height || canvas.height;
  if (!w || !h) return;
  const scale = Math.min(box.width / w, box.height / h);
  canvas.style.width = `${Math.floor(w * scale)}px`;
  canvas.style.height = `${Math.floor(h * scale)}px`;
}

// ---------------------------------------------------------------- views ----

function showMirrorView(on: boolean) {
  connectView.hidden = on;
  mirrorView.hidden = !on;
  if (!on) {
    stageHint.hidden = true;
    toastEl.hidden = true;
  }
}

function showHint(text: string | null) {
  stageHint.hidden = !text;
  stageHint.textContent = text ?? "";
}

function renderScreenOff() {
  screenOffPref.checked = screenOff;
  screenOffBtn.setAttribute("aria-pressed", String(screenOff));
  screenOffBtn.textContent = screenOff ? "📴" : "📱";
  screenOffBtn.title = screenOff ? "Phone screen off (click to turn it on)" : "Turn phone screen off";
}

function setScreenOff(value: boolean) {
  screenOff = value;
  savePref("screenOff", value ? "1" : "0");
  renderScreenOff();
  if (!mirroring) return;
  invoke("set_display_power", { on: !value }).catch((e) => setStatus(errorText(e), true));
  if (!value) setStatus("Phone screen on");
  else if (sessionScreenOffMode) setStatus("Phone screen turns off once the phone is unlocked");
  else setStatus("Phone screen off. Its own screen timeout still applies: stop and mirror again to keep it awake.", true);
  canvas.focus();
}

// ------------------------------------------------------------ mirroring ----

async function startMirror(dev: Device) {
  if (busySerial || mirroring) return;
  busySerial = dev.serial;
  renderDevices();

  if (dev.state === "available" || dev.state === "saved") {
    setStatus(`Connecting to ${dev.serial}…`);
    try {
      if (dev.state === "saved") {
        const ok = await invoke<string[]>("reconnect", { addrs: [dev.serial] });
        if (!ok.length) throw new Error(`Couldn't reach ${dev.serial}`);
      } else {
        await invoke<string>("adb_connect", { addr: dev.serial });
      }
    } catch (e) {
      busySerial = null;
      setStatus(
        dev.state === "saved"
          ? `${errorText(e)}. If the phone restarted, plug in USB and press Wi-Fi again; on Tailscale, check both devices are online.`
          : `${errorText(e)} – if this phone was never paired with this Mac, pair it first.`,
        true,
      );
      await refreshDevices(true);
      return;
    }
  }

  setStatus(`Starting on ${dev.model ?? dev.serial}…`);
  player.reset();
  const channel = new Channel<ArrayBuffer | number[] | Uint8Array>();
  channel.onmessage = (msg) => {
    if (msg instanceof ArrayBuffer) player.push(msg);
    else if (msg instanceof Uint8Array) player.push(msg.slice().buffer);
    else player.push(Uint8Array.from(msg).buffer);
  };

  try {
    connectSize = await logicalInnerSize().catch(() => null);
    const info = await invoke<StartInfo>("start_mirror", {
      serial: dev.serial,
      video: videoOptions(dev),
      options: { screenOffMode: screenOff, stopOnUse },
      onPacket: channel,
    });
    mirroring = true;
    deviceName = info.deviceName;
    sessionScreenOffMode = screenOff;
    input.enabled = true;
    mirrorTitle.textContent = `${deviceName} · ${KIND_LABEL[kindOf(dev)]}`;
    appWindow.setTitle(`Droid Mirror – ${deviceName}`).catch(() => {});
    showMirrorView(true);
    if (screenOff) invoke("set_display_power", { on: false }).catch(() => {});
    setStatus(`Mirroring ${deviceName}`, false, false);
    fitCanvas();
    canvas.focus();
  } catch (e) {
    setStatus(errorText(e), true);
  } finally {
    busySerial = null;
    renderDevices();
  }
}

/** Back to the connect view, after a stop or a lost connection. */
function leaveMirror() {
  mirroring = false;
  input.enabled = false;
  player.reset();
  showMirrorView(false);
  appWindow.setTitle("Droid Mirror").catch(() => {});
  restoreConnectSize();
  refreshDevices(true);
}

async function stopMirror() {
  try {
    await invoke("stop_mirror");
  } catch (e) {
    console.warn(e);
  }
  leaveMirror();
  setStatus("Stopped", false, false);
}

// --------------------------------------------------------------- wiring ----

player.onResize = (w, h) => {
  fitCanvas();
  if (mirroring || busySerial) fitWindowToVideo(w, h);
};
player.onConfig = (sps, codec) =>
  invoke("log_stats", { line: `config codec=${codec} sps=${sps} size=${player.width}x${player.height}` }).catch(
    () => {},
  );
player.onNeedKeyFrame = (reason) => {
  const now = performance.now();
  if (now - lastReset < 1000) return;
  lastReset = now;
  console.warn("requesting new key frame:", reason);
  invoke("device_action", { name: "reset_video" }).catch(() => {});
};
new ResizeObserver(fitCanvas).observe(stage);

$("refresh").addEventListener("click", () => refreshDevices(true));
qualitySelect.addEventListener("change", () => savePref("quality", qualitySelect.value));
screenOffPref.addEventListener("change", () => setScreenOff(screenOffPref.checked));
stopOnUsePref.addEventListener("change", () => {
  stopOnUse = stopOnUsePref.checked;
  savePref("stopOnUse", stopOnUse ? "1" : "0");
});
screenOffBtn.addEventListener("click", () => setScreenOff(!screenOff));
pinBtn.addEventListener("click", () => {
  pinned = !pinned;
  savePref("pinned", pinned ? "1" : "0");
  applyPinned();
  canvas.focus();
});
$("stop").addEventListener("click", () => stopMirror());

/** Phone screen -> Mac clipboard: a full-resolution screenshot from the phone,
 * or, if that fails, the frame being shown here (mirror resolution). */
async function copyScreen() {
  const btn = $<HTMLButtonElement>("copy-screen");
  btn.disabled = true;
  try {
    const size = await invoke<string>("copy_screen");
    setStatus(`Screen copied${size ? ` (${size})` : ""}`);
  } catch (e) {
    console.warn("screencap failed, using the mirrored frame", e);
    try {
      await invoke("copy_png_to_mac", { dataUrl: canvas.toDataURL("image/png") });
      setStatus(`Screen copied at mirror resolution (${canvas.width}×${canvas.height})`);
    } catch (e2) {
      setStatus(`Couldn't copy the screen: ${errorText(e2)}`, true);
    }
  } finally {
    btn.disabled = false;
    canvas.focus();
  }
}
$("copy-screen").addEventListener("click", () => copyScreen());

/** Phone -> Mac: newest image on the phone to the Mac clipboard. */
$("copy-latest").addEventListener("click", async () => {
  const btn = $<HTMLButtonElement>("copy-latest");
  btn.disabled = true;
  setStatus("Copying the phone's newest image…");
  try {
    const name = await invoke<string>("copy_latest_image");
    setStatus(`Copied ${name} to the Mac clipboard`);
  } catch (e) {
    setStatus(`Couldn't copy the image: ${errorText(e)}`, true);
  } finally {
    btn.disabled = false;
    canvas.focus();
  }
});

input.onNotice = (text, error = false) => setStatus(text, error);

$("connect-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const addr = $<HTMLInputElement>("connect-addr").value.trim();
  if (addr) runAdb(`Connecting to ${addr}`, "adb_connect", { addr });
});

$("pair-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const addr = $<HTMLInputElement>("pair-addr").value.trim();
  const code = $<HTMLInputElement>("pair-code").value.trim();
  if (addr && code) runAdb(`Pairing with ${addr}`, "adb_pair", { addr, code });
});

controls.querySelectorAll<HTMLButtonElement>("button[data-key], button[data-action]").forEach((btn) => {
  btn.addEventListener("click", () => {
    const req = btn.dataset.key
      ? invoke("press_key", { keycode: Number(btn.dataset.key) })
      : invoke("device_action", { name: btn.dataset.action });
    req.catch((e) => setStatus(errorText(e), true));
    canvas.focus();
  });
});


// Phone lock / sleep state, polled by Rust while mirroring.
listen<PhoneState>("phone-state", ({ payload: s }) => {
  if (!mirroring) return;
  if (!s.awake) {
    showHint("Phone is asleep. Press ⏻ to wake it.");
  } else if (s.secure && !s.locked) {
    showHint(
      "This screen is protected by the app (for example a WhatsApp view-once photo), so it can't be mirrored. View it on the phone.",
    );
  } else if (s.locked) {
    showHint(null);
    setStatus(screenOff ? "Phone is locked: unlock it here. Its screen turns off after you unlock." : "Phone is locked");
  } else {
    showHint(null);
  }
});

listen("screen-off-applied", () => setStatus("Phone screen off"));

// Someone touched the phone itself (Rust already stopped the session).
listen("phone-used", () => {
  leaveMirror();
  setStatus("Stopped: the phone was used", false, false);
});

listen<string>("use-watch-unavailable", (e) =>
  setStatus(`Can't detect phone use, so mirroring won't stop by itself: ${e.payload}`, true),
);

listen<string>("mirror-closed", (e) => {
  leaveMirror();
  setStatus(`Disconnected: ${e.payload}`, true);
});

setInterval(() => {
  if (!mirroring) return;
  invoke("log_stats", { line: player.takeStats() }).catch(() => {});
  const delay = Math.round(player.delayMs);
  fpsEl.textContent = `${player.fps} fps · +${delay} ms`;
  fpsEl.title = "Delay: extra time frames spend in transit compared to the fastest frame so far";
  fpsEl.classList.toggle("warn", delay > 300);
}, 1000);

let reconnecting = false;
let lastReconnect = 0;

/** Quietly reconnect saved addresses that aren't connected (4 s timeout each). */
async function autoReconnect() {
  if (reconnecting || mirroring || busySerial) return;
  const missing = saved
    .map((x) => x.addr)
    .filter((a) => !devices.some((d) => d.serial === a && d.state === "device"));
  if (!missing.length) return;
  reconnecting = true;
  lastReconnect = Date.now();
  try {
    const ok = await invoke<string[]>("reconnect", { addrs: missing });
    if (ok.length) await refreshDevices(true);
  } catch (e) {
    console.warn("reconnect failed", e);
  } finally {
    reconnecting = false;
  }
}

// Poll for plugged / unplugged / discovered devices while on the connect view,
// and retry saved devices every 15 s.
setInterval(() => {
  const typing = document.activeElement instanceof HTMLInputElement || document.activeElement instanceof HTMLSelectElement;
  if (mirroring || busySerial || typing) return;
  refreshDevices();
  if (Date.now() - lastReconnect > 15_000) autoReconnect();
}, 3000);

renderScreenOff();
applyPinned();
if (!Player.supported()) {
  setStatus("WebCodecs VideoDecoder is not available in this WebView (needs macOS 13.3+ / Safari 16.4+).", true);
}
refreshDevices(true).then(autoReconnect);
