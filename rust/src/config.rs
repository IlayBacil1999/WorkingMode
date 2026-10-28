//! Settings, session state, log file and the autostart key.
//! Everything lives in %LOCALAPPDATA%\WorkingMode.

use crate::engine::{AfterAction, Params};
use crate::win::{from_wide, wide};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, SYSTEMTIME};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

/// How the app puts the PC to "sleep" at the end. Modern Standby laptops may
/// not honor a programmatic sleep; the test plan settles which one works.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SleepMethod {
    Suspend,
    Hibernate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// GUID of our "Working Mode" power plan, created on first start.
    pub plan_guid: Option<String>,
    pub cpu_max: u32,
    pub turbo_off: bool,
    pub screen_off_secs: u32,
    pub unattended_zero: bool,
    pub hold_power_request: bool,
    pub end_with_lid_closed: AfterAction,
    pub sleep_method: SleepMethod,
    pub battery_floor: u8,
    pub warn_before_mins: u64,
    pub countdown_secs: u64,
    pub away_idle_secs: u64,
    pub reconnect: bool,
    pub net_check_secs: u64,
    pub reconnect_after_secs: u64,
    /// Heat guard: after 30 s at this CPU temperature the CPU limit drops to
    /// `hot_cpu_max` for at least 5 min, until it's 10 degrees cooler. 0 = off.
    /// Needs LibreHardwareMonitor; without it `hot_temp_c` on the board sensor is used.
    pub hot_cpu_temp_c: i32,
    pub hot_temp_c: i32,
    pub hot_cpu_max: u32,
    /// LibreHardwareMonitor's web server port; 0 = don't look for it.
    pub lhm_port: u16,
    /// Its web server login, when Authentication is on there. LHM keeps only a
    /// hash, so the password has to be repeated here.
    pub lhm_user: String,
    pub lhm_password: String,
    pub first_run_done: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            plan_guid: None,
            cpu_max: 90,
            turbo_off: true,
            screen_off_secs: 600,
            unattended_zero: true,
            hold_power_request: true,
            end_with_lid_closed: AfterAction::Sleep,
            sleep_method: SleepMethod::Suspend,
            battery_floor: 15,
            warn_before_mins: 10,
            countdown_secs: 60,
            away_idle_secs: 300,
            reconnect: true,
            net_check_secs: 60,
            reconnect_after_secs: 90,
            hot_cpu_temp_c: 85,
            hot_temp_c: 60,
            hot_cpu_max: 60,
            lhm_port: 8085,
            lhm_user: String::new(),
            lhm_password: String::new(),
            first_run_done: false,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let Ok(text) = fs::read_to_string(config_path()) else { return Config::default() };
        // Notepad may save with a BOM, which serde_json rejects
        match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
            Ok(c) => c,
            Err(e) => {
                // the next save would overwrite the hand edit (and plan_guid, the LHM password): keep a copy
                let bad = data_dir().join("config.json.bad");
                let _ = fs::write(&bad, &text);
                log(&format!("config.json unreadable ({e}), using defaults - your file is kept as {}", bad.display()));
                Config::default()
            }
        }
    }

    pub fn save(&self) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            write_atomic(&config_path(), &s);
        }
    }

    pub fn lhm(&self) -> crate::sensors::Lhm {
        crate::sensors::Lhm::new(self.lhm_port, &self.lhm_user, &self.lhm_password)
    }

    pub fn params(&self) -> Params {
        Params {
            end_with_lid_closed: self.end_with_lid_closed,
            battery_floor: self.battery_floor,
            warn_before_secs: self.warn_before_mins * 60,
            countdown_secs: self.countdown_secs,
            away_idle_secs: self.away_idle_secs,
            reconnect: self.reconnect,
            reconnect_after_secs: self.reconnect_after_secs,
            hot_cpu_temp_c: self.hot_cpu_temp_c,
            hot_temp_c: self.hot_temp_c,
        }
    }
}

/// What must survive a crash or reboot to put things back.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedState {
    pub previous_scheme: String,
    pub started_at: u64,
    pub ends_at: Option<u64>,
    pub wifi_profile: Option<String>,
}

impl SavedState {
    pub fn load() -> Option<Self> {
        serde_json::from_str(&fs::read_to_string(state_path()).ok()?).ok()
    }

    pub fn save(&self) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            write_atomic(&state_path(), &s);
        }
    }

    pub fn clear() {
        let _ = fs::remove_file(state_path());
    }
}

/// Write to a temp file, then rename over the target, so a crash or a dead
/// battery mid-write can't leave half a file.
fn write_atomic(path: &std::path::Path, text: &str) {
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, text).is_ok() && fs::rename(&tmp, path).is_err() {
        let _ = fs::write(path, text);
    }
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("WorkingMode");
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn state_path() -> PathBuf {
    data_dir().join("state.json")
}

pub fn log_path() -> PathBuf {
    data_dir().join("log.txt")
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn local_time() -> SYSTEMTIME {
    unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    }
}

/// Local wall-clock time `secs_from_now` seconds ahead, as "HH:MM".
pub fn clock_in(secs_from_now: u64) -> String {
    let t = local_time();
    let day = (t.wHour as u64 * 3600 + t.wMinute as u64 * 60 + t.wSecond as u64 + secs_from_now) % 86_400;
    format!("{:02}:{:02}", day / 3600, (day % 3600) / 60)
}

pub fn log(msg: &str) {
    let t = local_time();
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}  {msg}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = f.write_all(line.as_bytes());
    }
}

// ---- autostart (HKCU Run key, no admin needed)

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "WorkingMode";

pub fn autostart_enabled() -> bool {
    let (key, name) = (wide(RUN_KEY), wide(RUN_VALUE));
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size)
    };
    if rc != ERROR_SUCCESS {
        return false;
    }
    // only "enabled" if it points at this exe; a stale path counts as off
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    from_wide(&buf).trim_matches('"').eq_ignore_ascii_case(&exe)
}

pub fn set_autostart(on: bool) -> bool {
    let (key, name) = (wide(RUN_KEY), wide(RUN_VALUE));
    let rc = unsafe {
        if on {
            let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
            let data = wide(&format!("\"{exe}\""));
            RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32)
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr())
        }
    };
    rc == ERROR_SUCCESS
}
