//! The tray app: hidden window, tray icon + menu, balloon notifications,
//! lid events, and the glue that turns engine actions into system calls.

use crate::claude;
use crate::config::{self, autostart_enabled, clock_in, log, now, set_autostart, Config, SavedState};
use crate::engine::{fmt_span, Action, AfterAction, EndReason, Engine, HeatSource, Inputs, Readings};
use crate::power::{self, PowerRequest, GUID_CONSOLE_DISPLAY_STATE, GUID_LIDSWITCH_STATE_CHANGE, SCHEME_BALANCED};
use crate::sensors::Sensors;
use crate::win::{fill, guid_eq, guid_from_str, guid_to_string, opt_guid_eq, wide};
use crate::wifi;
use std::cell::RefCell;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::ptr::{null, null_mut};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::{RegisterPowerSettingNotification, POWERBROADCAST_SETTING};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const WM_NET: u32 = WM_APP + 2;
/// Sent by `working-mode.exe --extend/--stop`: wparam = REMOTE_*, lparam = seconds.
pub const WM_REMOTE: u32 = WM_APP + 3;
pub const REMOTE_EXTEND: usize = 1;
pub const REMOTE_STOP: usize = 2;
/// From --restore: end now, never sleep.
pub const REMOTE_END: usize = 3;
pub const WINDOW_CLASS: &str = "WorkingModeWindow";
const TIMER_ID: usize = 1;
const TICK_MS: u32 = 5_000;
/// An internet check (two 5 s probes plus DNS) that takes longer than this is presumed hung.
const NET_CHECK_GIVE_UP_SECS: u64 = 120;
pub const MUTEX_NAME: &str = "Local\\WorkingModeTray";

const PRESETS: [(&str, u64); 6] = [
    ("15 minutes", 900),
    ("1 hour", 3600),
    ("2 hours", 7200),
    ("4 hours", 14_400),
    ("8 hours", 28_800),
    ("until I stop it", 0),
];
const CPU_CHOICES: [(u32, &str); 4] =
    [(100, "100% - full speed"), (90, "90% (recommended)"), (80, "80% - cooler"), (70, "70% - coolest, slower")];
const END_CHOICES: [(AfterAction, &str); 3] = [
    (AfterAction::Sleep, "Sleep, like a normal lid close (recommended)"),
    (AfterAction::Hibernate, "Hibernate"),
    (AfterAction::Nothing, "Stay on"),
];
const BATTERY_CHOICES: [(u8, &str); 4] =
    [(10, "Stop at 10%"), (15, "Stop at 15% (recommended)"), (25, "Stop at 25%"), (0, "Off")];

const ID_START: usize = 100;
const ID_START_CLAUDE: usize = 150;
const ID_CLAUDE_SETUP: usize = 151;
const ID_CLAUDE_REMOVE: usize = 152;
const ID_EXTEND: usize = 200;
const ID_STOP: usize = 201;
const ID_KEEP: usize = 202;
const ID_CPU: usize = 300; // + percent (370..=400)
const ID_TURBO: usize = 410;
const ID_END: usize = 420;
const ID_BATTERY: usize = 430;
const ID_RECONNECT: usize = 440;
const ID_AUTOSTART: usize = 441;
const ID_FIX_PROFILE: usize = 442;
const ID_LOG: usize = 450;
const ID_CONFIG: usize = 451;
const ID_EXIT: usize = 499;

struct App {
    hwnd: HWND,
    cfg: Config,
    engine: Engine,
    plan: Option<GUID>,
    previous: Option<GUID>,
    request: Option<PowerRequest>,
    lid_closed: Option<bool>,
    /// Screen: 0 off, 1 on, 2 dimmed; `None` until Windows says.
    display: Option<u32>,
    wifi: Option<wifi::Connection>,
    /// Critical settings that didn't apply: not safe to close the lid.
    problems: Vec<String>,
    icons: Icons,
    last_net_check: u64,
    /// When the running internet check started.
    net_busy_since: Option<u64>,
    sensors: Option<Sensors>,
    /// Last Wi-Fi network seen, to log switches.
    ssid: Option<String>,
    /// What the heat guard went by on the last tick, to log when it changes.
    heat_source: Option<HeatSource>,
    /// The last tick's readings, for the menu.
    readings: Readings,
    /// When this app (re)started the session, and whether we already said
    /// there's no CPU temperature.
    started: u64,
    no_temp_notified: bool,
    plan_drift_notified: bool,
}

struct Icons {
    on: HICON,
    off: HICON,
    warn: HICON,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: RefCell<u32> = const { RefCell::new(0) };
}

/// Runs `f` on the app unless it's already borrowed (re-entrant message).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

/// Full path of a Windows tool, so a same-named exe next to ours can't stand in for it.
fn system32(exe: &str) -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    std::path::Path::new(&root).join("System32").join(exe)
}

/// Runs a Windows tool from System32 without a console window.
pub fn run_hidden(exe: &str, args: &[&str]) -> std::io::Result<bool> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new(system32(exe)).args(args).creation_flags(CREATE_NO_WINDOW).status().map(|s| s.success())
}

fn open_in_notepad(path: &std::path::Path) {
    let notepad = system32("notepad.exe").display().to_string();
    // quoted: the profile path may contain spaces
    let (op, file, notepad) = (wide("open"), wide(&format!("\"{}\"", path.display())), wide(&notepad));
    unsafe { ShellExecuteW(null_mut(), op.as_ptr(), notepad.as_ptr(), file.as_ptr(), null(), SW_SHOWNORMAL) };
}

fn confirm(text: &str) -> bool {
    let (t, c) = (wide(text), wide("Working Mode"));
    unsafe { MessageBoxW(null_mut(), t.as_ptr(), c.as_ptr(), MB_YESNO | MB_ICONQUESTION) == IDYES }
}

fn message_box(text: &str) {
    let (t, c) = (wide(text), wide("Working Mode"));
    unsafe { MessageBoxW(null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONINFORMATION) };
}

// ---------------------------------------------------------------- icons

/// Draws a 32x32 tray icon: a colored disc with a white glyph.
fn make_icon(rgb: (u8, u8, u8), glyph: fn(f32, f32) -> bool) -> HICON {
    const N: usize = 32;
    const SS: usize = 4; // supersampling per axis
    let mut px = vec![0u32; N * N];
    for y in 0..N {
        for x in 0..N {
            let (mut disc, mut mark) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let fy = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    if (fx - 16.0).powi(2) + (fy - 16.0).powi(2) <= 15.0f32.powi(2) {
                        disc += 1;
                        if glyph(fx, fy) {
                            mark += 1;
                        }
                    }
                }
            }
            let total = (SS * SS) as u32;
            let blend = |c: u8| ((c as u32 * (disc - mark) + 255 * mark) / disc.max(1)) as u8;
            let a = disc * 255 / total;
            px[y * N + x] = (a << 24) | ((blend(rgb.0) as u32) << 16) | ((blend(rgb.1) as u32) << 8) | blend(rgb.2) as u32;
        }
    }
    unsafe {
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = N as i32;
        bmi.bmiHeader.biHeight = -(N as i32); // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut bits = null_mut();
        let color = CreateDIBSection(null_mut(), &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if !bits.is_null() {
            std::ptr::copy_nonoverlapping(px.as_ptr(), bits.cast::<u32>(), N * N);
        }
        let mask_bits = vec![0u8; N * N / 8];
        let mask = CreateBitmap(N as i32, N as i32, 1, 1, mask_bits.as_ptr().cast());
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

fn glyph_play(x: f32, y: f32) -> bool {
    // triangle (12,8) (24,16) (12,24)
    x >= 12.0 && y >= 8.0 + (x - 12.0) * 8.0 / 12.0 && y <= 24.0 - (x - 12.0) * 8.0 / 12.0
}

fn glyph_pause(x: f32, y: f32) -> bool {
    (9.0..=24.0).contains(&y) && ((10.0..=14.0).contains(&x) || (18.0..=22.0).contains(&x))
}

fn glyph_bang(x: f32, y: f32) -> bool {
    (14.0..=18.0).contains(&x) && ((7.0..=19.0).contains(&y) || (21.5..=25.0).contains(&y))
}

// ---------------------------------------------------------------- tray

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = hwnd;
    d.uID = 1;
    d
}

impl App {
    fn add_tray_icon(&self) {
        let mut d = tray_data(self.hwnd);
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = self.icons.off;
        fill(&mut d.szTip, "Working Mode: off");
        unsafe { Shell_NotifyIconW(NIM_ADD, &d) };
        self.update_tray();
    }

    fn update_tray(&self) {
        let mut d = tray_data(self.hwnd);
        d.uFlags = NIF_ICON | NIF_TIP;
        let (icon, tip) = match &self.engine.session {
            None => (self.icons.off, "Working Mode: off".to_string()),
            Some(_) if !self.problems.is_empty() => (self.icons.warn, "Working Mode: ON - NOT safe to close the lid".into()),
            Some(s) if s.while_claude => (self.icons.on, format!("Working Mode: ON - {}", self.claude_status())),
            Some(_) => match self.engine.secs_left(now()) {
                Some(left) => (self.icons.on, format!("Working Mode: ON - {} left", fmt_span(left))),
                None => (self.icons.on, "Working Mode: ON - until you stop it".into()),
            },
        };
        d.hIcon = icon;
        fill(&mut d.szTip, &tip);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) };
    }

    fn notify(&self, title: &str, body: &str, warning: bool) {
        log(&format!("[notice] {title} - {body}"));
        let mut d = tray_data(self.hwnd);
        d.uFlags = NIF_INFO;
        d.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
        fill(&mut d.szInfoTitle, title);
        fill(&mut d.szInfo, body);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) };
    }

    // ------------------------------------------------------------ session

    fn inputs(&self) -> Inputs {
        Inputs {
            now: now(),
            lid_closed: self.lid_closed,
            display: self.display,
            idle_secs: power::idle_secs(),
            battery: power::battery(),
            readings: self.sensors.as_ref().map(|s| s.read(&self.cfg.lhm())).unwrap_or_default(),
            claude_busy: if self.engine.session.as_ref().is_some_and(|s| s.while_claude) {
                claude::busy_sessions(&claude::dir(), now())
            } else {
                0
            },
        }
    }

    fn save_state(&self) {
        match (&self.engine.session, &self.previous) {
            (Some(s), Some(prev)) => SavedState {
                previous_scheme: guid_to_string(prev),
                started_at: s.started_at,
                ends_at: s.ends_at,
                wifi_profile: self.wifi.as_ref().map(|w| w.profile.clone()),
                while_claude: s.while_claude,
            }
            .save(),
            // no session, but a plan to put back (starting, or a restore that failed):
            // an already-expired record, so a crash or restart restores it
            (None, Some(prev)) => SavedState {
                previous_scheme: guid_to_string(prev),
                started_at: now(),
                ends_at: Some(now()),
                wifi_profile: None,
                while_claude: false,
            }
            .save(),
            (_, None) => SavedState::clear(),
        }
    }

    /// Applies the plan to our power scheme, activates it, and reads it back.
    fn apply(&mut self) -> Result<(), String> {
        let plan = power::ensure_plan(&mut self.cfg)?;
        let prev = self.previous.unwrap_or(SCHEME_BALANCED);
        let cfg = if self.engine.cooling {
            Config { cpu_max: self.cfg.cpu_max.min(self.cfg.hot_cpu_max), ..self.cfg.clone() }
        } else {
            self.cfg.clone()
        };
        // the CPU limits are for heat in the bag: with the lid open the user is
        // there and the PC runs as usual, unless the heat guard is cooling it
        let hold_cpu = self.engine.cooling || self.lid_closed != Some(false);
        let unwritten = power::apply_plan(&plan, &cfg, &prev, hold_cpu, self.lid_closed == Some(true));
        if !unwritten.is_empty() {
            log(&format!("could not write: {}", unwritten.join(", ")));
        }
        power::set_active(&plan)?;
        self.plan = Some(plan);
        self.problems = power::verify_plan(&plan, &self.cfg);
        Ok(())
    }

    /// `secs = None` runs until stopped. `previous` is set when resuming after a crash.
    /// `while_claude`: no end time; ends once Claude Code has been quiet.
    fn start(&mut self, secs: Option<u64>, while_claude: bool, previous: Option<GUID>, resumed: bool) {
        // read before the state is rewritten below
        let saved_profile = if resumed { SavedState::load().and_then(|s| s.wifi_profile) } else { None };
        let active = power::active_scheme().ok();
        let plan = self.cfg.plan_guid.as_deref().and_then(guid_from_str);
        let prev = previous.or(active).filter(|g| !opt_guid_eq(Some(*g), plan)).unwrap_or(SCHEME_BALANCED);
        self.previous = Some(prev);
        // record the plan to go back to before switching, in case we die in between
        self.save_state();
        if let Err(e) = self.apply() {
            self.previous = None;
            self.save_state();
            self.notify("Couldn't start Working Mode", &e, true);
            return;
        }
        if !crate::watchdog::install() {
            log("couldn't create the watchdog task - if the app dies, nothing restarts it");
        }
        self.request = if self.cfg.hold_power_request { PowerRequest::new() } else { None };
        // after a reboot Wi-Fi may not be up yet: fall back to the saved profile
        self.wifi = wifi::current().or_else(|| {
            let profile = saved_profile?;
            Some(wifi::Connection { ssid: profile.clone(), profile, signal: 0 })
        });
        self.ssid = self.wifi.as_ref().map(|w| w.ssid.clone());
        if while_claude {
            self.engine.start_while_claude(now());
        } else {
            self.engine.start(now(), secs);
        }
        self.last_net_check = 0;
        self.plan_drift_notified = false;
        self.heat_source = None;
        self.started = now();
        self.no_temp_notified = false;
        self.save_state();
        log(&format!(
            "session {} - {} - previous plan {}, wifi {}, power request {}",
            if resumed { "resumed" } else { "started" },
            if while_claude { "while Claude works".into() } else { secs.map_or("until stopped".into(), fmt_span) },
            guid_to_string(&prev),
            self.wifi.as_ref().map_or("none".into(), |w| format!("'{}' (profile '{}', signal {}%)", w.ssid, w.profile, w.signal)),
            if self.request.is_some() { "held" } else { "off" },
        ));

        if !self.problems.is_empty() {
            self.notify(
                "Working Mode is on - do NOT close the lid",
                &format!("These settings didn't apply: {}. See the log.", self.problems.join(", ")),
                true,
            );
        } else {
            let until = match secs {
                _ if while_claude => format!(
                    "while Claude works, until it has been quiet for {}",
                    fmt_span(self.engine.params.claude_quiet_secs)
                ),
                Some(s) => format!("until {}", clock_in(s)),
                None => "until you stop it".into(),
            };
            let mut body = format!("You can close the lid. The PC keeps working {until}.");
            if let Some(w) = &self.wifi {
                body += &format!(" Staying on '{}'.", w.ssid);
                if !resumed && self.cfg.reconnect && wifi::profile_mode(&w.profile).as_deref() == Some("manual") {
                    body += " Tip: this network won't reconnect by itself - fix it in Settings.";
                }
            }
            let title = if resumed { "Working Mode resumed" } else { "Working Mode is on" };
            self.notify(title, &body, false);
        }
        self.update_tray();
    }

    fn restore(&mut self, reason: EndReason) {
        let prev = self.previous.take().filter(power::scheme_exists).unwrap_or(SCHEME_BALANCED);
        self.request = None;
        self.engine.cooling = false;
        self.problems.clear();
        if let Err(e) = power::set_active(&prev) {
            // keep state.json so the next start or --restore can still put it back
            self.previous = Some(prev);
            self.save_state();
            log(&format!("session ended ({reason:?}) but restoring failed: {e}"));
            self.notify("Couldn't restore power settings", &format!("{e}. Run working-mode.exe --restore."), true);
            self.update_tray();
            return;
        }
        self.save_state();
        crate::watchdog::remove();
        log(&format!("session ended ({reason:?}) - restored plan {}", guid_to_string(&prev)));
        let body = match reason {
            EndReason::Timer => "Time's up. Normal power settings are back.",
            EndReason::User => "Normal power settings are back.",
            EndReason::Battery => "Battery is low. Normal power settings are back.",
            EndReason::ClaudeDone => "Claude is done. Normal power settings are back.",
        };
        self.notify("Working Mode is off", body, false);
        self.update_tray();
    }

    fn run_actions(&mut self, acts: Vec<Action>) {
        for a in acts {
            match a {
                Action::Notify { title, body, warning } => self.notify(&title, &body, warning),
                Action::Log(m) => log(&m),
                Action::Restore(r) => self.restore(r),
                Action::Suspend(after) => {
                    power::suspend(after == AfterAction::Hibernate || self.cfg.sleep_method == config::SleepMethod::Hibernate)
                }
                Action::Reconnect => {
                    if let Some(profile) = self.wifi.as_ref().map(|w| w.profile.clone()) {
                        std::thread::spawn(move || {
                            let ok = wifi::reconnect(&profile);
                            log(&format!("reconnect to '{profile}': {}", if ok { "requested" } else { "FAILED" }));
                        });
                    } else {
                        log("offline, but there was no Wi-Fi at start to reconnect to");
                    }
                }
                Action::Cool(on) => {
                    if let Err(e) = self.apply() {
                        log(&format!("couldn't change the CPU limit: {e}"));
                    } else {
                        match (on, self.lid_closed) {
                            (true, _) => log(&format!("CPU limit now {}%", self.cfg.cpu_max.min(self.cfg.hot_cpu_max))),
                            (false, Some(false)) => log("CPU back to normal (lid open)"),
                            (false, _) => log(&format!("CPU limit now {}%", self.cfg.cpu_max)),
                        }
                    }
                }
            }
        }
        self.save_state();
        self.update_tray();
    }

    fn tick(&mut self) {
        // off: nothing to watch, so don't spend the UI thread on sensors
        if !self.engine.is_on() {
            return;
        }
        let inp = self.inputs();
        let acts = self.engine.tick(&inp);
        self.run_actions(acts);
        self.readings = inp.readings;
        if self.engine.is_on() {
            self.log_heat_source();
            self.keep_plan_active();
            self.follow_wifi();
        }
        // a check that never answered (e.g. DNS hung) must not stop all later checks
        let busy = self.net_busy_since.is_some_and(|t| inp.now.saturating_sub(t) < NET_CHECK_GIVE_UP_SECS);
        if self.engine.is_on() && !busy && inp.now.saturating_sub(self.last_net_check) >= self.cfg.net_check_secs {
            if self.net_busy_since.is_some() {
                log("internet check gave no answer - starting a new one");
            }
            self.net_busy_since = Some(inp.now);
            let hwnd = self.hwnd as isize;
            std::thread::spawn(move || {
                let ok = wifi::internet_ok();
                unsafe { PostMessageW(hwnd as HWND, WM_NET, ok as usize, 0) };
            });
        }
    }

    /// Logs what the heat guard goes by whenever that changes. The first
    /// CPU temperature arrives a tick or two after the start (it's read in
    /// the background), so a session may begin with "load" and move to "CPU".
    fn log_heat_source(&mut self) {
        let by = self.engine.heat_source(&self.readings);
        // LHM gets time to answer (it may still be starting after a reboot, and a
        // failed try waits a minute) before we say it's missing, once a session
        let waited = now().saturating_sub(self.started) >= 90;
        if by != HeatSource::Cpu && self.cfg.lhm_port > 0 && self.cfg.hot_cpu_temp_c > 0 && !self.no_temp_notified && waited {
            self.no_temp_notified = true;
            let fallback = if self.cfg.hot_load_pct > 0 { "goes by CPU load" } else { "has nothing to go by" };
            self.notify(
                "No CPU temperature",
                &format!("LibreHardwareMonitor isn't answering (not running, or its web server is off). Until it does, the heat guard {fallback}."),
                true,
            );
        }
        if self.heat_source == Some(by) {
            return;
        }
        let first = self.heat_source.is_none();
        self.heat_source = Some(by);
        log(match by {
            HeatSource::Cpu => "heat guard: CPU temperature (LibreHardwareMonitor)",
            HeatSource::Board => "heat guard: board sensor (no CPU temperature)",
            // at the start LHM's first answer may still be on its way
            HeatSource::Load if first => "heat guard: CPU load with the lid closed, until a temperature shows up",
            HeatSource::Load => "heat guard: CPU temperature lost - going by CPU load with the lid closed",
        });
    }

    /// Logs Wi-Fi switches.
    fn follow_wifi(&mut self) {
        let ssid = wifi::current().map(|c| c.ssid);
        if ssid != self.ssid {
            let name = |s: &Option<String>| s.as_deref().map_or("none".into(), |s| format!("'{s}'"));
            log(&format!("wi-fi: {} -> {}", name(&self.ssid), name(&ssid)));
            self.ssid = ssid;
        }
    }

    /// The network a reconnect goes back to is the last one that had internet:
    /// the user (or Ferry) may move to another one mid-session, but a network
    /// Windows joins on its own after a drop isn't trusted until it works.
    fn remember_working_wifi(&mut self) {
        let Some(conn) = wifi::current() else { return };
        if self.wifi.as_ref().is_some_and(|w| w.profile == conn.profile) {
            return;
        }
        log(&format!("reconnect target is now '{}' (it has internet)", conn.ssid));
        self.wifi = Some(conn);
    }

    /// The menu's heat line while a session is on.
    fn heat_line(&self) -> String {
        let r = &self.readings;
        let now = match self.engine.heat_source(r) {
            HeatSource::Cpu => format!("CPU {}C", r.cpu_temp_c.unwrap_or(0)),
            HeatSource::Board => format!("sensor {}C", r.temp_c.unwrap_or(0)),
            HeatSource::Load => format!("no temperature sensor, CPU busy {:.0}% avg", self.engine.busy_avg()),
        };
        if self.engine.cooling {
            format!("Heat: COOLING, CPU held at {}% | {now}", self.cfg.cpu_max.min(self.cfg.hot_cpu_max))
        } else {
            format!("Heat: ok | {now}")
        }
    }

    /// "N Claude sessions busy" or "Claude quiet for 3 min (ends at 10)".
    fn claude_status(&self) -> String {
        let t = now();
        let quiet = fmt_span(self.engine.params.claude_quiet_secs);
        match self.engine.claude_quiet_for(t) {
            Some(q) => format!("Claude quiet for {} (ends at {quiet})", fmt_span(q)),
            None => "while Claude works".into(),
        }
    }

    /// Our hooks are in Claude Code's settings and point at this exe.
    fn claude_hooks(&self) -> bool {
        let exe = std::env::current_exe().ok();
        claude::settings_path().zip(exe).is_some_and(|(p, exe)| claude::hooks_installed(&p, &exe))
    }

    fn set_claude_hooks(&mut self, on: bool) {
        let exe = std::env::current_exe().ok();
        let result = match (claude::settings_path(), &exe) {
            (Some(path), Some(exe)) => claude::set_hooks(&path, on.then_some(exe.as_path())),
            _ => Err("can't find the user profile or this exe".into()),
        };
        match result {
            Ok(()) if on => {
                log("added the Claude Code hooks");
                self.notify("Claude Code hooks added", "Choose \"Start while Claude works\" in the menu.", false);
            }
            Ok(()) => log("removed the Claude Code hooks"),
            Err(e) => {
                log(&format!("Claude Code hooks: {e}"));
                self.notify("Couldn't change Claude Code's settings", &e, true);
            }
        }
    }

    /// Something else (Control Panel, another tool) switched the power plan
    /// during a session: with the normal plan back, closing the lid sleeps
    /// the PC. Switch back and say so once.
    fn keep_plan_active(&mut self) {
        if self.engine.countdown.is_some() {
            return;
        }
        let (Some(plan), Ok(active)) = (self.plan, power::active_scheme()) else { return };
        if guid_eq(&plan, &active) {
            return;
        }
        log(&format!("power plan was switched to {} by something else - switching back", guid_to_string(&active)));
        if let Err(e) = self.apply() {
            log(&format!("couldn't switch back: {e}"));
        }
        if !self.plan_drift_notified {
            self.plan_drift_notified = true;
            self.notify("Power plan was changed", "Something switched the power plan. Working Mode switched it back.", true);
        }
    }

    fn on_net(&mut self, online: bool) {
        self.net_busy_since = None;
        self.last_net_check = now();
        if online && self.engine.is_on() {
            self.remember_working_wifi();
        }
        let acts = self.engine.on_net(now(), online);
        self.run_actions(acts);
    }

    /// Startup: resume a session that was running before a crash/reboot,
    /// or put back settings that were left behind.
    fn recover(&mut self) {
        let plan = self.cfg.plan_guid.as_deref().and_then(guid_from_str);
        let active = power::active_scheme().ok();
        match SavedState::load() {
            Some(s) if s.ends_at.is_none_or(|e| e > now() + 60) => {
                let prev = guid_from_str(&s.previous_scheme);
                self.start(s.ends_at.map(|e| e - now()), s.while_claude, prev, true);
            }
            Some(s) => {
                self.previous = guid_from_str(&s.previous_scheme);
                self.restore(EndReason::Timer);
            }
            None if opt_guid_eq(active, plan) => match power::set_active(&SCHEME_BALANCED) {
                Ok(()) => {
                    log("Working Mode plan was active with no session - switched back to Balanced");
                    self.notify("Working Mode", "Restored normal power settings left over from last time.", false);
                }
                Err(e) => {
                    log(&format!("Working Mode plan was active with no session; switching back failed: {e}"));
                    self.notify("Couldn't restore power settings", &format!("{e}. Run working-mode.exe --restore."), true);
                }
            },
            None => {}
        }
    }

    fn reapply_if_on(&mut self) {
        if self.engine.is_on() {
            if let Err(e) = self.apply() {
                self.notify("Couldn't apply the change", &e, true);
            }
            self.update_tray();
        }
    }

    // ------------------------------------------------------------ menu

    fn build_menu(&self) -> HMENU {
        unsafe {
            let m = CreatePopupMenu();
            let item = |menu: HMENU, flags: u32, id: usize, text: &str| {
                let t = wide(text);
                AppendMenuW(menu, flags, id, t.as_ptr());
            };
            let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
            let t = now();

            let header = match (&self.engine.session, &self.engine.countdown) {
                (_, Some(c)) => format!("Ending - {} in {} s", if c.after == AfterAction::Hibernate { "hibernate" } else { "sleep" }, c.at.saturating_sub(t)),
                (Some(s), None) if s.while_claude => format!("ON - {}", self.claude_status()),
                (Some(s), None) => match s.ends_at {
                    Some(e) => format!("ON - {} left (until {})", fmt_span(e.saturating_sub(t)), clock_in(e.saturating_sub(t))),
                    None => "ON - until you stop it".into(),
                },
                (None, None) => "Off - the PC sleeps normally".into(),
            };
            item(m, MF_GRAYED, 0, &header);

            let bat = power::battery().map_or("No battery".into(), |b| {
                format!("Battery {}%{}", b.percent, if b.on_ac { ", plugged in" } else { "" })
            });
            let conn = wifi::current();
            let net = match (&conn, self.engine.online) {
                (_, Some(false)) if self.engine.is_on() => "OFFLINE".to_string(),
                (Some(c), _) => format!("Wi-Fi {}", c.ssid),
                (None, _) => "no Wi-Fi".into(),
            };
            let lid = match self.lid_closed {
                Some(true) => " | lid closed",
                Some(false) => "",
                None => "",
            };
            item(m, MF_GRAYED, 0, &format!("{bat} | {net}{lid}"));
            if self.engine.is_on() {
                item(m, MF_GRAYED, 0, &self.heat_line());
            }
            if !self.problems.is_empty() {
                item(m, MF_GRAYED, 0, &format!("Not safe to close the lid: {}", self.problems.join(", ")));
            }
            item(m, MF_SEPARATOR, 0, "");

            if self.engine.countdown.is_some() {
                item(m, MF_STRING, ID_KEEP, "Keep working");
            }
            if self.engine.is_on() {
                item(m, MF_STRING, ID_EXTEND, "+1 hour");
                item(m, MF_STRING, ID_STOP, "Stop now");
            } else {
                for (i, (label, _)) in PRESETS.iter().enumerate() {
                    item(m, MF_STRING, ID_START + i, &format!("Start for {label}"));
                }
                if self.claude_hooks() {
                    item(m, MF_STRING, ID_START_CLAUDE, "Start while Claude works");
                } else {
                    item(m, MF_STRING, ID_CLAUDE_SETUP, "Start while Claude works (set up)...");
                }
            }
            item(m, MF_SEPARATOR, 0, "");

            let settings = CreatePopupMenu();
            let cpu = CreatePopupMenu();
            for (v, label) in CPU_CHOICES {
                item(cpu, check(self.cfg.cpu_max == v), ID_CPU + v as usize, label);
            }
            item(settings, MF_POPUP, cpu as usize, "CPU limit with the lid closed");
            item(settings, check(self.cfg.turbo_off), ID_TURBO, "Turbo boost off with the lid closed (much cooler)");
            let end = CreatePopupMenu();
            for (i, (v, label)) in END_CHOICES.iter().enumerate() {
                item(end, check(self.cfg.end_with_lid_closed == *v), ID_END + i, label);
            }
            item(settings, MF_POPUP, end as usize, "When it ends with the lid closed");
            let batt = CreatePopupMenu();
            for (i, (v, label)) in BATTERY_CHOICES.iter().enumerate() {
                item(batt, check(self.cfg.battery_floor == *v), ID_BATTERY + i, label);
            }
            item(settings, MF_POPUP, batt as usize, "Battery safety");
            item(settings, check(self.cfg.reconnect), ID_RECONNECT, "Reconnect Wi-Fi if the internet drops");
            item(settings, check(autostart_enabled()), ID_AUTOSTART, "Start with Windows (needed for crash recovery)");
            if let Some(c) = &conn {
                if wifi::profile_mode(&c.profile).as_deref() == Some("manual") {
                    item(settings, MF_STRING, ID_FIX_PROFILE, &format!("Make '{}' reconnect automatically", c.ssid));
                }
            }
            if claude::settings_path().is_some_and(|p| claude::any_hooks(&p)) {
                item(settings, MF_STRING, ID_CLAUDE_REMOVE, "Remove the Claude Code hooks");
            }
            item(settings, MF_SEPARATOR, 0, "");
            item(settings, MF_STRING, ID_CONFIG, "Advanced settings (config.json)");
            item(m, MF_POPUP, settings as usize, "Settings");

            item(m, MF_STRING, ID_LOG, "Open log");
            item(m, MF_STRING, ID_EXIT, "Exit");
            m
        }
    }

    fn command(&mut self, id: usize) {
        let t = now();
        match id {
            _ if (ID_START..ID_START + PRESETS.len()).contains(&id) => {
                let secs = PRESETS[id - ID_START].1;
                self.start((secs > 0).then_some(secs), false, None, false);
            }
            ID_START_CLAUDE => self.start(None, true, None, false),
            ID_CLAUDE_SETUP => {
                let ask = "\"While Claude works\" keeps the PC working while Claude Code is busy, \
                           and ends once it has been quiet for a while.\n\n\
                           To know when Claude is busy, Working Mode adds a few hooks to Claude Code's \
                           settings (%USERPROFILE%\\.claude\\settings.json). They only note when a \
                           session works; nothing else in the file changes, and a copy of it is kept.\n\n\
                           Sessions already open start reporting once restarted.\n\nAdd the hooks?";
                if !claude::git_bash_found() {
                    message_box(
                        "\"While Claude works\" needs Git for Windows (Git Bash): Claude Code runs the hooks with it.                          Install Git, then try again.",
                    );
                } else if confirm(ask) {
                    self.set_claude_hooks(true);
                    if self.claude_hooks() {
                        self.start(None, true, None, false);
                    }
                }
            }
            ID_CLAUDE_REMOVE => self.set_claude_hooks(false),
            ID_EXTEND => {
                let acts = self.engine.extend(t, 3600);
                self.run_actions(acts);
            }
            ID_STOP => {
                let inp = self.inputs();
                let acts = self.engine.stop(&inp);
                self.run_actions(acts);
            }
            ID_KEEP => {
                let acts = self.engine.cancel_countdown(t);
                self.run_actions(acts);
            }
            _ if (ID_CPU + 70..=ID_CPU + 100).contains(&id) => {
                self.cfg.cpu_max = (id - ID_CPU) as u32;
                self.cfg.save();
                self.reapply_if_on();
            }
            ID_TURBO => {
                self.cfg.turbo_off = !self.cfg.turbo_off;
                self.cfg.save();
                self.reapply_if_on();
            }
            _ if (ID_END..ID_END + END_CHOICES.len()).contains(&id) => {
                self.cfg.end_with_lid_closed = END_CHOICES[id - ID_END].0;
                self.cfg.save();
            }
            _ if (ID_BATTERY..ID_BATTERY + BATTERY_CHOICES.len()).contains(&id) => {
                self.cfg.battery_floor = BATTERY_CHOICES[id - ID_BATTERY].0;
                self.cfg.save();
            }
            ID_RECONNECT => {
                self.cfg.reconnect = !self.cfg.reconnect;
                self.cfg.save();
            }
            ID_AUTOSTART => {
                let on = !autostart_enabled();
                if !set_autostart(on) {
                    self.notify("Couldn't change autostart", "See the log.", true);
                }
            }
            ID_FIX_PROFILE => {
                if let Some(c) = wifi::current() {
                    let name = format!("name={}", c.profile);
                    let ok = run_hidden("netsh.exe", &["wlan", "set", "profileparameter", &name, "connectionmode=auto"]).unwrap_or(false)
                        && wifi::profile_mode(&c.profile).as_deref() == Some("auto");
                    log(&format!("set '{}' to connect automatically: {}", c.profile, if ok { "ok" } else { "FAILED" }));
                    if ok {
                        self.notify("Wi-Fi fixed", &format!("'{}' now reconnects by itself.", c.ssid), false);
                    } else {
                        self.notify("Couldn't change the Wi-Fi profile", "See the log.", true);
                    }
                }
            }
            ID_LOG => open_in_notepad(&config::log_path()),
            ID_CONFIG => {
                self.cfg.save();
                open_in_notepad(&config::config_path());
            }
            ID_EXIT => {
                if self.engine.is_on() {
                    self.engine.session = None;
                    self.engine.countdown = None;
                    self.restore(EndReason::User);
                }
                unsafe { DestroyWindow(self.hwnd) };
            }
            _ => {}
        }
        self.engine.params = self.cfg.params();
    }
}

fn show_menu(hwnd: HWND) {
    // re-read config.json so hand edits apply without a restart
    let Some(menu) = with_app(|a| {
        a.cfg = Config::load();
        a.engine.params = a.cfg.params();
        a.build_menu()
    }) else {
        return;
    };
    // the app must not be borrowed here: the menu runs a modal loop that keeps dispatching timer messages
    let cmd = unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, 0, hwnd, null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        cmd
    };
    if cmd > 0 {
        with_app(|a| a.command(cmd as usize));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let event = lparam as u32;
            if event == WM_LBUTTONUP || event == WM_RBUTTONUP {
                show_menu(hwnd);
            }
            0
        }
        WM_TIMER => {
            with_app(|a| a.tick());
            0
        }
        WM_NET => {
            with_app(|a| a.on_net(wparam != 0));
            0
        }
        // 0 = no session, 1 = done, 2 = stopping with a countdown (lid closed)
        WM_REMOTE => with_app(|a| {
            if !a.engine.is_on() {
                return 0;
            }
            let acts = match wparam {
                REMOTE_EXTEND => a.engine.extend(now(), lparam.clamp(60, 24 * 3600) as u64),
                REMOTE_STOP => {
                    let inp = a.inputs();
                    a.engine.stop(&inp)
                }
                REMOTE_END => {
                    a.engine.session = None;
                    a.engine.countdown = None;
                    vec![Action::Restore(EndReason::User)]
                }
                _ => return 0,
            };
            log(&format!("remote command {wparam} ({lparam} s)"));
            a.run_actions(acts);
            if a.engine.countdown.is_some() { 2 } else { 1 }
        })
        .unwrap_or(0),
        WM_POWERBROADCAST => {
            match wparam as u32 {
                // any process can send this message; only the system's carries a setting
                PBT_POWERSETTINGCHANGE if lparam != 0 => {
                    let s = &*(lparam as *const POWERBROADCAST_SETTING);
                    if guid_eq(&s.PowerSetting, &GUID_LIDSWITCH_STATE_CHANGE) && s.DataLength >= 4 {
                        let open = std::ptr::read_unaligned(s.Data.as_ptr().cast::<u32>()) != 0;
                        with_app(|a| {
                            if a.lid_closed.is_some() || a.engine.is_on() {
                                log(if open { "lid opened" } else { "lid closed" });
                            }
                            let changed = a.lid_closed != Some(!open);
                            a.lid_closed = Some(!open);
                            a.tick();
                            // CPU limits only while the lid is closed (see apply)
                            if changed && a.engine.is_on() {
                                let cpu = if !open || a.engine.cooling { "CPU limits on" } else { "CPU back to normal" };
                                let screen = match (open, a.cfg.lid_closed_screen_off_secs) {
                                    (true, _) | (false, 0) => "usual screen timer".to_string(),
                                    (false, n) => format!("screen off after {n} s idle"),
                                };
                                log(&format!("lid {} - {cpu}, {screen}", if open { "open" } else { "closed" }));
                                a.reapply_if_on();
                            }
                        });
                    } else if guid_eq(&s.PowerSetting, &GUID_CONSOLE_DISPLAY_STATE) && s.DataLength >= 4 {
                        let state = std::ptr::read_unaligned(s.Data.as_ptr().cast::<u32>());
                        with_app(|a| {
                            // the first value only says how things are; log changes, and only in a session
                            if a.display.is_some() && a.display != Some(state) && a.engine.is_on() {
                                let lid = match a.lid_closed {
                                    Some(true) => " (lid closed)",
                                    Some(false) => " (lid open)",
                                    None => "",
                                };
                                log(&format!("screen {}{lid}", crate::engine::display_name(Some(state))));
                            }
                            a.display = Some(state);
                        });
                    }
                }
                PBT_APMSUSPEND => log("system is suspending"),
                PBT_APMRESUMEAUTOMATIC => log("system resumed"),
                _ => {}
            }
            1
        }
        WM_DESTROY => {
            let d = tray_data(hwnd);
            Shell_NotifyIconW(NIM_DELETE, &d);
            PostQuitMessage(0);
            0
        }
        _ => {
            // Explorer restarted: put the tray icon back
            if msg != 0 && TASKBAR_CREATED.with(|t| *t.borrow()) == msg {
                with_app(|a| a.add_tray_icon());
                return 0;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

/// The tray app. Returns when the user chooses Exit.
pub fn run() {
    unsafe {
        let name = wide(MUTEX_NAME);
        let mutex = CreateMutexW(null(), 1, name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            message_box("Working Mode is already running - look for its icon in the tray.");
            return;
        }

        let mut cfg = Config::load();
        if !cfg.first_run_done {
            set_autostart(true);
            cfg.first_run_done = true;
            cfg.save();
            log("first run: enabled start with Windows");
        }

        let hinst = GetModuleHandleW(null());
        let class = wide(WINDOW_CLASS);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let title = wide("Working Mode");
        // a real (hidden) top-level window: message-only windows don't get power broadcasts
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), WS_OVERLAPPED, 0, 0, 0, 0, null_mut(), null_mut(), hinst, null());
        if hwnd.is_null() {
            log("could not create the app window");
            return;
        }
        let tc = wide("TaskbarCreated");
        TASKBAR_CREATED.with(|t| *t.borrow_mut() = RegisterWindowMessageW(tc.as_ptr()));

        let icons = Icons {
            on: make_icon((34, 160, 90), glyph_play),
            off: make_icon((120, 120, 120), glyph_pause),
            warn: make_icon((215, 140, 20), glyph_bang),
        };
        let app = App {
            hwnd,
            engine: Engine::new(cfg.params()),
            cfg,
            plan: None,
            previous: None,
            request: None,
            lid_closed: None,
            display: None,
            wifi: None,
            problems: vec![],
            icons,
            last_net_check: 0,
            net_busy_since: None,
            sensors: Sensors::new(),
            ssid: None,
            heat_source: None,
            readings: Readings::default(),
            started: 0,
            no_temp_notified: false,
            plan_drift_notified: false,
        };
        APP.with(|a| *a.borrow_mut() = Some(app));
        with_app(|a| a.add_tray_icon());
        log("app started");

        // the current lid state arrives right after registering
        RegisterPowerSettingNotification(hwnd, &GUID_LIDSWITCH_STATE_CHANGE, DEVICE_NOTIFY_WINDOW_HANDLE);
        RegisterPowerSettingNotification(hwnd, &GUID_CONSOLE_DISPLAY_STATE, DEVICE_NOTIFY_WINDOW_HANDLE);
        with_app(|a| a.recover());
        SetTimer(hwnd, TIMER_ID, TICK_MS, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        with_app(|a| {
            DestroyIcon(a.icons.on);
            DestroyIcon(a.icons.off);
            DestroyIcon(a.icons.warn);
        });
        log("app exited");
        CloseHandle(mutex);
    }
}
