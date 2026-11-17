//! Thin wrapper around the `adb` command-line tool.

use serde::Serialize;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

static ADB_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Find an executable. Apps launched from Finder do not inherit the shell
/// PATH, so check the usual Homebrew / Android SDK locations explicitly.
pub fn find_tool(name: &str, preferred: &[PathBuf]) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = preferred.to_vec();
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        candidates.push(PathBuf::from(dir).join(name));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join(name)));
    }
    candidates.into_iter().find(|p| p.is_file())
}

pub fn adb_path() -> Result<PathBuf, String> {
    if let Some(p) = ADB_PATH.get() {
        return Ok(p.clone());
    }
    let mut preferred = Vec::new();
    if let Some(p) = std::env::var_os("ADB") {
        preferred.push(PathBuf::from(p));
    }
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(sdk) = std::env::var_os(var) {
            preferred.push(PathBuf::from(sdk).join("platform-tools/adb"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        preferred.push(PathBuf::from(home).join("Library/Android/sdk/platform-tools/adb"));
    }
    let path = find_tool("adb", &preferred)
        .ok_or("adb not found. Install it with: brew install --cask android-platform-tools")?;
    let _ = ADB_PATH.set(path.clone());
    Ok(path)
}

pub fn command(serial: Option<&str>) -> Result<Command, String> {
    let mut cmd = Command::new(adb_path()?);
    if let Some(s) = serial {
        cmd.arg("-s").arg(s);
    }
    cmd.stdin(Stdio::null());
    Ok(cmd)
}

/// Run an adb command and return its stdout.
pub fn run(serial: Option<&str>, args: &[&str]) -> Result<String, String> {
    let out = command(serial)?
        .args(args)
        .output()
        .map_err(|e| format!("failed to run adb: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "adb {} failed: {} {}",
            args.join(" "),
            stderr.trim(),
            stdout.trim()
        ));
    }
    Ok(stdout)
}

#[derive(Serialize, Clone, Debug)]
pub struct Device {
    pub serial: String,
    /// "device", "unauthorized", "offline", ...
    pub state: String,
    pub model: Option<String>,
    pub wireless: bool,
}

/// Connected devices, plus phones advertising Android 11+ wireless debugging
/// on the LAN (via adb's mDNS discovery) that are not connected yet. Those get
/// state "available" and are connected with `connect` before mirroring.
pub fn devices() -> Result<Vec<Device>, String> {
    let mut list = connected_devices()?;
    let services = mdns_services();

    // Wireless debugging picks a new port whenever it restarts, leaving the old
    // ip:port "offline". If the same IP now advertises another port, drop it.
    let ip_of = |addr: &str| addr.rsplit_once(':').map(|(ip, _)| ip.to_string());
    list.retain(|d| {
        let stale = d.wireless
            && d.state == "offline"
            && services
                .iter()
                .any(|(_, addr)| *addr != d.serial && ip_of(addr) == ip_of(&d.serial));
        if stale {
            let _ = disconnect(&d.serial);
        }
        !stale
    });

    for (name, addr) in services {
        if list.iter().any(|d| d.serial == addr) {
            continue;
        }
        // Instance names look like "adb-<serial>-<suffix>".
        let usb_serial = name
            .strip_prefix("adb-")
            .and_then(|s| s.rsplit_once('-'))
            .map(|(serial, _)| serial.to_string());
        let model = usb_serial
            .and_then(|s| list.iter().find(|d| d.serial == s))
            .and_then(|d| d.model.clone());
        list.push(Device {
            serial: addr,
            state: "available".into(),
            model,
            wireless: true,
        });
    }
    Ok(list)
}

/// `adb mdns services` -> (instance name, ip:port) for wireless-debugging phones.
fn mdns_services() -> Vec<(String, String)> {
    match run(None, &["mdns", "services"]) {
        Ok(out) => parse_mdns_services(&out),
        Err(_) => Vec::new(),
    }
}

fn parse_mdns_services(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter(|l| l.contains("_adb-tls-connect._tcp"))
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let name = parts.next()?.to_string();
            let addr = parts.last()?.to_string();
            addr.contains(':').then_some((name, addr))
        })
        .collect()
}

fn connected_devices() -> Result<Vec<Device>, String> {
    let out = run(None, &["devices", "-l"])?;
    Ok(out
        .lines()
        .skip(1) // "List of devices attached"
        .filter(|l| !l.trim().is_empty() && !l.starts_with('*'))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            let model = parts
                .find_map(|p| p.strip_prefix("model:"))
                .map(|m| m.replace('_', " "));
            let wireless = serial.contains(':') || serial.contains("._adb-tls-connect.");
            Some(Device { serial, state, model, wireless })
        })
        .collect())
}

/// Input devices that mean "someone is using the phone itself": its built-in
/// touchscreen and hardware keys. Events injected from the Mac go through
/// Android's InputManager and never reach these kernel devices.
///
/// Skipped: Bluetooth/USB peripherals (a watch or keyboard, bus 0005/0003) and
/// sensors (proximity, light, cover) that fire without anyone touching the phone.
pub fn physical_input_devices(serial: &str) -> Result<Vec<String>, String> {
    let out = run(Some(serial), &["shell", "getevent -il"])?;
    Ok(parse_physical_input_devices(&out))
}

fn parse_physical_input_devices(getevent_il: &str) -> Vec<String> {
    const WANTED: [&str; 4] = ["ABS_MT_POSITION_X", "KEY_POWER", "KEY_VOLUMEUP", "KEY_VOLUMEDOWN"];
    let mut result = Vec::new();
    let mut flush = |path: &Option<String>, bus: &Option<String>, wanted: bool| {
        if let (Some(path), true) = (path, wanted) {
            let external = matches!(bus.as_deref(), Some("0003") | Some("0005"));
            if !external {
                result.push(path.clone());
            }
        }
    };
    let (mut path, mut bus, mut wanted) = (None::<String>, None::<String>, false);
    for line in getevent_il.lines() {
        if let Some(rest) = line.strip_prefix("add device ") {
            flush(&path, &bus, wanted);
            path = rest.split_once(": ").map(|(_, p)| p.trim().to_string());
            bus = None;
            wanted = false;
        } else if let Some(b) = line.trim().strip_prefix("bus:") {
            bus = Some(b.trim().to_string());
        } else if WANTED.iter().any(|w| line.contains(w)) {
            wanted = true;
        }
    }
    flush(&path, &bus, wanted);
    result
}

/// IPv4 address of the device's Wi-Fi interface.
pub fn wifi_ip(serial: &str) -> Result<String, String> {
    let routes = run(Some(serial), &["shell", "ip", "route"])?;
    for line in routes.lines().filter(|l| l.contains("wlan")) {
        let mut words = line.split_whitespace().skip_while(|w| *w != "src");
        if let (Some(_), Some(ip)) = (words.next(), words.next()) {
            return Ok(ip.to_string());
        }
    }
    let addr = run(Some(serial), &["shell", "ip", "-f", "inet", "addr", "show", "wlan0"])?;
    for line in addr.lines() {
        if let Some(rest) = line.trim().strip_prefix("inet ") {
            if let Some(ip) = rest.split('/').next() {
                return Ok(ip.to_string());
            }
        }
    }
    Err("Could not find the device's Wi-Fi IP address. Is Wi-Fi turned on?".into())
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhoneState {
    /// Lock screen is showing.
    pub locked: bool,
    /// Screen awake (not dozing/asleep). An asleep phone mirrors as black.
    pub awake: bool,
}

/// Lock and sleep state in one adb round trip. `grep` runs on the phone so
/// only two short lines cross the (possibly slow) link.
pub fn phone_state(serial: &str) -> Result<PhoneState, String> {
    let out = run(
        Some(serial),
        &[
            "shell",
            "dumpsys activity activities | grep -m1 -o 'isKeyguardShowing=[a-z]*'; \
             dumpsys power | grep -m1 -o 'mWakefulness=[A-Za-z]*'",
        ],
    )?;
    parse_phone_state(&out)
}

fn parse_phone_state(out: &str) -> Result<PhoneState, String> {
    let mut locked = None;
    let mut awake = None;
    for line in out.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("isKeyguardShowing=") {
            locked = Some(v == "true");
        } else if let Some(v) = line.strip_prefix("mWakefulness=") {
            awake = Some(v == "Awake");
        }
    }
    match (locked, awake) {
        (Some(locked), Some(awake)) => Ok(PhoneState { locked, awake }),
        _ => Err(format!("unexpected phone state output: {out:?}")),
    }
}

/// `adb connect host:port`. adb exits 0 even on failure, so check the text.
pub fn connect(addr: &str) -> Result<String, String> {
    let out = run(None, &["connect", addr])?;
    let msg = out.trim().to_string();
    if msg.contains("connected to") {
        Ok(msg)
    } else {
        Err(msg)
    }
}

/// `adb pair host:port code` (Android 11+ wireless debugging).
pub fn pair(addr: &str, code: &str) -> Result<String, String> {
    let out = run(None, &["pair", addr, code])?;
    let msg = out.trim().to_string();
    if msg.contains("Successfully paired") {
        Ok(msg)
    } else {
        Err(msg)
    }
}

pub fn disconnect(addr: &str) -> Result<String, String> {
    run(None, &["disconnect", addr]).map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed `getevent -il` from a Galaxy S24 FE with a Bluetooth watch paired.
    const GETEVENT_IL: &str = "\
add device 1: /dev/input/event10
  bus:      0005
  name:     \"HUAWEI WATCH FIT 5 Pro-28E\"
                ABS_MT_POSITION_X     : value 0, min 0, max 1079
add device 2: /dev/input/event9
  bus:      0005
  name:     \"HUAWEI WATCH FIT 5 Pro-28E Consumer Control\"
    KEY (0001): KEY_MUTE              KEY_VOLUMEDOWN        KEY_VOLUMEUP
add device 3: /dev/input/event8
  bus:      0018
  name:     \"sec_touchproximity\"
add device 4: /dev/input/event7
  bus:      0018
  name:     \"sec_touchpad\"
                ABS_MT_POSITION_X     : value 0, min 0, max 4095
add device 5: /dev/input/event1
  bus:      0018
  name:     \"s2mps27-power-keys\"
    KEY (0001): KEY_VOLUMEDOWN        KEY_POWER
add device 6: /dev/input/event0
  bus:      0019
  name:     \"gpio_keys\"
    KEY (0001): KEY_VOLUMEUP
add device 8: /dev/input/event6
  bus:      0018
  name:     \"sec_touchscreen\"
                ABS_MT_POSITION_X     : value 0, min 0, max 4095
add device 10: /dev/input/event4
  bus:      0000
  name:     \"hall\"
";

    #[test]
    fn picks_built_in_touch_and_keys_only() {
        assert_eq!(
            parse_physical_input_devices(GETEVENT_IL),
            vec!["/dev/input/event7", "/dev/input/event1", "/dev/input/event0", "/dev/input/event6"]
        );
    }

    #[test]
    fn parses_phone_state() {
        assert_eq!(
            parse_phone_state("isKeyguardShowing=true\n  mWakefulness=Dozing\n"),
            Ok(PhoneState { locked: true, awake: false })
        );
        assert_eq!(
            parse_phone_state("isKeyguardShowing=false\nmWakefulness=Awake\n"),
            Ok(PhoneState { locked: false, awake: true })
        );
        assert!(parse_phone_state("isKeyguardShowing=false\n").is_err());
    }

    #[test]
    fn parses_mdns_services() {
        let out = "List of discovered mdns services\n\
                   adb-RRCY507DB0Y-ID0FJZ\t_adb-tls-connect._tcp\t192.168.100.125:41933\n\
                   adb-RRCY507DB0Y-ID0FJZ\t_adb-tls-pairing._tcp\t192.168.100.125:37011\n";
        assert_eq!(
            parse_mdns_services(out),
            vec![("adb-RRCY507DB0Y-ID0FJZ".to_string(), "192.168.100.125:41933".to_string())]
        );
    }
}
