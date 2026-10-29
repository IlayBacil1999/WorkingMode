//! Temperature and CPU readings through performance counters (pdh.dll).
//! No admin: WMI's MSAcpi_ThermalZoneTemperature is denied to normal users,
//! but the same ACPI thermal zones are readable as counters. They are board
//! sensors, not the CPU die, so read them as a trend rather than a CPU temp.
//!
//! The real CPU temperature needs a kernel driver. If LibreHardwareMonitor is
//! running with its web server on, we read its numbers over localhost, so
//! this app itself still needs no admin and no driver.

use crate::engine::Readings;
use crate::win::wide;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::Performance::*;

const TEMP: &str = r"\Thermal Zone Information(*)\Temperature";
const LOAD: &str = r"\Processor Information(_Total)\% Processor Utility";
const BUSY: &str = r"\Processor Information(_Total)\% Processor Time";
const FREQ: &str = r"\Processor Information(_Total)\% of Maximum Frequency";

pub struct Sensors {
    query: isize,
    temp: Option<isize>,
    load: Option<isize>,
    busy: Option<isize>,
    freq: Option<isize>,
    lhm: Arc<LhmReader>,
}

/// LibreHardwareMonitor is read on a worker thread: a connect to a closed
/// localhost port takes ~2 s on Windows, too long to freeze the tray for.
#[derive(Default)]
struct LhmReader {
    /// LHM's id for the CPU temperature, once found.
    id: Mutex<Option<String>>,
    /// The last good reading and when it was taken.
    latest: Mutex<Option<(i32, Instant)>>,
    /// After a failed try, LHM is left alone until then.
    retry_at: Mutex<Option<Instant>>,
    /// A read is running on the worker thread.
    running: AtomicBool,
}

const LHM_RETRY: Duration = Duration::from_secs(60);
/// A reading older than this is no reading (LHM stopped answering).
const LHM_FRESH: Duration = Duration::from_secs(20);

impl Sensors {
    pub fn new() -> Option<Self> {
        let mut query = 0;
        if unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) } != 0 {
            return None;
        }
        let add = |path: &str| {
            let (p, mut c) = (wide(path), 0isize);
            (unsafe { PdhAddEnglishCounterW(query, p.as_ptr(), 0, &mut c) } == 0).then_some(c)
        };
        let s = Sensors { query, temp: add(TEMP), load: add(LOAD), busy: add(BUSY), freq: add(FREQ), lhm: Arc::default() };
        // rate counters need two samples; this is the first
        unsafe { PdhCollectQueryData(query) };
        Some(s)
    }

    /// Values since the previous call. The CPU temperature is the latest one
    /// the worker thread got; this never waits for the network.
    pub fn read(&self, lhm: &Lhm) -> Readings {
        let cpu_temp = self.lhm.poll(lhm);
        self.read_with(cpu_temp)
    }

    /// Same, but waits for LibreHardwareMonitor: for the command line.
    pub fn read_now(&self, lhm: &Lhm) -> Readings {
        let cpu_temp = self.lhm.fetch(lhm);
        self.read_with(cpu_temp)
    }

    fn read_with(&self, cpu_temp_c: Option<i32>) -> Readings {
        unsafe { PdhCollectQueryData(self.query) };
        let pct = |c: Option<isize>| c.and_then(value).map(|v| v.round().clamp(0.0, 999.0) as u32);
        Readings {
            cpu_temp_c,
            // the counter is in kelvin; with several zones, the hottest one
            temp_c: self.temp.and_then(max_value).map(|k| (k - 273.15).round() as i32),
            cpu_load: pct(self.load),
            cpu_busy: pct(self.busy),
            cpu_freq: pct(self.freq),
        }
    }
}

impl LhmReader {
    fn backing_off(&self) -> bool {
        self.retry_at.lock().unwrap().is_some_and(|t| Instant::now() < t)
    }

    /// The latest fresh reading; starts a new one in the background unless one
    /// is running or LHM failed a moment ago.
    fn poll(self: &Arc<Self>, lhm: &Lhm) -> Option<i32> {
        if lhm.port == 0 {
            return None;
        }
        if !self.backing_off() && !self.running.swap(true, Ordering::AcqRel) {
            let (me, lhm) = (self.clone(), lhm.clone());
            std::thread::spawn(move || {
                me.fetch(&lhm);
                me.running.store(false, Ordering::Release);
            });
        }
        let latest = *self.latest.lock().unwrap();
        latest.filter(|(_, at)| at.elapsed() < LHM_FRESH).map(|(t, _)| t)
    }

    /// One read on the calling thread; remembers it, or backs off on failure.
    fn fetch(&self, lhm: &Lhm) -> Option<i32> {
        if lhm.port == 0 || self.backing_off() {
            return None;
        }
        let t = self.read_lhm(lhm);
        *self.retry_at.lock().unwrap() = t.is_none().then(|| Instant::now() + LHM_RETRY);
        if let Some(t) = t {
            *self.latest.lock().unwrap() = Some((t, Instant::now()));
        }
        t
    }

    /// CPU temperature from LibreHardwareMonitor. The full sensor tree is
    /// ~50 KB, so it's fetched only to find the sensor's id; after that we ask
    /// for that one sensor. If that fails (LHM restarted, id gone), the next
    /// call looks again.
    fn read_lhm(&self, lhm: &Lhm) -> Option<i32> {
        let cached = self.id.lock().unwrap().clone();
        if let Some(id) = cached {
            if let Some(t) = lhm_sensor(lhm, &id) {
                return Some(t);
            }
            // id gone (LHM restarted?): look it up again right away, before backing off
            *self.id.lock().unwrap() = None;
        }
        let tree: Value = serde_json::from_str(&http_get(lhm, "/data.json")?).ok()?;
        let (id, t) = cpu_temp_from_json(&tree)?;
        *self.id.lock().unwrap() = id;
        Some(t)
    }
}

/// Where LibreHardwareMonitor's web server is, and how to log in to it.
#[derive(Clone)]
pub struct Lhm {
    /// 0 = don't use it.
    pub port: u16,
    /// "Basic …" header value, when its web server has Authentication on.
    pub auth: Option<String>,
}

impl Lhm {
    pub fn new(port: u16, user: &str, password: &str) -> Self {
        let auth = (!user.is_empty()).then(|| format!("Basic {}", base64(format!("{user}:{password}").as_bytes())));
        Lhm { port, auth }
    }
}

/// Why there's no CPU temperature, in words for the log and --status.
pub fn lhm_problem(lhm: &Lhm) -> &'static str {
    if lhm.port == 0 {
        return "is off (lhm_port is 0)";
    }
    match http_request(lhm, "/data.json") {
        None => "isn't running, or its web server is off",
        Some((401, _)) if lhm.auth.is_some() => "rejected the user name or password (lhm_user / lhm_password in config.json)",
        Some((401, _)) => "wants a password: set lhm_user and lhm_password in config.json",
        Some((200, _)) => "runs but shows no CPU temperature",
        Some(_) => "answered with an error",
    }
}

impl Drop for Sensors {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

fn value(counter: isize) -> Option<f64> {
    let mut v: PDH_FMT_COUNTERVALUE = unsafe { std::mem::zeroed() };
    let rc = unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, std::ptr::null_mut(), &mut v) };
    (rc == 0 && v.CStatus == 0).then(|| unsafe { v.Anonymous.doubleValue })
}

/// Largest value across the instances of a wildcard counter.
fn max_value(counter: isize) -> Option<f64> {
    let (mut size, mut count) = (0u32, 0u32);
    let rc = unsafe { PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, std::ptr::null_mut()) };
    if rc != PDH_MORE_DATA || size == 0 {
        return None;
    }
    // items followed by their names, in one buffer; u64 keeps it aligned
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
    if unsafe { PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, items) } != 0 {
        return None;
    }
    let items = unsafe { std::slice::from_raw_parts(items, count as usize) };
    items
        .iter()
        .filter(|i| i.FmtValue.CStatus == 0)
        .map(|i| unsafe { i.FmtValue.Anonymous.doubleValue })
        .filter(|k| *k > 200.0) // zones without a sensor report 0
        .reduce(f64::max)
}

/// Status code and body of a GET to LibreHardwareMonitor's web server on localhost.
fn http_request(lhm: &Lhm, path: &str) -> Option<(u16, String)> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, lhm.port));
    let timeout = Duration::from_millis(1500);
    let mut s = TcpStream::connect_timeout(&addr, timeout).ok()?;
    s.set_read_timeout(Some(timeout)).ok()?;
    let auth = lhm.auth.as_ref().map_or(String::new(), |a| format!("Authorization: {a}\r\n"));
    // HTTP/1.0: a plain body, no chunked encoding to undo
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n{auth}\r\n").as_bytes()).ok()?;
    let mut buf = Vec::new();
    // a server that keeps the socket open ends in a read timeout; what came before is still good
    let _ = s.take(4 << 20).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let status = text.split(' ').nth(1)?.parse().ok()?;
    let body = text.find("\r\n\r\n").map_or("", |i| &text[i + 4..]);
    Some((status, body.to_string()))
}

fn http_get(lhm: &Lhm, path: &str) -> Option<String> {
    http_request(lhm, path).filter(|(status, _)| *status == 200).map(|(_, body)| body)
}

/// Standard base64, for the Basic auth header.
fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// One sensor's value: {"value": 50.0, "min": …, "max": …, "format": …}.
fn lhm_sensor(lhm: &Lhm, id: &str) -> Option<i32> {
    let body = http_get(lhm, &format!("/Sensor?action=Get&id={}", encode_query(id)))?;
    let v: Value = serde_json::from_str(&body).ok()?;
    v["value"].as_f64().filter(|t| (1.0..150.0).contains(t)).map(|t| t.round() as i32)
}

/// Percent-encodes what would break a query string; ids look like "/intelcpu/0/temperature/8".
fn encode_query(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Picks the CPU temperature out of the sensor tree: "CPU Package" (Intel),
/// "Core (Tctl/Tdie)" (AMD), else the hottest core. Returns its SensorId too,
/// when the tree has them.
fn cpu_temp_from_json(root: &Value) -> Option<(Option<String>, i32)> {
    let mut temps: Vec<CpuTemp> = Vec::new();
    collect_cpu_temps(root, false, &mut temps);
    let named = |n: &str| temps.iter().find(|t| t.text.eq_ignore_ascii_case(n));
    named("CPU Package")
        .or_else(|| named("Core (Tctl/Tdie)"))
        .or_else(|| named("Core Max"))
        .or_else(|| temps.iter().max_by(|a, b| a.celsius.total_cmp(&b.celsius)))
        .map(|t| (t.id.clone(), t.celsius.round() as i32))
}

struct CpuTemp {
    text: String,
    id: Option<String>,
    celsius: f64,
}

/// Temperature sensors under the CPU node. Newer versions tag each sensor
/// with SensorId ("/intelcpu/0/temperature/..."); older ones only have a tree
/// of Text labels, so a CPU is recognised by its icon.
fn collect_cpu_temps(node: &Value, in_cpu: bool, out: &mut Vec<CpuTemp>) {
    let text = node["Text"].as_str().unwrap_or("");
    let id = node["SensorId"].as_str().unwrap_or("");
    let in_cpu = in_cpu || node["ImageURL"].as_str().is_some_and(|u| u.contains("cpu"));
    let cpu_sensor = id.contains("cpu/") && id.contains("/temperature/");
    // "Distance to TjMax" is a headroom, not a temperature
    if (cpu_sensor || (in_cpu && id.is_empty())) && !text.contains("Distance") {
        if let Some(celsius) = node["Value"].as_str().and_then(parse_celsius) {
            let id = (!id.is_empty()).then(|| id.to_string());
            out.push(CpuTemp { text: text.to_string(), id, celsius });
        }
    }
    if let Some(kids) = node["Children"].as_array() {
        for k in kids {
            collect_cpu_temps(k, in_cpu, out);
        }
    }
}

/// "52.0 °C" or "52,0 °C" (the decimal mark follows the PC's locale).
fn parse_celsius(s: &str) -> Option<f64> {
    let num = s.trim().strip_suffix("°C")?.trim().replace(',', ".");
    num.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_cpu_package_from_tagged_tree() {
        let j = serde_json::json!({"Text": "Sensor", "Children": [{"Text": "PC", "Children": [
            {"Text": "Intel Core i7-13700H", "ImageURL": "images_icon/cpu.png", "Children": [
                {"Text": "Temperatures", "Children": [
                    {"Text": "CPU Core #1", "Value": "61.0 °C", "SensorId": "/intelcpu/0/temperature/0", "Children": []},
                    {"Text": "CPU Package", "Value": "64,0 °C", "SensorId": "/intelcpu/0/temperature/8", "Children": []}
                ]},
                {"Text": "Clocks", "Children": [
                    {"Text": "CPU Core #1", "Value": "2400.0 MHz", "SensorId": "/intelcpu/0/clock/1", "Children": []}
                ]}
            ]},
            {"Text": "NVMe", "Children": [
                {"Text": "Temperature", "Value": "70.0 °C", "SensorId": "/nvme/0/temperature/0", "Children": []}
            ]}
        ]}]});
        assert_eq!(cpu_temp_from_json(&j), Some((Some("/intelcpu/0/temperature/8".into()), 64)));
    }

    /// A fake LibreHardwareMonitor: answers `n` requests, returns the paths asked for.
    /// With `auth`, requests without that Authorization header get a 401.
    fn fake_lhm(n: usize, auth: Option<&'static str>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for stream in listener.incoming().take(n) {
                let mut s = stream.unwrap();
                let mut buf = [0u8; 1024];
                let len = s.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..len]).to_string();
                let path = req.split(' ').nth(1).unwrap().to_string();
                if auth.is_some_and(|a| !req.contains(&format!("Authorization: {a}\r\n"))) {
                    let _ = write!(s, "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
                    paths.push(format!("401 {path}"));
                    continue;
                }
                let body = if path == "/data.json" {
                    r#"{"Text":"Sensor","Children":[{"Text":"CPU Package","Value":"50.0 °C","SensorId":"/intelcpu/0/temperature/8","Children":[]}]}"#
                } else {
                    r#"{"value":61.4,"min":40,"max":70,"format":"{0:F1} °C"}"#
                };
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                paths.push(path);
            }
            paths
        });
        (port, h)
    }

    #[test]
    fn finds_the_sensor_once_then_reads_only_it() {
        let (port, server) = fake_lhm(3, None);
        let (s, lhm) = (Sensors::new().unwrap(), Lhm::new(port, "", ""));
        assert_eq!(s.lhm.fetch(&lhm), Some(50));
        assert_eq!(s.lhm.fetch(&lhm), Some(61));
        assert_eq!(s.lhm.fetch(&lhm), Some(61));
        let paths = server.join().unwrap();
        assert_eq!(paths, ["/data.json", "/Sensor?action=Get&id=/intelcpu/0/temperature/8", "/Sensor?action=Get&id=/intelcpu/0/temperature/8"]);
    }

    #[test]
    fn logs_in_when_the_web_server_has_a_password() {
        // base64("wm:s3cret")
        let (port, server) = fake_lhm(4, Some("Basic d206czNjcmV0"));
        let s = Sensors::new().unwrap();
        assert_eq!(s.lhm.fetch(&Lhm::new(port, "wm", "s3cret")), Some(50));
        assert_eq!(s.lhm.fetch(&Lhm::new(port, "wm", "s3cret")), Some(61));
        assert!(lhm_problem(&Lhm::new(port, "wm", "wrong")).starts_with("rejected"));
        assert!(lhm_problem(&Lhm::new(port, "", "")).starts_with("wants a password"));
        let paths = server.join().unwrap();
        assert!(!paths[1].starts_with("401") && paths[2].starts_with("401") && paths[3].starts_with("401"));
    }

    #[test]
    fn backs_off_after_a_failed_try() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (s, lhm) = (Sensors::new().unwrap(), Lhm::new(port, "", ""));
        assert_eq!(s.lhm.fetch(&lhm), None);
        let t = Instant::now();
        assert_eq!(s.lhm.fetch(&lhm), None);
        assert!(t.elapsed() < Duration::from_millis(50), "second try should not touch the network");
    }

    #[test]
    fn reads_in_the_background_without_waiting() {
        // LHM that answers only after a second, like a closed port does
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut s = listener.incoming().next().unwrap().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            std::thread::sleep(Duration::from_millis(1000));
            let body = r#"{"Text":"Sensor","Children":[{"Text":"CPU Package","Value":"57.0 °C","SensorId":"/intelcpu/0/temperature/8","Children":[]}]}"#;
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        });
        let (s, lhm) = (Sensors::new().unwrap(), Lhm::new(port, "", ""));
        let t = Instant::now();
        assert_eq!(s.read(&lhm).cpu_temp_c, None);
        assert!(t.elapsed() < Duration::from_millis(200), "read must not wait for LHM");
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.lhm.latest.lock().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(s.read(&lhm).cpu_temp_c, Some(57));
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"wm:s3cret"), "d206czNjcmV0");
    }

    #[test]
    fn query_encoding_keeps_ids_readable() {
        assert_eq!(encode_query("/intelcpu/0/temperature/8"), "/intelcpu/0/temperature/8");
        assert_eq!(encode_query("/a b&c"), "/a%20b%26c");
    }

    #[test]
    fn untagged_tree_falls_back_to_hottest_cpu_core() {
        let j = serde_json::json!({"Text": "Sensor", "Children": [
            {"Text": "AMD Ryzen", "ImageURL": "images_icon/cpu.png", "Children": [
                {"Text": "Temperatures", "Children": [
                    {"Text": "Core #1", "Value": "55.0 °C", "Children": []},
                    {"Text": "Core #2", "Value": "58.5 °C", "Children": []}
                ]}
            ]},
            {"Text": "Disk", "ImageURL": "images_icon/hdd.png", "Children": [
                {"Text": "Temperature", "Value": "90.0 °C", "Children": []}
            ]}
        ]});
        assert_eq!(cpu_temp_from_json(&j), Some((None, 59)));
    }

    #[test]
    fn no_cpu_no_temperature() {
        assert_eq!(cpu_temp_from_json(&serde_json::json!({"Text": "Sensor", "Children": []})), None);
        assert_eq!(parse_celsius("2400.0 MHz"), None);
    }
}
