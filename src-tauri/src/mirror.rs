//! Launches scrcpy-server on the device and pumps its video stream to the webview.
//!
//! Protocol (scrcpy-server 5.0.1, forward tunnel, audio disabled):
//!   video socket:   dummy byte, 64-byte device name, u32 codec id,
//!                   then 12-byte headers, each either a session packet
//!                   (MSB set; width u32, height u32) or a media packet
//!                   (flags+pts u64, size u32) followed by `size` bytes.
//!   control socket: client -> device control messages (see control.rs),
//!                   device -> client device messages (clipboard, ...).

use crate::{adb, control};
use serde::Serialize;
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, Manager};

const DEVICE_JAR: &str = "/data/local/tmp/droid-mirror-server.jar";
const CODEC_H264: u32 = 0x6832_3634; // "h264"
const DEVICE_NAME_LEN: usize = 64;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub struct MirrorState {
    pub current: Mutex<Option<Arc<Session>>>,
}

pub struct Session {
    id: u64,
    serial: String,
    port: u16,
    control: Mutex<TcpStream>,
    video: TcpStream,
    child: Mutex<Child>,
    clipboard_seq: AtomicU64,
}

impl Session {
    pub fn send(&self, msg: &[u8]) -> Result<(), String> {
        let mut stream = self.control.lock().map_err(|e| e.to_string())?;
        stream.write_all(msg).map_err(|e| format!("control socket: {e}"))
    }

    pub fn next_clipboard_seq(&self) -> u64 {
        self.clipboard_seq.fetch_add(1, Ordering::Relaxed)
    }

    fn shutdown(&self) {
        let _ = self.video.shutdown(Shutdown::Both);
        if let Ok(c) = self.control.lock() {
            let _ = c.shutdown(Shutdown::Both);
        }
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = adb::run(
            Some(&self.serial),
            &["forward", "--remove", &format!("tcp:{}", self.port)],
        );
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartInfo {
    pub device_name: String,
    pub serial: String,
}

/// Stop the current session. With `only_id`, stop it only if it is that session.
/// Returns whether a session was stopped.
pub fn stop(state: &MirrorState, only_id: Option<u64>) -> bool {
    let session = {
        let mut current = state.current.lock().unwrap();
        match (current.as_ref(), only_id) {
            (None, _) => return false,
            (Some(s), Some(id)) if s.id != id => return false,
            _ => current.take().unwrap(),
        }
    };
    session.shutdown();
    true
}

/// Find the scrcpy-server jar and the exact version it expects.
fn locate_server() -> Result<(PathBuf, String), String> {
    let mut jars = Vec::new();
    if let Some(p) = std::env::var_os("SCRCPY_SERVER_PATH") {
        jars.push(PathBuf::from(p));
    }
    jars.push("/opt/homebrew/share/scrcpy/scrcpy-server".into());
    jars.push("/usr/local/share/scrcpy/scrcpy-server".into());
    let jar = jars.into_iter().find(|p| p.is_file()).ok_or(
        "scrcpy-server not found. Install it with: brew install scrcpy (or set SCRCPY_SERVER_PATH)",
    )?;

    let version = match std::env::var("SCRCPY_SERVER_VERSION") {
        Ok(v) => v,
        Err(_) => {
            let bin = adb::find_tool("scrcpy", &[])
                .ok_or("scrcpy binary not found; set SCRCPY_SERVER_VERSION to the server's version")?;
            let out = Command::new(bin)
                .arg("--version")
                .output()
                .map_err(|e| format!("failed to run scrcpy --version: {e}"))?;
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .nth(1) // "scrcpy 5.0.1 <https://...>"
                .ok_or("could not parse `scrcpy --version` output")?
                .to_string()
        }
    };
    Ok((jar, version))
}

fn random_scid() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
        .unwrap_or(0);
    (nanos ^ std::process::id().rotate_left(16)) & 0x7fff_ffff
}

fn free_port() -> Result<u16, String> {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("no free local port: {e}"))
}

fn pipe_logs<R: Read + Send + 'static>(
    app: AppHandle,
    stream: Option<R>,
    log: Arc<Mutex<VecDeque<String>>>,
) {
    let Some(stream) = stream else { return };
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            eprintln!("[scrcpy-server] {line}");
            let _ = app.emit("server-log", &line);
            let mut l = log.lock().unwrap();
            if l.len() >= 20 {
                l.pop_front();
            }
            l.push_back(line);
        }
    });
}

/// In forward mode, connecting succeeds as soon as `adb forward` exists, even if the
/// server is not listening yet. The server's dummy byte confirms a real connection.
fn connect_video(port: u16, child: &mut Child) -> Result<TcpStream, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("scrcpy-server exited early ({status})"));
        }
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut dummy = [0u8; 1];
            if s.read_exact(&mut dummy).is_ok() {
                return Ok(s);
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("timed out waiting for scrcpy-server".into())
}

/// Read the device name and codec id that precede the video packets.
fn read_stream_header(video: &mut TcpStream) -> Result<String, String> {
    let io_err = |e: io::Error| format!("reading stream header: {e}");
    let mut name = [0u8; DEVICE_NAME_LEN];
    video.read_exact(&mut name).map_err(io_err)?;
    let end = name.iter().position(|&b| b == 0).unwrap_or(DEVICE_NAME_LEN);
    let device_name = String::from_utf8_lossy(&name[..end]).into_owned();

    let mut codec = [0u8; 4];
    video.read_exact(&mut codec).map_err(io_err)?;
    match u32::from_be_bytes(codec) {
        CODEC_H264 => Ok(device_name),
        0 => Err("the device disabled the video stream".into()),
        1 => Err("scrcpy-server reported a configuration error".into()),
        other => Err(format!("unexpected video codec id {other:#010x}")),
    }
}

/// Push and start scrcpy-server on `serial`, connect to it, store the session in
/// `MirrorState` and start streaming packets to `channel`.
pub fn start(
    app: &AppHandle,
    serial: &str,
    max_size: u32,
    channel: Channel<InvokeResponseBody>,
) -> Result<StartInfo, String> {
    let (jar, version) = locate_server()?;
    adb::run(Some(serial), &["push", &jar.to_string_lossy(), DEVICE_JAR])?;

    let scid = random_scid();
    let port = free_port()?;
    let local = format!("tcp:{port}");
    adb::run(
        Some(serial),
        &["forward", &local, &format!("localabstract:scrcpy_{scid:08x}")],
    )?;

    let result = launch(app, serial, &version, scid, port, max_size, channel);
    if result.is_err() {
        let _ = adb::run(Some(serial), &["forward", "--remove", &local]);
    }
    result
}

fn launch(
    app: &AppHandle,
    serial: &str,
    version: &str,
    scid: u32,
    port: u16,
    max_size: u32,
    channel: Channel<InvokeResponseBody>,
) -> Result<StartInfo, String> {
    let mut child = adb::command(Some(serial))?
        .arg("shell")
        .arg(format!("CLASSPATH={DEVICE_JAR}"))
        .args(["app_process", "/", "com.genymobile.scrcpy.Server", version])
        .arg(format!("scid={scid:08x}"))
        .arg(format!("max_size={max_size}"))
        .args([
            "log_level=info",
            "tunnel_forward=true",
            "video_codec=h264",
            "audio=false",
            "control=true",
            "cleanup=true",
            "clipboard_autosync=false",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start scrcpy-server: {e}"))?;

    let log = Arc::new(Mutex::new(VecDeque::new()));
    pipe_logs(app.clone(), child.stdout.take(), log.clone());
    pipe_logs(app.clone(), child.stderr.take(), log.clone());

    let connected = (|| {
        let mut video = connect_video(port, &mut child)?;
        let control = TcpStream::connect(("127.0.0.1", port))
            .map_err(|e| format!("control socket: {e}"))?;
        let device_name = read_stream_header(&mut video)?;
        let _ = video.set_read_timeout(None);
        let _ = video.set_nodelay(true);
        let _ = control.set_nodelay(true);
        Ok::<_, String>((video, control, device_name))
    })();

    let (video, control, device_name) = match connected {
        Ok(v) => v,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            thread::sleep(Duration::from_millis(200)); // let the log threads drain
            let tail: Vec<String> = log.lock().unwrap().iter().cloned().collect();
            return Err(if tail.is_empty() {
                e
            } else {
                format!("{e}\n\nserver log:\n{}", tail.join("\n"))
            });
        }
    };

    let clone_err = |e: io::Error| format!("socket clone: {e}");
    let session = Arc::new(Session {
        id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
        serial: serial.to_string(),
        port,
        control: Mutex::new(control.try_clone().map_err(clone_err)?),
        video: video.try_clone().map_err(clone_err)?,
        child: Mutex::new(child),
        clipboard_seq: AtomicU64::new(1),
    });

    *app.state::<MirrorState>().current.lock().unwrap() = Some(session.clone());

    spawn_video_reader(app.clone(), session.id, video, channel);
    spawn_device_reader(control);

    Ok(StartInfo {
        device_name,
        serial: serial.to_string(),
    })
}

fn spawn_video_reader(
    app: AppHandle,
    session_id: u64,
    mut video: TcpStream,
    channel: Channel<InvokeResponseBody>,
) {
    thread::spawn(move || {
        let reason = match pump_video(&mut video, &channel) {
            Ok(()) => "The device closed the stream".to_string(),
            Err(e) => e,
        };
        // Stopped by the user? Then the session is already gone and nothing is emitted.
        if stop(&app.state::<MirrorState>(), Some(session_id)) {
            let _ = app.emit("mirror-closed", reason);
        }
    });
}

/// Forward each packet (12-byte header + payload) to the webview unchanged.
fn pump_video(video: &mut TcpStream, channel: &Channel<InvokeResponseBody>) -> Result<(), String> {
    let mut header = [0u8; 12];
    loop {
        if let Err(e) = video.read_exact(&mut header) {
            return match e.kind() {
                io::ErrorKind::UnexpectedEof => Ok(()),
                _ => Err(format!("video stream: {e}")),
            };
        }
        let packet = if header[0] & 0x80 != 0 {
            header.to_vec() // session packet: no payload
        } else {
            let len = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
            let mut buf = vec![0u8; 12 + len];
            buf[..12].copy_from_slice(&header);
            video
                .read_exact(&mut buf[12..])
                .map_err(|e| format!("video stream: {e}"))?;
            buf
        };
        channel
            .send(InvokeResponseBody::Raw(packet))
            .map_err(|e| format!("frontend channel closed: {e}"))?;
    }
}

/// Read device -> client messages. Clipboard content goes to the macOS clipboard.
fn spawn_device_reader(mut stream: TcpStream) {
    thread::spawn(move || {
        let mut read_msg = || -> io::Result<()> {
            let mut kind = [0u8; 1];
            stream.read_exact(&mut kind)?;
            match kind[0] {
                0 => {
                    // clipboard: u32 length + UTF-8 text
                    let mut len = [0u8; 4];
                    stream.read_exact(&mut len)?;
                    let mut text = vec![0u8; u32::from_be_bytes(len) as usize];
                    stream.read_exact(&mut text)?;
                    let _ = set_mac_clipboard(&String::from_utf8_lossy(&text));
                }
                1 => {
                    // clipboard ack: u64 sequence
                    let mut seq = [0u8; 8];
                    stream.read_exact(&mut seq)?;
                }
                2 => {
                    // uhid output: u16 id, u16 length, data
                    let mut hdr = [0u8; 4];
                    stream.read_exact(&mut hdr)?;
                    let mut data = vec![0u8; u16::from_be_bytes([hdr[2], hdr[3]]) as usize];
                    stream.read_exact(&mut data)?;
                }
                other => {
                    return Err(io::Error::other(format!("unknown device message {other}")));
                }
            }
            Ok(())
        };
        while read_msg().is_ok() {}
    });
}

pub fn get_mac_clipboard() -> Result<String, String> {
    let out = Command::new("/usr/bin/pbpaste")
        .output()
        .map_err(|e| format!("pbpaste: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn set_mac_clipboard(text: &str) -> Result<(), String> {
    let mut child = Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("pbcopy: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    }
    child.wait().map_err(|e| e.to_string())?;
    Ok(())
}

pub fn current(app: &AppHandle) -> Result<Arc<Session>, String> {
    app.state::<MirrorState>()
        .current
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "Not mirroring".to_string())
}

pub fn send_key_press(session: &Session, keycode: u32) -> Result<(), String> {
    session.send(&control::keycode(0, keycode, 0, 0))?;
    session.send(&control::keycode(1, keycode, 0, 0))
}
