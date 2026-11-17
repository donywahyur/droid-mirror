# Droid Mirror

I made Droid Mirror to connect my Android phone to my MacBook. It shows the
phone's screen live in a native macOS window. I can tap, swipe and scroll with
the mouse or trackpad, type with the Mac keyboard, and copy and paste between
the two devices. It works over USB or Wi-Fi, and nothing needs to be installed
on the phone.

Under the hood it's a Tauri app: Rust launches
[scrcpy](https://github.com/Genymobile/scrcpy)'s server on the device over adb,
and the webview decodes the H.264 stream with WebCodecs.

## How it works

1. Rust pushes `scrcpy-server` to the phone (`adb push`), opens an `adb forward`
   tunnel and starts the server with `app_process` (no app install, no root).
2. The server streams H.264 over the video socket. Rust forwards each packet to
   the webview through a Tauri `Channel`.
3. The webview decodes with WebCodecs `VideoDecoder` and draws to a `<canvas>`
   (`src/player.ts`).
4. Mouse, wheel and keyboard events become scrcpy control messages
   (`src/input.ts` → Rust commands → `src-tauri/src/control.rs`).

The protocol targets **scrcpy-server 5.0.1**. The server rejects a client whose version
string differs, so the version is read from `scrcpy --version` at runtime.

## Requirements

```bash
brew install --cask android-platform-tools
brew install scrcpy
```

On the phone, enable Developer options → USB debugging.

Overrides: `ADB`, `SCRCPY_SERVER_PATH`, `SCRCPY_SERVER_VERSION`.

## Run

```bash
pnpm install
pnpm tauri dev
pnpm tauri build   # .app / .dmg in src-tauri/target/release/bundle
```

## Using it

- **Connect screen:** each device is a card showing how it's reached: *USB*,
  *Wi-Fi* or *Tailscale*, plus its address. Problems such as "Allow USB
  debugging on the phone" or "Offline" appear underneath. Click a card to
  mirror it. USB cards have a **Wi-Fi** button (`adb tcpip 5555`), and wireless
  cards have **✕** to disconnect. *Add a device by address* connects or pairs by
  ip:port. Quality and *Turn phone screen off* are remembered.
- **Mirror view:** the phone fills the window, with nothing on top of it. A
  thin top bar holds the window buttons, the device name (drag it to move the
  window), and fps and delay. A column on the right is always visible: Back,
  Home, Recents, volume, power, rotate, notifications, phone screen off,
  📌 keep on top, and ■ stop.
- **Window:** when mirroring starts, the window resizes to the phone's shape
  (plus the bar and column). It resizes again on rotation, and goes back to
  its previous size when you stop.

| Input | Action |
| --- | --- |
| Left click / drag | Tap / swipe |
| Right click | Back |
| Middle click | Home |
| Wheel / trackpad | Scroll |
| Typing | Text input |
| Enter, Backspace, arrows, Tab, Esc… | Android keys |
| Ctrl+letter | Android Ctrl shortcuts |
| Cmd+V | Paste the Mac clipboard on the device |
| Cmd+C | Copy on the device into the Mac clipboard |

## Phone screen off

*Turn phone screen off* on the connect screen (or 📴 in the mirror view's
control column) keeps the phone's own screen dark while you use it from the
Mac. Turn it on before you start mirroring. The setting is remembered.

- If the phone is locked, its screen stays on until you unlock it (from the
  Mac or by hand), then turns off. Android puts a locked phone back to sleep
  after about 10 s, and a sleeping phone mirrors as black.
- Unlocking turns the screen back on, so it is turned off again after every
  unlock. Waking the phone with its own power button leaves it on.
- For the session, the phone's screen timeout is raised to 30 minutes, so
  the phone doesn't fall asleep while dark. scrcpy-server restores your setting
  when mirroring stops, and the phone locks (`power_off_on_close`).
- While mirroring, the app checks once a second whether the phone is locked or
  asleep (`dumpsys`, filtered on the phone) and says so instead of showing a
  black picture.

A dark screen is not a lock. Until mirroring stops, anyone holding the phone
can press its power button and get in unlocked.

## Wi-Fi

- **Android 11+ wireless debugging:** phones with Wireless debugging on are
  found automatically on the local network (adb mDNS) and listed as
  "found on network". Clicking the card connects first. A phone that was never
  paired with this Mac needs *Add a device by address → Pair* once (pairing
  ip:port and code).
- **USB first:** click **Wi-Fi** on the phone's USB card. This runs
  `adb tcpip 5555` and then `adb connect <ip>:5555`. After that you can unplug
  the cable.

Wireless debugging is what makes adb listen on the network, so it must be on
to connect that way. Android turns it off when the Wi-Fi connection changes,
and it gets a new port each time. "Connection refused" from a phone that is
otherwise reachable almost always means it is off.

## Tailscale (mirror from anywhere)

With [Tailscale](https://tailscale.com) on both the Mac and the phone, the
phone has a fixed `100.x.y.z` address the Mac can reach from any network.
The device list labels entries as *Wi-Fi* or *Tailscale* with their IP.

The steadiest setup is the fixed adb port, because it doesn't need Wireless
debugging and works on mobile data too:

1. Plug in USB once, then click **Wi-Fi** on the phone's USB card
   (`adb tcpip 5555`).
2. *Add a device by address → Connect* to `<phone's Tailscale IP>:5555`, then
   unplug.
3. Repeat after each phone reboot, which closes port 5555.

Port 5555 is then open on every network the phone joins, not only Tailscale.
adb still requires the RSA key you approved, but reboot the phone before using
public Wi-Fi.

Wireless debugging also works over Tailscale (`<Tailscale IP>:<port shown on
the phone>`), but the phone must be on some Wi-Fi, the port changes, and mDNS
discovery only finds phones on the local network.

On the same network, Tailscale takes the direct LAN path, so speed is about
the same as plain Wi-Fi. Through a relay it is slower: use *Low* or *Minimal*.

## Quality and lag

The quality menu sets the size, bitrate and frame-rate cap. *Auto* uses High
(1920p, 8 Mbps, 60 fps) over USB and Low (1024p, 2 Mbps, 30 fps) over Wi-Fi.
A stream above the link's throughput queues up in network buffers as growing
lag. Over Wi-Fi the encoder runs in constant-bitrate mode
(`video_codec_options=bitrate-mode=2`): full-screen motion such as the camera
otherwise makes VBR overshoot, and a slow link turns that into seconds of
backlog.

The mirror view's top bar shows `fps · +delay`. The delay is transit time above
the fastest frame so far, and it turns red above 300 ms. Each second the app
also prints a `[stats]` line to its terminal: arrival and decode counts,
transit, decode time, decoder queue, and the largest packet gap both at the
socket (`socketGapMax`) and in the webview (`arrivalGapMax`).

Most Wi-Fi lag is the network itself: crowded 2.4 GHz, and two hops through
the router. 5 GHz or USB helps far more than lower quality.

Port 5555 stays open on your LAN until the phone reboots. adb still requires
the RSA key you approved, but use it only on trusted networks.

## Layout

- `src-tauri/src/adb.rs`: adb discovery, devices, connect, pair, tcpip
- `src-tauri/src/mirror.rs`: server launch, sockets, video pump, clipboard
- `src-tauri/src/control.rs`: control message serialization (unit-tested)
- `src/player.ts`: packet parsing, Annex-B to AVCC conversion, WebCodecs decoding
- `src/input.ts`: pointer and keyboard mapping
