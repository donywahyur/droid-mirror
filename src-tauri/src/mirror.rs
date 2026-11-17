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
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, Manager};

const DEVICE_JAR: &str = "/data/local/tmp/droid-mirror-server.jar";
const CODEC_H264: u32 = 0x6832_3634; // "h264"
const DEVICE_NAME_LEN: usize = 64;
/// Phone screen timeout while mirroring in "screen off" mode: 30 minutes of
/// no input from the Mac still lets the phone sleep and lock.
const SCREEN_OFF_MODE_TIMEOUT_MS: u32 = 30 * 60 * 1000;
/// Android KEYCODE_SLEEP: puts the device to sleep (and so locks it).
const KEYCODE_SLEEP: u32 = 223;
/// Physical input right after mirroring starts (the hand that just unlocked
/// the phone or tapped Mirror on it) doesn't count as "using the phone".
const USE_GRACE: Duration = Duration::from_secs(3);
/// After the phone is unlocked by hand (fingerprint, PIN), its touches keep
/// coming for a moment (finger lifting, swipe-up animation): not "use" yet.
const UNLOCK_GRACE: Duration = Duration::from_secs(2);

const LOCK_UNKNOWN: u8 = 0;
const LOCK_LOCKED: u8 = 1;
const LOCK_UNLOCKED: u8 = 2;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Largest gap between video packets read from the socket since the last
/// `take_socket_gap_ms()` call. Compared with the webview's arrival gaps, it
/// shows whether bursts come from the network or from the IPC to the webview.
static SOCKET_GAP_MAX_US: AtomicU64 = AtomicU64::new(0);

pub fn take_socket_gap_ms() -> u64 {
    SOCKET_GAP_MAX_US.swap(0, Ordering::Relaxed) / 1000
}

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
    /// "Phone screen off" mode: keep the physical display off while mirroring.
    screen_off: AtomicBool,
    /// Display-off requested but not applied yet: the phone watcher applies it
    /// once the phone is awake and unlocked.
    off_pending: AtomicBool,
    started: Instant,
    /// `adb shell getevent` watching the phone's own touchscreen and buttons.
    use_watcher: Mutex<Option<Child>>,
    /// Last polled lock state (LOCK_*): locked means lock screen or asleep.
    lock_state: AtomicU8,
    /// When the phone watcher last saw it go from locked to unlocked.
    unlocked_at: Mutex<Option<Instant>>,
}

/// See `Session::physical_input_counts`.
fn input_counts(since_start: Duration, lock_state: u8, since_unlock: Option<Duration>) -> bool {
    since_start >= USE_GRACE
        && lock_state == LOCK_UNLOCKED
        && since_unlock.is_none_or(|d| d >= UNLOCK_GRACE)
}

impl Session {
    /// Turn screen-off mode on/off. Turning on is deferred to the phone
    /// watcher: on the lock screen the display must stay on, because Android
    /// puts a locked phone back to sleep after ~10 s, and asleep mirrors black.
    pub fn set_screen_off(&self, off: bool) -> Result<(), String> {
        self.screen_off.store(off, Ordering::Relaxed);
        self.off_pending.store(off, Ordering::Relaxed);
        if off {
            Ok(())
        } else {
            self.send(&control::display_power(true))
        }
    }

    /// Whether a physical touch/key now means "someone is using the phone".
    /// Not while it is locked or asleep (waking it, fingerprint, PIN all
    /// happen there), nor just after it was unlocked by hand.
    fn physical_input_counts(&self) -> bool {
        input_counts(
            self.started.elapsed(),
            self.lock_state.load(Ordering::Relaxed),
            self.unlocked_at.lock().unwrap().map(|t| t.elapsed()),
        )
    }

    pub fn send(&self, msg: &[u8]) -> Result<(), String> {
        let mut stream = self.control.lock().map_err(|e| e.to_string())?;
        stream.write_all(msg).map_err(|e| format!("control socket: {e}"))
    }

    pub fn next_clipboard_seq(&self) -> u64 {
        self.clipboard_seq.fetch_add(1, Ordering::Relaxed)
    }

    fn shutdown(&self, kind: StopKind) {
        match kind {
            // Stopped from the Mac with the phone dark: lock it rather than
            // leave an unlocked phone lying around. Turn the display back on
            // first, so scrcpy's cleanup doesn't light it up again after.
            StopKind::User if self.screen_off.load(Ordering::Relaxed) => {
                let _ = self.send(&control::display_power(true));
                let _ = self.send(&control::keycode(0, KEYCODE_SLEEP, 0, 0));
                let _ = self.send(&control::keycode(1, KEYCODE_SLEEP, 0, 0));
                thread::sleep(Duration::from_millis(250));
            }
            // Someone has the phone in hand: make sure its screen is on.
            StopKind::PhoneUsed => {
                let _ = self.send(&control::display_power(true));
                thread::sleep(Duration::from_millis(100));
            }
            _ => {}
        }
        if let Ok(mut watcher) = self.use_watcher.lock() {
            if let Some(mut child) = watcher.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
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

/// Encoder settings passed to scrcpy-server. 0 means "server default / no limit".
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct VideoOptions {
    pub max_size: u32,
    /// bits per second
    pub bit_rate: u32,
    pub max_fps: u32,
    /// MediaCodec options, e.g. "bitrate-mode=2" (CBR). Empty = encoder defaults.
    #[serde(default)]
    pub codec_options: String,
}

/// Session behavior chosen in the UI.
#[derive(Deserialize, Clone, Copy, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionOptions {
    /// Keep the phone's own screen dark while mirroring.
    #[serde(default)]
    pub screen_off_mode: bool,
    /// Stop mirroring as soon as someone touches the phone or its buttons.
    #[serde(default)]
    pub stop_on_use: bool,
}

/// Why a session ends; decides what happens to the phone's screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopKind {
    /// Stop button or app quit: lock the phone if its screen was kept off.
    User,
    /// Someone picked up the phone: leave it unlocked with the screen on.
    PhoneUsed,
    /// Connection lost, or replaced by a new session: no extra action.
    Quiet,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartInfo {
    pub device_name: String,
    pub serial: String,
}

/// Stop the current session. With `only_id`, stop it only if it is that session.
/// Returns whether a session was stopped.
pub fn stop(state: &MirrorState, only_id: Option<u64>, kind: StopKind) -> bool {
    let session = {
        let mut current = state.current.lock().unwrap();
        match (current.as_ref(), only_id) {
            (None, _) => return false,
            (Some(s), Some(id)) if s.id != id => return false,
            _ => current.take().unwrap(),
        }
    };
    session.shutdown(kind);
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
    video: VideoOptions,
    options: SessionOptions,
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

    let result = launch(app, serial, &version, scid, port, video, options, channel);
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
    video: VideoOptions,
    options: SessionOptions,
    channel: Channel<InvokeResponseBody>,
) -> Result<StartInfo, String> {
    let mut cmd = adb::command(Some(serial))?;
    cmd.arg("shell")
        .arg(format!("CLASSPATH={DEVICE_JAR}"))
        .args(["app_process", "/", "com.genymobile.scrcpy.Server", version])
        .arg(format!("scid={scid:08x}"))
        .arg(format!("max_size={}", video.max_size));
    if video.bit_rate > 0 {
        cmd.arg(format!("video_bit_rate={}", video.bit_rate));
    }
    if video.max_fps > 0 {
        cmd.arg(format!("max_fps={}", video.max_fps));
    }
    let codec_options = video.codec_options.trim();
    if !codec_options.is_empty() {
        cmd.arg(format!("video_codec_options={codec_options}"));
    }
    if options.screen_off_mode {
        // No power_off_on_close: the phone must not lock when mirroring stops
        // because someone picked it up. Locking on a user stop is done in
        // Session::shutdown instead.
        // With the display dark nobody touches the phone itself, so its own
        // screen timeout (often 30 s) would put it to sleep mid-session:
        // video freezes and the phone locks. The server raises the timeout
        // for the session and restores the user's setting on exit (cleanup).
        cmd.arg(format!("screen_off_timeout={SCREEN_OFF_MODE_TIMEOUT_MS}"));
    }
    let mut child = cmd
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
        screen_off: AtomicBool::new(false),
        off_pending: AtomicBool::new(false),
        started: Instant::now(),
        use_watcher: Mutex::new(None),
        lock_state: AtomicU8::new(LOCK_UNKNOWN),
        unlocked_at: Mutex::new(None),
    });

    *app.state::<MirrorState>().current.lock().unwrap() = Some(session.clone());

    spawn_video_reader(app.clone(), session.id, video, channel);
    spawn_device_reader(control);
    spawn_phone_watcher(app.clone(), session.clone());
    if options.stop_on_use {
        spawn_use_watcher(app.clone(), session.clone());
    }

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
        if stop(&app.state::<MirrorState>(), Some(session_id), StopKind::Quiet) {
            let _ = app.emit("mirror-closed", reason);
        }
    });
}

/// Polls the phone's lock/sleep state once a second for the whole session:
/// - emits "phone-state" on change, so the UI can say "asleep" / "locked"
///   instead of showing an unexplained black or frozen picture;
/// - in screen-off mode, turns the display off once the phone is awake and
///   unlocked, and again after every unlock (unlocking turns it back on).
///   Only on those events: a phone woken by hand with its power button stays on.
fn spawn_phone_watcher(app: AppHandle, session: Arc<Session>) {
    thread::spawn(move || {
        let mut last: Option<adb::PhoneState> = None;
        loop {
            thread::sleep(Duration::from_secs(1));
            let still_current = current(&app).map(|s| s.id == session.id).unwrap_or(false);
            if !still_current {
                break;
            }
            let Ok(state) = adb::phone_state(&session.serial) else {
                continue;
            };
            if last != Some(state) {
                let _ = app.emit("phone-state", state);
            }
            let locked = state.locked || !state.awake;
            let new = if locked { LOCK_LOCKED } else { LOCK_UNLOCKED };
            if session.lock_state.swap(new, Ordering::Relaxed) == LOCK_LOCKED && !locked {
                *session.unlocked_at.lock().unwrap() = Some(Instant::now());
            }
            let unlocked_now = last.is_some_and(|l| l.locked) && !state.locked;
            if unlocked_now && session.screen_off.load(Ordering::Relaxed) {
                session.off_pending.store(true, Ordering::Relaxed);
            }
            if state.awake && !state.locked && session.off_pending.load(Ordering::Relaxed) {
                if unlocked_now {
                    // Let the unlock animation finish, or it turns the display on after us.
                    thread::sleep(Duration::from_millis(500));
                }
                if session.send(&control::display_power(false)).is_ok() {
                    session.off_pending.store(false, Ordering::Relaxed);
                    let _ = app.emit("screen-off-applied", ());
                }
            }
            last = Some(state);
        }
    });
}

/// Stops mirroring when the phone itself is used. `getevent` reads the
/// phone's built-in touchscreen and hardware keys at the kernel level, where
/// input injected from the Mac never appears, so any event means a person is
/// touching the phone.
fn spawn_use_watcher(app: AppHandle, session: Arc<Session>) {
    thread::spawn(move || {
        let unavailable = |why: String| {
            let _ = app.emit("use-watch-unavailable", why);
        };
        let devices = match adb::physical_input_devices(&session.serial) {
            Ok(d) if !d.is_empty() => d,
            Ok(_) => return unavailable("no built-in touchscreen or buttons found".into()),
            Err(e) => return unavailable(e),
        };
        let mut cmd = match adb::command(Some(&session.serial)) {
            Ok(c) => c,
            Err(e) => return unavailable(e),
        };
        // getevent takes only one device, so read all of them and keep the
        // phone's own touchscreen/buttons, filtering on the phone so idle
        // sensors never cross the link.
        let script = format!(
            "getevent -q | grep --line-buffered -E '^({}): '",
            devices.join("|")
        );
        let mut child = match cmd
            .args(["shell", &script])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return unavailable(format!("getevent: {e}")),
        };
        let stdout = child.stdout.take();
        *session.use_watcher.lock().unwrap() = Some(child);
        // The session may have ended while getevent was starting.
        if !current(&app).map(|s| s.id == session.id).unwrap_or(false) {
            if let Some(mut c) = session.use_watcher.lock().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            return;
        }
        let Some(stdout) = stdout else { return };
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // Only real event lines ("/dev/input/eventN: type code value"),
            // never error or usage text.
            if !line.starts_with("/dev/input/") || !session.physical_input_counts() {
                continue;
            }
            if stop(&app.state::<MirrorState>(), Some(session.id), StopKind::PhoneUsed) {
                let _ = app.emit("phone-used", ());
            }
            break;
        }
    });
}

/// Forward each packet (12-byte header + payload) to the webview unchanged.
fn pump_video(video: &mut TcpStream, channel: &Channel<InvokeResponseBody>) -> Result<(), String> {
    let mut header = [0u8; 12];
    let mut last_packet: Option<Instant> = None;
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
            let now = Instant::now();
            if let Some(prev) = last_packet {
                let gap = now.duration_since(prev).as_micros() as u64;
                SOCKET_GAP_MAX_US.fetch_max(gap, Ordering::Relaxed);
            }
            last_packet = Some(now);
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

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn physical_input_rules() {
        // Unlocked, long after start and unlock: counts.
        assert!(input_counts(S(30), LOCK_UNLOCKED, None));
        assert!(input_counts(S(30), LOCK_UNLOCKED, Some(S(5))));
        // Locked or asleep: waking, fingerprint, PIN don't count.
        assert!(!input_counts(S(30), LOCK_LOCKED, None));
        // State not polled yet: don't guess.
        assert!(!input_counts(S(30), LOCK_UNKNOWN, None));
        // Just unlocked by hand: finger lifting / swipe-up don't count.
        assert!(!input_counts(S(30), LOCK_UNLOCKED, Some(Duration::from_millis(800))));
        // First seconds of the session: the hand that just started it.
        assert!(!input_counts(S(1), LOCK_UNLOCKED, None));
    }
}
