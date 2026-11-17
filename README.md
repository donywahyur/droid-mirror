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

## Controls

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

## Wi-Fi

- **USB first:** select the USB device, then click *Wi-Fi → Switch selected USB
  device to Wi-Fi*. This runs `adb tcpip 5555` and then `adb connect <ip>:5555`.
  After that you can unplug the cable.
- **Android 11+ wireless debugging:** use *Pair* with the pairing ip:port and
  code, then *Connect* to the ip:port on the Wireless debugging screen.

Port 5555 stays open on your LAN until the phone reboots. adb still requires
the RSA key you approved, but use it only on trusted networks.

## Layout

- `src-tauri/src/adb.rs`: adb discovery, devices, connect, pair, tcpip
- `src-tauri/src/mirror.rs`: server launch, sockets, video pump, clipboard
- `src-tauri/src/control.rs`: control message serialization (unit-tested)
- `src/player.ts`: packet parsing, Annex-B to AVCC conversion, WebCodecs decoding
- `src/input.ts`: pointer and keyboard mapping
