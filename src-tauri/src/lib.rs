mod adb;
mod control;
mod mirror;

use mirror::{MirrorState, SessionOptions, StartInfo, StopKind, VideoOptions};
use std::thread;
use std::time::Duration;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager};

type Res<T> = Result<T, String>;

/// Run blocking work (adb calls, socket setup) off the main thread.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

// ---- adb / devices ----

#[tauri::command]
async fn list_devices() -> Res<Vec<adb::Device>> {
    blocking(adb::devices).await
}

#[tauri::command]
async fn adb_connect(addr: String) -> Res<String> {
    blocking(move || adb::connect(&addr)).await
}

#[tauri::command]
async fn adb_pair(addr: String, code: String) -> Res<String> {
    blocking(move || adb::pair(&addr, &code)).await
}

#[tauri::command]
async fn adb_disconnect(addr: String) -> Res<String> {
    blocking(move || adb::disconnect(&addr)).await
}

/// Switch a USB-connected device to adb over TCP and connect to it.
#[tauri::command]
async fn enable_wifi(serial: String) -> Res<String> {
    blocking(move || {
        let ip = adb::wifi_ip(&serial)?;
        adb::run(Some(&serial), &["tcpip", "5555"])?;
        let addr = format!("{ip}:5555");
        let mut last_err = String::new();
        // adbd restarts in TCP mode; give it a moment.
        for _ in 0..6 {
            thread::sleep(Duration::from_millis(1000));
            match adb::connect(&addr) {
                Ok(msg) => return Ok(msg),
                Err(e) => last_err = e,
            }
        }
        Err(format!("Could not connect to {addr}: {last_err}"))
    })
    .await
}

// ---- mirroring ----

#[tauri::command]
async fn start_mirror(
    app: AppHandle,
    serial: String,
    video: VideoOptions,
    options: SessionOptions,
    on_packet: Channel<InvokeResponseBody>,
) -> Res<StartInfo> {
    blocking(move || {
        mirror::stop(&app.state::<MirrorState>(), None, StopKind::Quiet);
        mirror::start(&app, &serial, video, options, on_packet)
    })
    .await
}

#[tauri::command]
async fn stop_mirror(app: AppHandle) -> Res<()> {
    blocking(move || {
        mirror::stop(&app.state::<MirrorState>(), None, StopKind::User);
        Ok(())
    })
    .await
}

// ---- input ----

#[tauri::command]
fn touch(app: AppHandle, action: u8, x: i32, y: i32, w: u16, h: u16) -> Res<()> {
    mirror::current(&app)?.send(&control::touch(action, x, y, w, h))
}

#[tauri::command]
fn scroll(app: AppHandle, x: i32, y: i32, w: u16, h: u16, hscroll: f32, vscroll: f32) -> Res<()> {
    mirror::current(&app)?.send(&control::scroll(x, y, w, h, hscroll, vscroll))
}

#[tauri::command]
fn key(app: AppHandle, action: u8, keycode: u32, repeat: u32, meta: u32) -> Res<()> {
    mirror::current(&app)?.send(&control::keycode(action, keycode, repeat, meta))
}

#[tauri::command]
fn press_key(app: AppHandle, keycode: u32) -> Res<()> {
    let session = mirror::current(&app)?;
    mirror::send_key_press(&session, keycode)
}

#[tauri::command]
fn inject_text(app: AppHandle, text: String) -> Res<()> {
    let session = mirror::current(&app)?;
    for chunk in control::text_chunks(&text) {
        session.send(&control::text(chunk))?;
    }
    Ok(())
}

/// Turn the phone's own screen on/off; mirroring continues either way.
#[tauri::command]
fn set_display_power(app: AppHandle, on: bool) -> Res<()> {
    mirror::current(&app)?.set_screen_off(!on)
}

/// One-shot device actions without payload.
#[tauri::command]
fn device_action(app: AppHandle, name: String) -> Res<()> {
    let kind = match name.as_str() {
        "rotate" => control::TYPE_ROTATE_DEVICE,
        "notifications" => control::TYPE_EXPAND_NOTIFICATION_PANEL,
        "reset_video" => control::TYPE_RESET_VIDEO,
        other => return Err(format!("unknown action {other}")),
    };
    mirror::current(&app)?.send(&control::empty(kind))
}

/// Cmd+V: text on the Mac clipboard is pasted on the phone. With only an
/// image there (or with `image_only`, Cmd+Shift+V), the image is saved into
/// the phone's gallery instead: the phone's clipboard can't hold images
/// through scrcpy-server. Returns a message for the UI, if any.
#[tauri::command]
async fn paste_from_mac(app: AppHandle, image_only: Option<bool>) -> Res<Option<String>> {
    blocking(move || {
        let session = mirror::current(&app)?;
        if !image_only.unwrap_or(false) {
            let text = mirror::get_mac_clipboard()?;
            if !text.is_empty() {
                session.send(&control::set_clipboard(session.next_clipboard_seq(), &text, true))?;
                return Ok(None);
            }
        }
        let Some(png) = mirror::get_mac_clipboard_png()? else {
            return Ok(Some("The Mac clipboard has no text or image".into()));
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dir = "/sdcard/Pictures/DroidMirror";
        let remote = format!("{dir}/paste-{stamp}.png");
        let local = std::env::temp_dir().join("droid-mirror-paste.png");
        std::fs::write(&local, &png).map_err(|e| e.to_string())?;
        adb::run(Some(session.serial()), &["shell", "mkdir", "-p", dir])?;
        adb::run(Some(session.serial()), &["push", &local.to_string_lossy(), &remote])?;
        session.send(&control::scan_file(&remote))?;
        Ok(Some("Image saved to the phone's gallery (Pictures/DroidMirror)".into()))
    })
    .await
}

/// Phone -> Mac: copy the newest image on the phone (camera, screenshot,
/// download, chat...) to the Mac clipboard. Returns its file name.
#[tauri::command]
async fn copy_latest_image(app: AppHandle) -> Res<String> {
    blocking(move || {
        let serial = mirror::current(&app)?.serial().to_string();
        let path = adb::latest_image_path(&serial)?;
        let bytes = adb::pull_bytes(&serial, &path)?;
        let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("jpg");
        mirror::set_mac_clipboard_photo(&bytes, ext)?;
        Ok(path.rsplit('/').next().unwrap_or(&path).to_string())
    })
    .await
}

/// Copy the phone's screen (full-resolution screenshot) to the Mac clipboard.
/// Returns the image size, e.g. "1080×2340".
#[tauri::command]
async fn copy_screen(app: AppHandle) -> Res<String> {
    blocking(move || {
        let serial = mirror::current(&app)?.serial().to_string();
        let png = adb::screencap_png(&serial)?;
        mirror::set_mac_clipboard_png(&png)?;
        Ok(mirror::png_size(&png)
            .map(|(w, h)| format!("{w}×{h}"))
            .unwrap_or_default())
    })
    .await
}

/// Fallback for `copy_screen`: a PNG made in the webview from the mirrored
/// frame, as a `data:image/png;base64,...` URL.
#[tauri::command]
async fn copy_png_to_mac(data_url: String) -> Res<()> {
    blocking(move || {
        let b64 = data_url
            .strip_prefix("data:image/png;base64,")
            .ok_or("expected a PNG data URL")?;
        let png = base64_decode(b64)?;
        mirror::set_mac_clipboard_png(&png)
    })
    .await
}

fn base64_decode(input: &str) -> Res<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in input.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | val(c).ok_or("invalid base64")?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// Cmd+C: press "copy" on the device; the device sends its clipboard back and
/// the control reader writes it to the Mac clipboard.
#[tauri::command]
fn copy_to_mac(app: AppHandle) -> Res<()> {
    mirror::current(&app)?.send(&control::get_clipboard(control::COPY_KEY_COPY))
}

/// Webview timing stats, printed to the terminal running the app.
#[tauri::command]
fn log_stats(line: String) {
    eprintln!("[stats] {line} socketGapMax={}ms", mirror::take_socket_gap_ms());
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(MirrorState::default())
        .invoke_handler(tauri::generate_handler![
            list_devices,
            adb_connect,
            adb_pair,
            adb_disconnect,
            enable_wifi,
            start_mirror,
            stop_mirror,
            touch,
            scroll,
            key,
            press_key,
            inject_text,
            device_action,
            set_display_power,
            paste_from_mac,
            copy_to_mac,
            copy_screen,
            copy_latest_image,
            copy_png_to_mac,
            log_stats,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                mirror::stop(&app.state::<MirrorState>(), None, StopKind::User);
            }
        });
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_base64() {
        assert_eq!(super::base64_decode("aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
        assert_eq!(super::base64_decode("iVBORw0KGgo=").unwrap(), b"\x89PNG\r\n\x1a\n");
        assert!(super::base64_decode("**").is_err());
    }
}
