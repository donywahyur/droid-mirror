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
