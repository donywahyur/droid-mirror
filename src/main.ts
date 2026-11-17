import { invoke, Channel } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Player } from "./player";
import { InputHandler } from "./input";

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

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const deviceSelect = $<HTMLSelectElement>("device");
const sizeSelect = $<HTMLSelectElement>("size");
const mirrorBtn = $<HTMLButtonElement>("mirror");
const refreshBtn = $<HTMLButtonElement>("refresh");
const wifiToggle = $<HTMLButtonElement>("wifi-toggle");
const wifiPanel = $<HTMLElement>("wifi-panel");
const stage = $<HTMLElement>("stage");
const canvas = $<HTMLCanvasElement>("screen");
const placeholder = $<HTMLElement>("placeholder");
const statusEl = $<HTMLElement>("status");
const fpsEl = $<HTMLElement>("fps");

const player = new Player(canvas);
const input = new InputHandler(canvas, player);

let devices: Device[] = [];
let mirroring = false;
let busy = false;
let lastReset = 0;

function setStatus(text: string, error = false) {
  statusEl.textContent = text;
  statusEl.title = text;
  statusEl.classList.toggle("error", error);
}

function errorText(e: unknown) {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}

function selectedDevice(): Device | undefined {
  return devices.find((d) => d.serial === deviceSelect.value);
}

function updateUi() {
  const dev = selectedDevice();
  mirrorBtn.textContent = mirroring ? "Stop" : busy ? "Starting…" : "Mirror";
  mirrorBtn.classList.toggle("stop", mirroring);
  mirrorBtn.disabled = busy || (!mirroring && dev?.state !== "device");
  deviceSelect.disabled = mirroring || busy;
  sizeSelect.disabled = mirroring || busy;
  canvas.hidden = !mirroring;
  placeholder.hidden = mirroring;
  $<HTMLButtonElement>("wifi-enable").disabled = !dev || dev.wireless || dev.state !== "device";
  $<HTMLButtonElement>("wifi-disconnect").disabled = !dev || !dev.wireless;
  document.querySelectorAll<HTMLButtonElement>(".navbar button").forEach((b) => (b.disabled = !mirroring));
}

/** Scale the canvas to fit the stage while keeping the device aspect ratio. */
function fit() {
  const w = player.width || canvas.width;
  const h = player.height || canvas.height;
  if (!w || !h) return;
  const box = stage.getBoundingClientRect();
  const scale = Math.min(box.width / w, box.height / h);
  canvas.style.width = `${Math.floor(w * scale)}px`;
  canvas.style.height = `${Math.floor(h * scale)}px`;
}

async function refreshDevices(prefer?: string) {
  try {
    devices = await invoke<Device[]>("list_devices");
  } catch (e) {
    devices = [];
    setStatus(errorText(e), true);
  }
  const previous = prefer ?? deviceSelect.value;
  deviceSelect.replaceChildren(
    ...devices.map((d) => {
      const label = `${d.model ?? d.serial}${d.wireless ? " (Wi-Fi)" : " (USB)"}`;
      const suffix =
        d.state === "device" ? "" : d.state === "unauthorized" ? " – allow USB debugging on phone" : ` – ${d.state}`;
      return new Option(label + suffix, d.serial);
    }),
  );
  if (devices.length === 0) deviceSelect.append(new Option("No devices found", ""));
  if (devices.some((d) => d.serial === previous)) deviceSelect.value = previous;
  updateUi();
}

async function startMirror() {
  const dev = selectedDevice();
  if (!dev) return;
  busy = true;
  updateUi();
  setStatus(`Starting scrcpy-server on ${dev.model ?? dev.serial}…`);
  player.reset();

  const channel = new Channel<ArrayBuffer | number[] | Uint8Array>();
  channel.onmessage = (msg) => {
    if (msg instanceof ArrayBuffer) player.push(msg);
    else if (msg instanceof Uint8Array) player.push(msg.slice().buffer);
    else player.push(Uint8Array.from(msg).buffer);
  };

  try {
    const info = await invoke<StartInfo>("start_mirror", {
      serial: dev.serial,
      maxSize: Number(sizeSelect.value),
      onPacket: channel,
    });
    mirroring = true;
    input.enabled = true;
    setStatus(`Mirroring ${info.deviceName}`);
    document.title = `Droid Mirror – ${info.deviceName}`;
  } catch (e) {
    setStatus(errorText(e), true);
  } finally {
    busy = false;
    updateUi();
    if (mirroring) {
      fit();
      canvas.focus();
    }
  }
}

async function stopMirror() {
  mirroring = false;
  input.enabled = false;
  updateUi();
  try {
    await invoke("stop_mirror");
  } catch (e) {
    console.warn(e);
  }
  player.reset();
  document.title = "Droid Mirror";
  setStatus("Stopped");
}

async function runAdb(label: string, cmd: string, args: Record<string, unknown>) {
  setStatus(`${label}…`);
  try {
    const msg = await invoke<string>(cmd, args);
    setStatus(msg || `${label}: done`);
  } catch (e) {
    setStatus(errorText(e), true);
  }
  await refreshDevices();
}

// ---- wiring ----

player.onResize = () => fit();
player.onNeedKeyFrame = (reason) => {
  const now = performance.now();
  if (now - lastReset < 1000) return;
  lastReset = now;
  console.warn("requesting new key frame:", reason);
  invoke("device_action", { name: "reset_video" }).catch(() => {});
};
new ResizeObserver(fit).observe(stage);

mirrorBtn.addEventListener("click", () => (mirroring ? stopMirror() : startMirror()));
refreshBtn.addEventListener("click", () => refreshDevices());
deviceSelect.addEventListener("change", updateUi);

wifiToggle.addEventListener("click", () => {
  wifiPanel.hidden = !wifiPanel.hidden;
  wifiToggle.setAttribute("aria-expanded", String(!wifiPanel.hidden));
  requestAnimationFrame(fit);
});

$("wifi-enable").addEventListener("click", () => {
  const dev = selectedDevice();
  if (dev) runAdb("Switching to Wi-Fi", "enable_wifi", { serial: dev.serial });
});

$("wifi-disconnect").addEventListener("click", () => {
  const dev = selectedDevice();
  if (dev) runAdb("Disconnecting", "adb_disconnect", { addr: dev.serial });
});

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

document.querySelectorAll<HTMLButtonElement>(".navbar button").forEach((btn) => {
  btn.addEventListener("click", () => {
    const req = btn.dataset.key
      ? invoke("press_key", { keycode: Number(btn.dataset.key) })
      : invoke("device_action", { name: btn.dataset.action });
    req.catch((e) => setStatus(errorText(e), true));
    canvas.focus();
  });
});

listen<string>("mirror-closed", (e) => {
  mirroring = false;
  input.enabled = false;
  player.reset();
  document.title = "Droid Mirror";
  setStatus(`Disconnected: ${e.payload}`, true);
  refreshDevices();
});

setInterval(() => {
  fpsEl.textContent = mirroring ? `${player.fps} fps · ${player.width}×${player.height}` : "";
}, 1000);

// Poll for plugged/unplugged devices while idle.
setInterval(() => {
  if (!mirroring && !busy && document.activeElement !== deviceSelect) refreshDevices();
}, 3000);

if (!Player.supported()) {
  setStatus("WebCodecs VideoDecoder is not available in this WebView (needs macOS 13.3+ / Safari 16.4+).", true);
}
refreshDevices();
