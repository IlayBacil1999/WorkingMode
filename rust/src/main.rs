//! Working Mode: keep a Windows laptop working with the lid closed for a set
//! time, then put everything back. See DESIGN.md.
//!
//!   working-mode.exe              tray app
//!   working-mode.exe --selftest   print what the app sees; changes nothing
//!   working-mode.exe --status     one-screen status of the running session
//!   working-mode.exe --extend [N] add N minutes (default 60) to the running session
//!   working-mode.exe --stop       stop the running session
//!   working-mode.exe --watchdog   run by a scheduled task: restart the app if a session lost it
//!   working-mode.exe --claude-hook       run by Claude Code hooks (see claude.rs)
//!   working-mode.exe --claude-setup      add those hooks to Claude Code's settings
//!   working-mode.exe --claude-remove     take them out again
//!   working-mode.exe --restore    emergency: put normal power settings back
//!   working-mode.exe --uninstall  restore, delete the plan, autostart and data

#![windows_subsystem = "windows"]

mod app;
mod claude;
mod config;
mod engine;
mod power;
mod sensors;
mod watchdog;
mod win;
mod wifi;

use config::{Config, SavedState};
use win::{guid_from_str, guid_to_string, opt_guid_eq, wide};
use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SendMessageTimeoutW, SMTO_ABORTIFHUNG};

fn main() {
    let mut args = std::env::args().skip(1);
    let arg = args.next();
    let Some(arg) = arg else {
        app::run();
        return;
    };
    // runs on every Claude Code tool call: no console, no output, no delay
    if arg == "--claude-hook" {
        std::process::exit(claude::hook());
    }
    // GUI-subsystem exe: borrow the terminal's console for output
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    let code = match arg.as_str() {
        "--selftest" => selftest(),
        "--restore" => restore(),
        "--uninstall" => uninstall(),
        "--status" => status(),
        "--extend" => match args.next().map(|n| n.parse::<u64>()) {
            None => remote(app::REMOTE_EXTEND, 3600),
            Some(Ok(m)) if m > 0 => remote(app::REMOTE_EXTEND, m * 60),
            Some(_) => {
                println!("usage: working-mode.exe --extend [minutes]");
                2
            }
        },
        "--stop" => remote(app::REMOTE_STOP, 0),
        "--watchdog" => watchdog::run(app_running()),
        "--claude-setup" | "--claude-remove" => {
            let install = arg == "--claude-setup";
            let exe = std::env::current_exe().ok();
            let result = match (claude::settings_path(), &exe) {
                _ if install && !claude::git_bash_found() => Err("Claude Code runs these hooks with Git Bash, and it isn't installed".into()),
                (Some(path), Some(exe)) => claude::set_hooks(&path, install.then_some(exe.as_path())).map(|()| path),
                _ => Err("can't find the user profile or this exe".into()),
            };
            match result {
                Ok(path) if install => {
                    println!("Added the hooks to {}. New Claude Code sessions report to Working Mode.", path.display());
                    0
                }
                Ok(path) => {
                    println!("Removed the hooks from {}.", path.display());
                    0
                }
                Err(e) => {
                    println!("{e}");
                    1
                }
            }
        }
        _ => {
            println!("usage: working-mode.exe [--status | --extend [minutes] | --stop | --selftest | --restore | --uninstall]");
            2
        }
    };
    std::process::exit(code);
}

fn app_running() -> bool {
    let name = wide(app::MUTEX_NAME);
    let h = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
    if h.is_null() {
        return false;
    }
    unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    true
}

fn selftest() -> i32 {
    let cfg = Config::load();
    let plan = cfg.plan_guid.as_deref().and_then(guid_from_str);
    let active = power::active_scheme();
    println!("Working Mode self-test (read-only)\n");
    match &active {
        Ok(g) => println!(
            "active power plan    {}{}",
            guid_to_string(g),
            if opt_guid_eq(Some(*g), plan) { "  (= Working Mode plan)" } else { "" }
        ),
        Err(e) => println!("active power plan    ERROR {e}"),
    }
    println!(
        "Working Mode plan    {}",
        match plan {
            Some(g) if power::scheme_exists(&g) => guid_to_string(&g),
            _ => "not created yet".into(),
        }
    );
    if let Ok(g) = active {
        println!("\nactive plan values   (AC / battery)");
        for s in power::ALL {
            let v = power::read(&g, s).map_or("n/a".into(), |(ac, dc)| format!("{ac} / {dc}"));
            println!("  {:<26} {v}", s.name);
        }
    }
    println!();
    match power::battery() {
        Some(b) => println!("battery              {}%{}", b.percent, if b.on_ac { " plugged in" } else { " on battery" }),
        None => println!("battery              none"),
    }
    println!("idle                 {} s", power::idle_secs());
    match wifi::current() {
        Some(c) => {
            println!("wi-fi                '{}' profile '{}' signal {}%", c.ssid, c.profile, c.signal);
            println!("profile reconnects   {}", wifi::profile_mode(&c.profile).unwrap_or_else(|| "?".into()));
        }
        None => println!("wi-fi                not connected"),
    }
    println!("internet (HTTP)      {}", if wifi::internet_ok() { "ok" } else { "FAILED" });
    println!("start with Windows   {}", config::autostart_enabled());
    println!("app running          {}", app_running());
    println!("session saved        {}", SavedState::load().is_some());
    println!("data folder          {}", config::data_dir().display());
    0
}

/// Sends a command to the running tray app.
/// The tray app's answer, or `None` if it isn't running or didn't answer in 10 s.
fn send_to_app(cmd: usize, secs: u64) -> Option<isize> {
    let class = wide(app::WINDOW_CLASS);
    let hwnd = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    if hwnd.is_null() {
        return None;
    }
    let mut answer = 0usize;
    let ok = unsafe { SendMessageTimeoutW(hwnd, app::WM_REMOTE, cmd, secs as isize, SMTO_ABORTIFHUNG, 10_000, &mut answer) };
    (ok != 0).then_some(answer as isize)
}

fn remote(cmd: usize, secs: u64) -> i32 {
    let Some(answer) = send_to_app(cmd, secs) else {
        println!("Working Mode isn't running, or didn't answer.");
        return 1;
    };
    match answer {
        0 => {
            println!("Working Mode is running, but no session is on.");
            1
        }
        2 => {
            println!("Stopping: the lid is closed, so the PC sleeps in about a minute.");
            0
        }
        _ => {
            if cmd == app::REMOTE_EXTEND {
                status();
            } else {
                println!("Stopped. Normal power settings are back.");
            }
            0
        }
    }
}

/// A short status for checking in from a phone.
fn status() -> i32 {
    let cfg = Config::load();
    let sensors = sensors::Sensors::new();
    let t = config::now();
    match SavedState::load() {
        Some(s) if s.while_claude => println!(
            "session              ON, while Claude works ({} Claude session(s) busy now)",
            claude::busy_sessions(&claude::dir(), t)
        ),
        Some(s) => match s.ends_at {
            Some(e) => println!(
                "session              ON, {} left (until {})",
                engine::fmt_span(e.saturating_sub(t)),
                config::clock_in(e.saturating_sub(t))
            ),
            None => println!("session              ON, until stopped"),
        },
        None => println!("session              off"),
    }
    println!("app running          {}", app_running());
    match power::battery() {
        Some(b) if b.on_ac => println!("battery              {}% plugged in", b.percent),
        Some(b) => println!(
            "battery              {}% on battery{}",
            b.percent,
            b.secs_left.map_or(String::new(), |l| format!(", about {} left", engine::fmt_span(l)))
        ),
        None => println!("battery              none"),
    }
    match wifi::current() {
        Some(c) => println!("wi-fi                '{}' signal {}%", c.ssid, c.signal),
        None => println!("wi-fi                not connected"),
    }
    println!("internet (HTTP)      {}", if wifi::internet_ok() { "ok" } else { "FAILED" });
    if let Some(s) = sensors {
        // the rate counters need a second sample after a moment
        std::thread::sleep(std::time::Duration::from_secs(1));
        let r = s.read_now(&cfg.lhm());
        let opt = |v: Option<u32>| v.map_or("?".into(), |v| format!("{v}%"));
        println!(
            "CPU temperature      {}",
            match r.cpu_temp_c {
                Some(t) => format!("{t}C"),
                None => format!("? - LibreHardwareMonitor {}", sensors::lhm_problem(&cfg.lhm())),
            }
        );
        println!(
            "board / busy / speed {} / {} / {}",
            r.temp_c.map_or("?".into(), |t| format!("{t}C")),
            opt(r.cpu_busy),
            opt(r.cpu_freq)
        );
        println!(
            "heat guard           {}",
            match (r.cpu_temp_c, cfg.hot_cpu_temp_c, cfg.hot_load_pct) {
                (Some(_), t, _) if t > 0 => format!("by CPU temperature: CPU held at {}% above {t}C", cfg.hot_cpu_max),
                (None, _, l) if l > 0 => format!(
                    "by CPU load: CPU held at {}% after a few minutes above {l}% busy with the lid closed \
                     (the board sensor counts only if it moves)",
                    cfg.hot_cpu_max
                ),
                _ => "off".into(),
            }
        );
    }
    if let Ok(log) = std::fs::read_to_string(config::log_path()) {
        println!("
last log lines:");
        let lines: Vec<&str> = log.lines().collect();
        for l in &lines[lines.len().saturating_sub(5)..] {
            println!("  {l}");
        }
    }
    0
}

fn restore() -> i32 {
    let cfg = Config::load();
    let plan = cfg.plan_guid.as_deref().and_then(guid_from_str);
    let target = SavedState::load()
        .and_then(|s| guid_from_str(&s.previous_scheme))
        .filter(|g| power::scheme_exists(g) && !opt_guid_eq(Some(*g), plan))
        .unwrap_or(power::SCHEME_BALANCED);
    // end the tray app's session too, or its next tick would carry on (no sleep, even with the lid closed)
    let app_ended = send_to_app(app::REMOTE_END, 0);
    match power::set_active(&target) {
        Ok(()) => {
            SavedState::clear();
            config::log(&format!("--restore: switched to {}", guid_to_string(&target)));
            println!("Normal power settings restored ({}).", guid_to_string(&target));
            if app_running() && app_ended.is_none() {
                println!("Note: the tray app didn't answer; choose Stop or Exit there too.");
            }
            0
        }
        Err(e) => {
            println!("Failed: {e}");
            1
        }
    }
}

fn uninstall() -> i32 {
    if app_running() {
        println!("Working Mode is running. Right-click its tray icon > Exit, then run --uninstall again.");
        return 1;
    }
    let cfg = Config::load();
    let rc = restore();
    if rc != 0 {
        // the data folder is the only record of the plan to remove; keep it for a retry
        println!("Stopped: put normal power settings back first, then run --uninstall again.");
        return rc;
    }
    if let Some(g) = cfg.plan_guid.as_deref().and_then(guid_from_str) {
        if power::scheme_exists(&g) {
            match power::delete_scheme(&g) {
                Ok(()) => println!("Deleted the Working Mode power plan."),
                Err(e) => println!("{e}"),
            }
        }
    }
    config::set_autostart(false);
    watchdog::remove();
    let _ = std::fs::remove_dir_all(config::data_dir());
    println!("Removed autostart and settings.");
    rc
}
