//! Power plans, power requests, battery, idle time and sleep/hibernate,
//! straight through powrprof/kernel32 (no powercfg.exe, no admin).

use crate::config::{log, Config};
use crate::engine::Battery;
use crate::win::{guid_from_str, guid_to_string, wide};
use std::ptr::{null, null_mut};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE, LUID};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Power::*;
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

pub const SCHEME_BALANCED: GUID = GUID::from_u128(0x381b4222_f694_41f0_9685_ff5bb260df2e);
pub const GUID_LIDSWITCH_STATE_CHANGE: GUID = GUID::from_u128(0xba3e0f4d_b817_4094_a2d1_d56379e6a0f3);

pub struct Setting {
    pub name: &'static str,
    pub sub: GUID,
    pub id: GUID,
}

const fn setting(name: &'static str, sub: u128, id: u128) -> Setting {
    Setting { name, sub: GUID::from_u128(sub), id: GUID::from_u128(id) }
}

const SUB_NONE: u128 = 0xfea3413e_7e05_4911_9a71_700331f1c294;
const SUB_BUTTONS: u128 = 0x4f971e89_eebd_4455_a8de_9e59040e7347;
const SUB_SLEEP: u128 = 0x238c9fa8_0aad_41ed_83f4_97be242c8f20;
const SUB_VIDEO: u128 = 0x7516b95f_f776_4464_8c53_06167f40cc99;
const SUB_PROCESSOR: u128 = 0x54533251_82be_4824_96c1_47b60b740d00;
const SUB_WIFI: u128 = 0x19cbb8fa_5279_450e_9fac_8a3d5fedd0c1;

pub const LID_ACTION: Setting = setting("lid close action", SUB_BUTTONS, 0x5ca83367_6e45_459f_a27b_476b1d01c936);
pub const SLEEP_AFTER: Setting = setting("sleep timer", SUB_SLEEP, 0x29f6c1db_86da_48c5_9fdb_f2b67b1f44da);
pub const HIBERNATE_AFTER: Setting = setting("hibernate timer", SUB_SLEEP, 0x9d7815a6_7ee4_497e_8888_515a05f02364);
pub const UNATTENDED_SLEEP: Setting = setting("unattended sleep timeout", SUB_SLEEP, 0x7bc4a2f9_d8fc_4469_b07b_33eb785aaca0);
pub const NET_IN_STANDBY: Setting = setting("network in standby", SUB_NONE,0xf15576e8_98b7_4186_b944_eafa664402d9);
pub const SCREEN_OFF: Setting = setting("screen off timer", SUB_VIDEO, 0x3c0bc021_c8a8_4e07_a973_6b14cbcb2b7e);
pub const CPU_MAX: Setting = setting("max processor state", SUB_PROCESSOR, 0xbc5038f7_23e0_4960_96da_33abaf5935ec);
pub const TURBO: Setting = setting("processor boost mode", SUB_PROCESSOR, 0xbe337238_0d82_4146_a960_4f3749d470c7);
pub const WIFI_SAVING: Setting = setting("Wi-Fi power saving", SUB_WIFI, 0x12bbebe6_58d6_4636_95bb_3217ef867c1a);

pub const ALL: [&Setting; 9] =
    [&LID_ACTION, &SLEEP_AFTER, &HIBERNATE_AFTER, &UNATTENDED_SLEEP, &NET_IN_STANDBY, &SCREEN_OFF, &CPU_MAX, &TURBO, &WIFI_SAVING];

fn ok(rc: u32, what: &str) -> Result<(), String> {
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("{what} failed (error {rc})"))
    }
}

/// Copies a GUID that powrprof allocated with LocalAlloc, then frees it.
unsafe fn take_guid(p: *mut GUID) -> GUID {
    let g = *p;
    LocalFree(p.cast());
    g
}

pub fn active_scheme() -> Result<GUID, String> {
    let mut p = null_mut();
    unsafe {
        ok(PowerGetActiveScheme(null_mut(), &mut p), "reading the active power plan")?;
        Ok(take_guid(p))
    }
}

pub fn set_active(g: &GUID) -> Result<(), String> {
    unsafe { ok(PowerSetActiveScheme(null_mut(), g), "switching power plan") }
}

/// (AC, DC) values of a setting in a plan.
pub fn read(scheme: &GUID, s: &Setting) -> Option<(u32, u32)> {
    let (mut ac, mut dc) = (0u32, 0u32);
    unsafe {
        let a = PowerReadACValueIndex(null_mut(), scheme, &s.sub, &s.id, &mut ac);
        let d = PowerReadDCValueIndex(null_mut(), scheme, &s.sub, &s.id, &mut dc);
        (a == ERROR_SUCCESS && d == ERROR_SUCCESS).then_some((ac, dc))
    }
}

fn write(scheme: &GUID, s: &Setting, ac: u32, dc: u32) -> Result<(), String> {
    unsafe {
        ok(PowerWriteACValueIndex(null_mut(), scheme, &s.sub, &s.id, ac), s.name)?;
        ok(PowerWriteDCValueIndex(null_mut(), scheme, &s.sub, &s.id, dc), s.name)
    }
}

pub fn scheme_exists(g: &GUID) -> bool {
    read(g, &SLEEP_AFTER).is_some()
}

/// Our plan, creating it (a copy of Balanced) if it's missing.
pub fn ensure_plan(cfg: &mut Config) -> Result<GUID, String> {
    if let Some(g) = cfg.plan_guid.as_deref().and_then(guid_from_str) {
        if scheme_exists(&g) {
            return Ok(g);
        }
    }
    let mut p = null_mut();
    let g = unsafe {
        ok(PowerDuplicateScheme(null_mut(), &SCHEME_BALANCED, &mut p), "creating the Working Mode power plan")?;
        take_guid(p)
    };
    let name = wide("Working Mode");
    unsafe {
        PowerWriteFriendlyName(null_mut(), &g, null(), null(), name.as_ptr().cast(), (name.len() * 2) as u32);
    }
    cfg.plan_guid = Some(guid_to_string(&g));
    cfg.save();
    log(&format!("created power plan {}", guid_to_string(&g)));
    Ok(g)
}

/// Writes all Working Mode values into our plan. `previous` supplies the
/// user's own values for anything the config says to leave alone.
/// Returns the settings that could not be written (missing on this PC is fine).
pub fn apply_plan(plan: &GUID, cfg: &Config, previous: &GUID) -> Vec<String> {
    let keep = |s: &Setting, fallback: (u32, u32)| read(previous, s).unwrap_or(fallback);
    let cpu = cfg.cpu_max.clamp(5, 100);
    let turbo = if cfg.turbo_off { (0, 0) } else { keep(&TURBO, (2, 2)) };
    let unattended = if cfg.unattended_zero { (0, 0) } else { keep(&UNATTENDED_SLEEP, (120, 120)) };
    let values: [(&Setting, (u32, u32)); 9] = [
        (&LID_ACTION, (0, 0)),
        (&SLEEP_AFTER, (0, 0)),
        (&HIBERNATE_AFTER, (0, 0)),
        (&UNATTENDED_SLEEP, unattended),
        (&NET_IN_STANDBY, (1, 1)),
        (&SCREEN_OFF, (cfg.screen_off_secs, cfg.screen_off_secs)),
        (&CPU_MAX, (cpu, cpu)),
        (&TURBO, turbo),
        (&WIFI_SAVING, (0, 0)),
    ];
    let mut failed = Vec::new();
    for (s, (ac, dc)) in values {
        if let Err(e) = write(plan, s, ac, dc) {
            log(&e);
            failed.push(s.name.to_string());
        }
    }
    failed
}

/// Reads the critical settings back. Returns the names of those that are wrong;
/// empty means it's safe to close the lid.
pub fn verify_plan(plan: &GUID, cfg: &Config) -> Vec<String> {
    let mut critical = vec![&LID_ACTION, &SLEEP_AFTER, &WIFI_SAVING];
    if cfg.unattended_zero {
        critical.push(&UNATTENDED_SLEEP);
    }
    critical.into_iter().filter(|s| read(plan, s) != Some((0, 0))).map(|s| s.name.to_string()).collect()
}

pub fn delete_scheme(g: &GUID) -> Result<(), String> {
    unsafe { ok(PowerDeleteScheme(null_mut(), g), "deleting the Working Mode plan") }
}

/// Held for the whole session: blocks idle sleep and app throttling.
/// It cannot block lid-close sleep; the plan does that.
pub struct PowerRequest(HANDLE);

impl PowerRequest {
    pub fn new() -> Option<Self> {
        let mut reason = wide("Working Mode: keeping the PC working with the lid closed");
        let ctx = REASON_CONTEXT {
            Version: 0, // POWER_REQUEST_CONTEXT_VERSION
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 { SimpleReasonString: reason.as_mut_ptr() },
        };
        unsafe {
            let h = PowerCreateRequest(&ctx);
            if h.is_null() || h as isize == -1 {
                return None;
            }
            let sys = PowerSetRequest(h, PowerRequestSystemRequired) != 0;
            let exe = PowerSetRequest(h, PowerRequestExecutionRequired) != 0;
            if !sys || !exe {
                log(&format!("power request partly refused (system={sys}, execution={exe})"));
            }
            Some(PowerRequest(h))
        }
    }
}

impl Drop for PowerRequest {
    fn drop(&mut self) {
        unsafe {
            PowerClearRequest(self.0, PowerRequestSystemRequired);
            PowerClearRequest(self.0, PowerRequestExecutionRequired);
            CloseHandle(self.0);
        }
    }
}

pub fn battery() -> Option<Battery> {
    let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut s) } == 0 || s.BatteryFlag & 128 != 0 || s.BatteryLifePercent > 100 {
        return None;
    }
    let secs_left = (s.BatteryLifeTime != u32::MAX).then_some(s.BatteryLifeTime as u64);
    Some(Battery { percent: s.BatteryLifePercent, on_ac: s.ACLineStatus == 1, secs_left })
}

pub fn idle_secs() -> u64 {
    let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if GetLastInputInfo(&mut li) == 0 {
            return 0;
        }
        (GetTickCount().wrapping_sub(li.dwTime) / 1000) as u64
    }
}

fn enable_shutdown_privilege() {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
            return;
        }
        let mut luid = LUID { LowPart: 0, HighPart: 0 };
        let name = wide("SeShutdownPrivilege");
        if LookupPrivilegeValueW(null(), name.as_ptr(), &mut luid) != 0 {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            AdjustTokenPrivileges(token, 0, &tp, 0, null_mut(), null_mut());
        }
        CloseHandle(token);
    }
}

/// Sleep or hibernate now. If sleep is refused (possible on Modern Standby),
/// falls back to hibernate: with the lid closed, staying on is the worst outcome.
pub fn suspend(hibernate: bool) {
    enable_shutdown_privilege();
    log(if hibernate { "hibernating" } else { "going to sleep" });
    if unsafe { SetSuspendState(hibernate as u8, 0, 0) } != 0 {
        return;
    }
    log(&format!("SetSuspendState({}) refused - falling back to shutdown /h", if hibernate { "hibernate" } else { "sleep" }));
    if !crate::app::run_hidden("shutdown.exe", &["/h"]).unwrap_or(false) {
        log("shutdown /h failed too (is hibernation off?) - the PC stays on");
    }
}
