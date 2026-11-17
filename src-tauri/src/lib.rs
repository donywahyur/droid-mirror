mod adb;
mod control;
mod mirror;

use mirror::{MirrorState, StartInfo, VideoOptions};
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
    screen_off_mode: bool,
    on_packet: Channel<InvokeResponseBody>,
) -> Res<StartInfo> {
    blocking(move || {
        mirror::stop(&app.state::<MirrorState>(), None);
        mirror::start(&app, &serial, video, screen_off_mode, on_packet)
    })
    .await
}

#[tauri::command]
async fn stop_mirror(app: AppHandle) -> Res<()> {
    blocking(move || {
        mirror::stop(&app.state::<MirrorState>(), None);
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

/// Cmd+V: put the Mac clipboard on the device clipboard and paste it.
#[tauri::command]
async fn paste_from_mac(app: AppHandle) -> Res<()> {
    blocking(move || {
        let session = mirror::current(&app)?;
        let text = mirror::get_mac_clipboard()?;
        if text.is_empty() {
            return Ok(());
        }
        session.send(&control::set_clipboard(session.next_clipboard_seq(), &text, true))
    })
    .await
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
            log_stats,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                mirror::stop(&app.state::<MirrorState>(), None);
            }
        });
}
