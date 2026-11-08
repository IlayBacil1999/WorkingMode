//! The Working Mode state machine. Pure logic: it gets the time and sensor
//! readings as inputs and returns actions for the app to carry out, so every
//! decision can be unit-tested without touching the system.

use serde::{Deserialize, Serialize};

/// What happens after the session ends while the user is away (lid closed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AfterAction {
    Nothing,
    Sleep,
    Hibernate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    Timer,
    User,
    Battery,
    /// "While Claude works": every Claude session has been quiet long enough.
    ClaudeDone,
}

#[derive(Clone, Debug)]
pub struct Params {
    pub end_with_lid_closed: AfterAction,
    pub battery_floor: u8,
    pub warn_before_secs: u64,
    pub countdown_secs: u64,
    pub away_idle_secs: u64,
    /// "While Claude works" ends after Claude has been quiet this long.
    pub claude_quiet_secs: u64,
    pub reconnect: bool,
    pub reconnect_after_secs: u64,
    /// Lower the CPU limit at this CPU temperature (LibreHardwareMonitor); 0 = off.
    pub hot_cpu_temp_c: i32,
    /// Same, on the board sensor, used when there is no CPU temperature and
    /// the sensor has been seen to move; 0 = off.
    pub hot_temp_c: i32,
    /// With no usable temperature at all: lower the CPU limit when the CPU has
    /// averaged this busy (percent) with the lid closed; 0 = off.
    pub hot_load_pct: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Battery {
    pub percent: u8,
    pub on_ac: bool,
    /// Windows' estimate; `None` while plugged in or still estimating.
    pub secs_left: Option<u64>,
    /// Charge (+) or discharge (-) rate in milliwatts, when the battery reports it.
    pub rate_mw: Option<i32>,
}

/// Sensor readings for the log and the heat guard. Any of them can be missing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Readings {
    /// Real CPU temperature, from LibreHardwareMonitor when it runs.
    pub cpu_temp_c: Option<i32>,
    /// ACPI board sensor; on some laptops it hardly moves.
    pub temp_c: Option<i32>,
    pub cpu_load: Option<u32>,
    /// Percent of time the CPU was busy, whatever its speed.
    pub cpu_busy: Option<u32>,
    /// Percent of the CPU's nominal speed; drops when it throttles.
    pub cpu_freq: Option<u32>,
}

/// One reading of the world, taken by the app every tick.
#[derive(Clone, Copy, Debug)]
pub struct Inputs {
    pub now: u64,
    /// `None` until the first lid-switch notification arrives.
    pub lid_closed: Option<bool>,
    /// Screen state (`GUID_CONSOLE_DISPLAY_STATE`): 0 off, 1 on, 2 dimmed; `None` until it arrives.
    pub display: Option<u32>,
    pub idle_secs: u64,
    pub battery: Option<Battery>,
    pub readings: Readings,
    /// Claude Code sessions working right now (read only in "while Claude works").
    pub claude_busy: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub started_at: u64,
    /// `None` = until the user stops it.
    pub ends_at: Option<u64>,
    pub warned: bool,
    /// Ends when Claude Code has been quiet for `claude_quiet_secs`.
    pub while_claude: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Countdown {
    pub at: u64,
    pub reason: EndReason,
    pub after: AfterAction,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Notify { title: String, body: String, warning: bool },
    Log(String),
    /// Put the normal power plan back and end the session.
    Restore(EndReason),
    /// Run after `Restore`.
    Suspend(AfterAction),
    Reconnect,
    /// Switch the CPU limit to the cooler value (true) or back (false).
    Cool(bool),
}

const GAP_SECS: u64 = 60;
const HEARTBEAT_SECS: u64 = 60;
const PRESENT_IDLE_SECS: u64 = 30;
/// The heat guard acts once it has been hot this long (CPU temps spike briefly),
const HOT_FOR_SECS: u64 = 30;
/// and lifts after at least this long, once it is clearly below the limit.
const COOL_FOR_SECS: u64 = 300;
const CPU_HYSTERESIS_C: i32 = 10;
const BOARD_HYSTERESIS_C: i32 = 5;
/// The load guard lifts once the average is this many points below its limit.
const LOAD_HYSTERESIS_PCT: i32 = 35;
/// Some ACPI zones report a fixed value (28C on this Zenbook): the board
/// sensor counts only once it has moved this much.
const BOARD_MOVES_C: i32 = 2;
/// Time constant of the CPU busy average: ~4 min of full load reach 80%.
const BUSY_AVG_SECS: f64 = 150.0;

/// What the heat guard goes by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeatSource {
    /// Real CPU temperature (LibreHardwareMonitor).
    Cpu,
    /// ACPI board sensor, once it has been seen to move.
    Board,
    /// No usable temperature: sustained CPU load with the lid closed.
    Load,
}

pub struct Engine {
    pub params: Params,
    pub session: Option<Session>,
    pub countdown: Option<Countdown>,
    pub online: Option<bool>,
    pub cooling: bool,
    /// What started the cooling; only that may lift it.
    cooling_by: HeatSource,
    hot_since: Option<u64>,
    cooling_since: u64,
    /// Lowest and highest board reading this session.
    board_range: Option<(i32, i32)>,
    /// Moving averages of `cpu_busy` and `cpu_load`, and when they were last fed.
    busy_avg: f64,
    work_avg: f64,
    /// Which source the running 30 s hot count is for.
    hot_by: Option<HeatSource>,
    busy_at: Option<u64>,
    battery_alerted: bool,
    /// Since when no Claude session has been working.
    claude_quiet_since: Option<u64>,
    offline_since: Option<u64>,
    last_reconnect: Option<u64>,
    last_tick: Option<u64>,
    last_heartbeat: u64,
}

fn notify(title: &str, body: impl Into<String>) -> Action {
    Action::Notify { title: title.into(), body: body.into(), warning: false }
}

fn warn(title: &str, body: impl Into<String>) -> Action {
    Action::Notify { title: title.into(), body: body.into(), warning: true }
}

/// `GUID_CONSOLE_DISPLAY_STATE` value as a word for the log.
pub fn display_name(state: Option<u32>) -> &'static str {
    match state {
        Some(0) => "off",
        Some(1) => "on",
        Some(2) => "dimmed",
        Some(_) | None => "?",
    }
}

pub fn fmt_span(secs: u64) -> String {
    match secs {
        0..=59 => "under a minute".into(),
        60..=3599 => format!("{} min", secs.div_ceil(60)),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

impl Engine {
    pub fn new(params: Params) -> Self {
        Engine {
            params,
            session: None,
            countdown: None,
            online: None,
            cooling: false,
            cooling_by: HeatSource::Cpu,
            hot_since: None,
            cooling_since: 0,
            board_range: None,
            busy_avg: 0.0,
            work_avg: 0.0,
            hot_by: None,
            busy_at: None,
            battery_alerted: false,
            claude_quiet_since: None,
            offline_since: None,
            last_reconnect: None,
            last_tick: None,
            last_heartbeat: 0,
        }
    }

    pub fn is_on(&self) -> bool {
        self.session.is_some()
    }

    pub fn secs_left(&self, now: u64) -> Option<u64> {
        self.session.as_ref()?.ends_at.map(|e| e.saturating_sub(now))
    }

    /// Called after the app has applied the power plan successfully.
    /// `secs = None` runs until stopped. Also used to resume after a crash.
    pub fn start(&mut self, now: u64, secs: Option<u64>) {
        self.session = Some(Session { started_at: now, ends_at: secs.map(|s| now + s), warned: false, while_claude: false });
        self.claude_quiet_since = None;
        self.countdown = None;
        self.cooling = false;
        self.hot_since = None;
        self.board_range = None;
        self.busy_avg = 0.0;
        self.work_avg = 0.0;
        self.hot_by = None;
        self.busy_at = None;
        self.battery_alerted = false;
        self.offline_since = None;
        self.last_reconnect = None;
        self.last_tick = None;
        self.last_heartbeat = now;
    }

    /// Like `start` with no end time, ending instead once Claude Code goes quiet.
    pub fn start_while_claude(&mut self, now: u64) {
        self.start(now, None);
        if let Some(s) = self.session.as_mut() {
            s.while_claude = true;
        }
    }

    /// In "while Claude works": how long no Claude session has worked, if so.
    pub fn claude_quiet_for(&self, now: u64) -> Option<u64> {
        self.claude_quiet_since.map(|t| now.saturating_sub(t))
    }

    pub fn extend(&mut self, now: u64, secs: u64) -> Vec<Action> {
        let Some(s) = self.session.as_mut() else { return vec![] };
        // more time is no answer to a low battery: that countdown stays
        let battery = self.countdown.as_ref().is_some_and(|c| c.reason == EndReason::Battery);
        if !battery {
            self.countdown = None;
        }
        let Some(end) = s.ends_at else {
            if s.while_claude {
                // "+1 hour" here means: don't end now, wait for quiet again
                self.claude_quiet_since = None;
                let quiet = fmt_span(self.params.claude_quiet_secs);
                return vec![notify("Working Mode runs while Claude works", format!("It ends after Claude has been quiet for {quiet}."))];
            }
            return vec![notify("Working Mode runs until you stop it", "There's no end time to extend.")];
        };
        let base = end.max(now);
        s.ends_at = Some(base + secs);
        s.warned = false;
        let left = fmt_span(base + secs - now);
        if battery {
            return vec![warn("Working Mode extended", format!("{left} left, but the battery is low: still hibernating."))];
        }
        vec![notify("Working Mode extended", format!("{left} left."))]
    }

    pub fn stop(&mut self, inp: &Inputs) -> Vec<Action> {
        if self.session.is_none() {
            return vec![];
        }
        self.finish(EndReason::User, inp)
    }

    /// "Keep working" during the countdown.
    pub fn cancel_countdown(&mut self, now: u64) -> Vec<Action> {
        let Some(c) = self.countdown.take() else { return vec![] };
        match c.reason {
            EndReason::User => vec![],
            // the timer already ran out: keeping going means another hour
            EndReason::Timer => self.extend(now, 3600),
            EndReason::Battery => vec![notify("Working Mode keeps running", "Battery safety is paused until you plug in.")],
            EndReason::ClaudeDone => {
                self.claude_quiet_since = None;
                let quiet = fmt_span(self.params.claude_quiet_secs);
                vec![notify("Working Mode keeps running", format!("It ends after Claude has been quiet for {quiet} again."))]
            }
        }
    }

    fn away(&self, inp: &Inputs) -> bool {
        inp.lid_closed.unwrap_or(inp.idle_secs >= self.params.away_idle_secs)
    }

    fn present(&self, inp: &Inputs) -> bool {
        match inp.lid_closed {
            Some(closed) => !closed,
            None => inp.idle_secs < PRESENT_IDLE_SECS,
        }
    }

    fn finish(&mut self, reason: EndReason, inp: &Inputs) -> Vec<Action> {
        let after = if !self.away(inp) {
            AfterAction::Nothing
        } else if reason == EndReason::Battery {
            AfterAction::Hibernate
        } else {
            self.params.end_with_lid_closed
        };
        if after == AfterAction::Nothing || self.params.countdown_secs == 0 {
            self.session = None;
            self.countdown = None;
            let mut acts = vec![Action::Restore(reason)];
            if after != AfterAction::Nothing {
                acts.push(Action::Suspend(after));
            }
            return acts;
        }
        self.countdown = Some(Countdown { at: inp.now + self.params.countdown_secs, reason, after });
        let what = if after == AfterAction::Hibernate { "Hibernating" } else { "Going to sleep" };
        let why = match reason {
            EndReason::Timer => "Time's up.".to_string(),
            EndReason::User => "Stopped.".to_string(),
            EndReason::Battery => format!("Battery is at {}%.", inp.battery.map_or(0, |b| b.percent)),
            EndReason::ClaudeDone => "Claude is done.".to_string(),
        };
        vec![warn(
            &format!("{what} in {} s", self.params.countdown_secs),
            format!("{why} Open the lid to stay awake, or choose \"Keep working\" in the menu."),
        )]
    }

    pub fn tick(&mut self, inp: &Inputs) -> Vec<Action> {
        let mut acts = Vec::new();
        if self.session.is_none() {
            return acts;
        }

        if let Some(last) = self.last_tick {
            if inp.now.saturating_sub(last) > GAP_SECS {
                acts.push(Action::Log(format!("GAP: no tick for {}s - the PC was suspended", inp.now - last)));
                // a reading from before the gap says nothing about now
                self.hot_since = None;
            }
        }
        self.last_tick = Some(inp.now);

        if let Some(c) = self.countdown.clone() {
            if c.reason == EndReason::ClaudeDone && inp.claude_busy > 0 {
                // Claude took up work again (a remote prompt, background work waking up)
                self.countdown = None;
                self.claude_quiet_since = None;
                acts.push(Action::Log("Claude is working again - not ending".into()));
                acts.push(notify("Working Mode keeps running", "Claude is working again."));
                return acts;
            }
            if self.present(inp) {
                // the user came back during the countdown: end without suspending
                self.countdown = None;
                self.session = None;
                acts.push(Action::Restore(c.reason));
            } else if inp.now >= c.at {
                self.countdown = None;
                self.session = None;
                acts.push(Action::Restore(c.reason));
                acts.push(Action::Suspend(c.after));
            }
            return acts;
        }

        if inp.now.saturating_sub(self.last_heartbeat) >= HEARTBEAT_SECS {
            self.last_heartbeat = inp.now;
            let bat = inp.battery.map_or("-".into(), |b| match (b.on_ac, b.secs_left) {
                (true, _) => format!("{}%+AC", b.percent),
                (false, Some(left)) => format!("{}% ({} left)", b.percent, fmt_span(left)),
                (false, None) => format!("{}%", b.percent),
            });
            let lid = match inp.lid_closed {
                Some(true) => "closed",
                Some(false) => "open",
                None => "?",
            };
            let display = display_name(inp.display);
            let watts = inp.battery.and_then(|b| b.rate_mw).map_or(String::new(), |mw| format!("  power={:+.1}W", mw as f64 / 1000.0));
            let online = match self.online {
                Some(true) => "yes",
                Some(false) => "NO",
                None => "?",
            };
            let r = inp.readings;
            let opt = |v: Option<u32>| v.map_or("?".into(), |v| v.to_string());
            acts.push(Action::Log(format!(
                "alive  online={online}  battery={bat}{watts}  lid={lid}  display={display}  idle={}s  cputemp={}C  board={}C  cpu={}%  busy={}% (avg {:.0}%)  speed={}%{}",
                inp.idle_secs,
                r.cpu_temp_c.map_or("?".into(), |t| t.to_string()),
                r.temp_c.map_or("?".into(), |t| t.to_string()),
                opt(r.cpu_load),
                opt(r.cpu_busy),
                self.busy_avg,
                opt(r.cpu_freq),
                if self.cooling { "  COOLING" } else { "" },
            )));
            if self.session.as_ref().is_some_and(|s| s.while_claude) {
                acts.push(Action::Log(format!("claude  busy sessions={}", inp.claude_busy)));
            }
        }

        if self.session.as_ref().is_some_and(|s| s.while_claude) {
            if inp.claude_busy > 0 {
                self.claude_quiet_since = None;
            } else {
                let since = *self.claude_quiet_since.get_or_insert(inp.now);
                if inp.now.saturating_sub(since) >= self.params.claude_quiet_secs {
                    acts.push(Action::Log(format!("no Claude session has worked for {} - ending", fmt_span(inp.now.saturating_sub(since)))));
                    acts.extend(self.finish(EndReason::ClaudeDone, inp));
                    return acts;
                }
            }
        }

        let s = self.session.as_mut().unwrap();
        if let Some(end) = s.ends_at {
            if inp.now >= end {
                acts.extend(self.finish(EndReason::Timer, inp));
                return acts;
            }
            if end - inp.now <= self.params.warn_before_secs && !s.warned {
                s.warned = true;
                acts.push(notify(
                    &format!("Working Mode ends in {}", fmt_span(end - inp.now)),
                    "Choose \"+1 hour\" in the menu to keep going.",
                ));
            }
        }

        acts.extend(self.heat_guard(inp));

        if let Some(b) = inp.battery {
            if b.on_ac {
                self.battery_alerted = false;
            } else if self.params.battery_floor > 0 && b.percent <= self.params.battery_floor && !self.battery_alerted {
                self.battery_alerted = true;
                acts.extend(self.finish(EndReason::Battery, inp));
            }
        }
        acts
    }

    /// Feeds the board range and the load averages; called every tick.
    fn track(&mut self, inp: &Inputs) {
        let r = inp.readings;
        if let Some(t) = r.temp_c {
            let (lo, hi) = self.board_range.get_or_insert((t, t));
            (*lo, *hi) = ((*lo).min(t), (*hi).max(t));
        }
        // a long gap (sleep) counts as one minute, not as hours at this reading
        let dt = self.busy_at.map_or(0, |t| inp.now.saturating_sub(t)).min(60) as f64;
        let k = (dt / BUSY_AVG_SECS).min(1.0);
        if let Some(b) = r.cpu_busy {
            self.busy_avg += (b.min(100) as f64 - self.busy_avg) * k;
        }
        if let Some(u) = r.cpu_load {
            self.work_avg += (u.min(100) as f64 - self.work_avg) * k;
        }
        if r.cpu_busy.is_some() || r.cpu_load.is_some() {
            self.busy_at = Some(inp.now);
        }
    }

    pub fn busy_avg(&self) -> f64 {
        self.busy_avg
    }

    fn board_moves(&self) -> bool {
        self.board_range.is_some_and(|(lo, hi)| hi - lo >= BOARD_MOVES_C)
    }

    /// What the heat guard goes by with these readings. Without a CPU
    /// temperature it watches CPU load, and the board sensor too once it moves.
    pub fn heat_source(&self, r: &Readings) -> HeatSource {
        if r.cpu_temp_c.is_some() {
            HeatSource::Cpu
        } else if r.temp_c.is_some() && self.board_moves() {
            HeatSource::Board
        } else {
            HeatSource::Load
        }
    }

    /// The sources to watch for heat, best first.
    fn heat_candidates(&self, r: &Readings) -> Vec<HeatSource> {
        match self.heat_source(r) {
            HeatSource::Cpu => vec![HeatSource::Cpu],
            // a board sensor that moves a little may never reach its limit: load still counts
            HeatSource::Board => vec![HeatSource::Board, HeatSource::Load],
            HeatSource::Load => vec![HeatSource::Load],
        }
    }

    /// Current value, limit, and how to say it, for one source, to start cooling.
    fn heat_signal(&self, by: HeatSource, inp: &Inputs) -> (Option<i32>, i32, fn(i32) -> String) {
        let r = inp.readings;
        match by {
            HeatSource::Cpu => (r.cpu_temp_c, self.params.hot_cpu_temp_c, |t| format!("CPU at {t}C")),
            HeatSource::Board => (r.temp_c.filter(|_| self.board_moves()), self.params.hot_temp_c, |t| format!("Sensor at {t}C")),
            HeatSource::Load => (
                r.cpu_busy.map(|_| self.busy_avg.round() as i32),
                self.params.hot_load_pct as i32,
                |p| format!("CPU busy {p}% for minutes with the lid closed"),
            ),
        }
    }

    /// Value and the level at or under which cooling may lift, for one source.
    fn cool_signal(&self, by: HeatSource, inp: &Inputs) -> (Option<i32>, i32, fn(i32) -> String) {
        let r = inp.readings;
        let p = &self.params;
        match by {
            HeatSource::Cpu => (r.cpu_temp_c, p.hot_cpu_temp_c - CPU_HYSTERESIS_C, |t| format!("CPU at {t}C")),
            HeatSource::Board => (
                r.temp_c.filter(|_| self.board_moves()),
                p.hot_temp_c - BOARD_HYSTERESIS_C,
                |t| format!("Sensor at {t}C"),
            ),
            // Busy time can't say the job is over: capped, the same job keeps the
            // CPU busy longer. Work done (busy x speed) can, so lifting goes by that.
            // without the utility counter, busy time is all there is
            HeatSource::Load if r.cpu_load.is_none() => (
                r.cpu_busy.map(|_| self.busy_avg.round() as i32),
                p.hot_load_pct as i32 - LOAD_HYSTERESIS_PCT,
                |p| format!("CPU busy down to {p}%"),
            ),
            HeatSource::Load => (
                r.cpu_load.map(|_| self.work_avg.round() as i32),
                p.hot_load_pct as i32 - LOAD_HYSTERESIS_PCT,
                |p| format!("CPU work down to {p}%"),
            ),
        }
    }

    fn heat_guard(&mut self, inp: &Inputs) -> Vec<Action> {
        self.track(inp);
        if self.cooling {
            // only the source that started it may lift it: the board reads far lower
            // than the CPU, and a missing sensor is no proof of cooling down
            let by = self.cooling_by;
            let hot = self.heat_signal(by, inp).1;
            if hot <= 0 {
                return self.lift("heat guard turned off".into());
            }
            // the load guard is for a closed lid; with the user back the fan can breathe
            if by == HeatSource::Load && !self.away(inp) {
                return self.lift("lid opened".into());
            }
            let (t, cool, say) = self.cool_signal(by, inp);
            match t {
                Some(t) if t <= cool && inp.now.saturating_sub(self.cooling_since) >= COOL_FOR_SECS => self.lift(say(t)),
                _ => vec![],
            }
        } else {
            let hot = self.heat_candidates(&inp.readings).into_iter().find_map(|by| {
                let (t, hot, say) = self.heat_signal(by, inp);
                let lid_ok = by != HeatSource::Load || self.away(inp);
                t.filter(|&t| hot > 0 && t >= hot && lid_ok).map(|t| (by, t, say))
            });
            let Some((by, t, say)) = hot else {
                self.hot_since = None;
                return vec![];
            };
            // the 30 s count belongs to one source
            if self.hot_by != Some(by) {
                self.hot_by = Some(by);
                self.hot_since = None;
            }
            let since = *self.hot_since.get_or_insert(inp.now);
            if inp.now.saturating_sub(since) < HOT_FOR_SECS {
                return vec![];
            }
            self.cooling = true;
            self.cooling_by = by;
            self.cooling_since = inp.now;
            vec![
                Action::Log(format!("hot: {} - lowering the CPU limit", say(t))),
                Action::Cool(true),
                warn("Running hot", format!("{}. Slowing the CPU down to cool off.", say(t))),
            ]
        }
    }

    fn lift(&mut self, why: String) -> Vec<Action> {
        self.cooling = false;
        self.hot_since = None;
        vec![Action::Log(format!("cooled: {why} - CPU limit back to normal")), Action::Cool(false)]
    }

    /// Result of an internet probe.
    pub fn on_net(&mut self, now: u64, online: bool) -> Vec<Action> {
        let was = self.online.replace(online);
        let mut acts = Vec::new();
        if self.session.is_none() {
            return acts;
        }
        if online {
            if let Some(since) = self.offline_since.take() {
                let msg = format!("Internet is back after {}.", fmt_span(now - since));
                acts.push(Action::Log(msg.clone()));
                acts.push(notify("Back online", msg));
            }
            self.last_reconnect = None;
            return acts;
        }
        let since = *self.offline_since.get_or_insert(now);
        if was != Some(false) {
            acts.push(Action::Log("internet check failed".into()));
        }
        let due = now - since >= self.params.reconnect_after_secs
            && self.last_reconnect.is_none_or(|t| now - t >= self.params.reconnect_after_secs);
        if self.params.reconnect && due {
            if self.last_reconnect.is_none() {
                acts.push(warn("Internet dropped", "Reconnecting to Wi-Fi..."));
            }
            self.last_reconnect = Some(now);
            acts.push(Action::Log(format!("offline for {}s - reconnecting", now - since)));
            acts.push(Action::Reconnect);
        }
        acts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params {
            end_with_lid_closed: AfterAction::Sleep,
            battery_floor: 15,
            warn_before_secs: 600,
            countdown_secs: 60,
            away_idle_secs: 300,
            claude_quiet_secs: 600,
            reconnect: true,
            reconnect_after_secs: 90,
            hot_cpu_temp_c: 85,
            hot_temp_c: 60,
            hot_load_pct: 80,
        }
    }

    fn inp(now: u64, lid_closed: Option<bool>) -> Inputs {
        Inputs {
            now,
            lid_closed,
            display: Some(1),
            idle_secs: 0,
            battery: Some(Battery { percent: 80, on_ac: true, secs_left: None, rate_mw: None }),
            readings: Readings::default(),
            claude_busy: 0,
        }
    }

    fn restores(acts: &[Action]) -> bool {
        acts.iter().any(|a| matches!(a, Action::Restore(_)))
    }

    #[test]
    fn timer_end_with_lid_open_only_restores() {
        let mut e = Engine::new(params());
        e.start(0, Some(3600));
        let acts = e.tick(&inp(3600, Some(false)));
        assert!(acts.contains(&Action::Restore(EndReason::Timer)));
        assert!(!acts.iter().any(|a| matches!(a, Action::Suspend(_))));
        assert!(!e.is_on());
    }

    #[test]
    fn timer_end_with_lid_closed_counts_down_then_sleeps() {
        let mut e = Engine::new(params());
        e.start(0, Some(3600));
        let acts = e.tick(&inp(3600, Some(true)));
        assert!(!restores(&acts));
        assert!(e.countdown.is_some());
        assert!(e.tick(&inp(3630, Some(true))).is_empty());
        let acts = e.tick(&inp(3660, Some(true)));
        assert_eq!(acts, vec![Action::Restore(EndReason::Timer), Action::Suspend(AfterAction::Sleep)]);
        assert!(!e.is_on());
    }

    #[test]
    fn opening_lid_during_countdown_restores_without_sleep() {
        let mut e = Engine::new(params());
        e.start(0, Some(60));
        e.tick(&inp(60, Some(true)));
        let acts = e.tick(&inp(70, Some(false)));
        assert_eq!(acts, vec![Action::Restore(EndReason::Timer)]);
    }

    #[test]
    fn keep_working_after_timer_adds_an_hour() {
        let mut e = Engine::new(params());
        e.start(0, Some(60));
        e.tick(&inp(60, Some(true)));
        e.cancel_countdown(65);
        assert!(e.countdown.is_none());
        assert_eq!(e.secs_left(65), Some(3600));
    }

    #[test]
    fn unknown_lid_falls_back_to_idle_time() {
        let mut e = Engine::new(params());
        e.start(0, Some(60));
        let mut i = inp(60, None);
        i.idle_secs = 10;
        assert!(restores(&e.tick(&i)));
        e.start(100, Some(60));
        i.now = 160;
        i.idle_secs = 600;
        assert!(e.countdown.is_none());
        e.tick(&i);
        assert!(e.countdown.is_some());
    }

    #[test]
    fn low_battery_away_hibernates_not_sleeps() {
        let mut e = Engine::new(params());
        e.start(0, None);
        let mut i = inp(10, Some(true));
        i.battery = Some(Battery { percent: 14, on_ac: false, secs_left: None, rate_mw: None });
        e.tick(&i);
        assert_eq!(e.countdown.as_ref().unwrap().after, AfterAction::Hibernate);
    }

    #[test]
    fn low_battery_present_just_restores_and_fires_once() {
        let mut e = Engine::new(params());
        e.start(0, None);
        let mut i = inp(10, Some(false));
        i.battery = Some(Battery { percent: 10, on_ac: false, secs_left: None, rate_mw: None });
        assert_eq!(e.tick(&i), vec![Action::Restore(EndReason::Battery)]);
    }

    #[test]
    fn user_stop_with_lid_open_restores() {
        let mut e = Engine::new(params());
        e.start(0, None);
        assert_eq!(e.stop(&inp(5, Some(false))), vec![Action::Restore(EndReason::User)]);
    }

    #[test]
    fn warns_once_before_end() {
        let mut e = Engine::new(params());
        e.start(0, Some(3600));
        let n = |a: &Vec<Action>| a.iter().filter(|x| matches!(x, Action::Notify { .. })).count();
        assert_eq!(n(&e.tick(&inp(3000, Some(false)))), 1);
        assert_eq!(n(&e.tick(&inp(3005, Some(false)))), 0);
    }

    #[test]
    fn extend_resets_warning_and_adds_time() {
        let mut e = Engine::new(params());
        e.start(0, Some(3600));
        e.tick(&inp(3000, Some(false)));
        e.extend(3000, 3600);
        assert_eq!(e.secs_left(3000), Some(4200));
        assert!(!e.session.as_ref().unwrap().warned);
    }

    #[test]
    fn detects_gap_between_ticks() {
        let mut e = Engine::new(params());
        e.start(0, None);
        e.tick(&inp(5, Some(true)));
        let acts = e.tick(&inp(905, Some(true)));
        assert!(acts.iter().any(|a| matches!(a, Action::Log(m) if m.starts_with("GAP"))));
    }

    #[test]
    fn reconnects_after_delay_and_retries() {
        let mut e = Engine::new(params());
        e.start(0, None);
        assert!(!e.on_net(0, false).contains(&Action::Reconnect));
        assert!(!e.on_net(60, false).contains(&Action::Reconnect));
        assert!(e.on_net(90, false).contains(&Action::Reconnect));
        assert!(!e.on_net(120, false).contains(&Action::Reconnect));
        assert!(e.on_net(180, false).contains(&Action::Reconnect));
        let back = e.on_net(240, true);
        assert!(back.iter().any(|a| matches!(a, Action::Notify { title, .. } if title == "Back online")));
    }

    #[test]
    fn nothing_happens_when_off() {
        let mut e = Engine::new(params());
        assert!(e.tick(&inp(100, Some(true))).is_empty());
        assert!(e.on_net(100, false).is_empty());
    }

    fn cpu_at(now: u64, t: i32) -> Inputs {
        let mut i = inp(now, Some(true));
        i.readings.cpu_temp_c = Some(t);
        i.readings.temp_c = Some(28);
        i
    }

    fn cools(a: &[Action], on: bool) -> bool {
        a.contains(&Action::Cool(on))
    }

    #[test]
    fn heat_guard_ignores_short_spikes() {
        let mut e = Engine::new(params());
        e.start(0, None);
        assert!(!cools(&e.tick(&cpu_at(5, 95)), true));
        assert!(!cools(&e.tick(&cpu_at(20, 95)), true));
        assert!(!cools(&e.tick(&cpu_at(25, 70)), true));
        assert!(!cools(&e.tick(&cpu_at(50, 90)), true));
        assert!(!e.cooling);
    }

    #[test]
    fn heat_guard_cools_after_30s_and_lifts_after_5min_below() {
        let mut e = Engine::new(params());
        e.start(0, None);
        e.tick(&cpu_at(5, 86));
        assert!(cools(&e.tick(&cpu_at(35, 88)), true));
        assert!(!cools(&e.tick(&cpu_at(40, 90)), true));
        // cool enough, but not for long enough
        assert!(!cools(&e.tick(&cpu_at(100, 70)), false));
        // long enough, but not cool enough
        assert!(!cools(&e.tick(&cpu_at(400, 80)), false));
        assert!(cools(&e.tick(&cpu_at(405, 75)), false));
        assert!(!e.cooling);
    }

    #[test]
    fn heat_guard_uses_board_sensor_without_cpu_temp() {
        let mut e = Engine::new(params());
        e.start(0, None);
        let board = |now, t| {
            let mut i = inp(now, Some(true));
            i.readings.temp_c = Some(t);
            i
        };
        e.tick(&board(5, 55));
        e.tick(&board(10, 61));
        assert!(cools(&e.tick(&board(45, 61)), true));
    }

    #[test]
    fn heat_guard_ignores_a_board_sensor_that_never_moves() {
        let mut e = Engine::new(params());
        e.start(0, None);
        for now in (5..600).step_by(5) {
            let mut i = inp(now, Some(true));
            i.readings.temp_c = Some(61);
            assert!(!cools(&e.tick(&i), true));
        }
    }

    fn busy_at(now: u64, busy: u32, lid_closed: bool) -> Inputs {
        let mut i = inp(now, Some(lid_closed));
        i.readings.temp_c = Some(28);
        i.readings.cpu_busy = Some(busy);
        i.readings.cpu_load = Some(busy);
        i
    }

    /// Ticks every 5 s from `from` to `to`; true if cooling was switched as `on`.
    fn run_busy(e: &mut Engine, from: u64, to: u64, busy: u32, lid_closed: bool, on: bool) -> bool {
        (from..=to).step_by(5).any(|t| cools(&e.tick(&busy_at(t, busy, lid_closed)), on))
    }

    #[test]
    fn load_guard_cools_after_minutes_of_full_load_with_the_lid_closed() {
        let mut e = Engine::new(params());
        e.start(0, None);
        // a two-minute burst is not enough
        assert!(!run_busy(&mut e, 5, 120, 100, true, true));
        assert!(!run_busy(&mut e, 125, 300, 10, true, true));
        // sustained: cools within ~5 min
        assert!(run_busy(&mut e, 305, 600, 100, true, true));
        assert_eq!(e.cooling_by, HeatSource::Load);
        // still busy: stays cool
        assert!(!run_busy(&mut e, 605, 1200, 100, true, false));
        // the job ends: lifts once the average is low and 5 min have passed
        assert!(run_busy(&mut e, 1205, 1800, 5, true, false));
    }

    #[test]
    fn load_guard_leaves_an_open_lid_alone() {
        let mut e = Engine::new(params());
        e.start(0, None);
        assert!(!run_busy(&mut e, 5, 900, 100, false, true));
        // closing the lid under that load cools soon
        assert!(run_busy(&mut e, 905, 960, 100, true, true));
        // opening it lifts
        assert!(cools(&e.tick(&busy_at(965, 100, false)), false));
    }

    #[test]
    fn load_cooldown_lifts_when_the_work_is_light_even_if_busy_time_is_not() {
        // capped at 60%, a light job keeps the CPU busy longer: busy time stays
        // high while the work done (busy x speed) is low
        let mut e = Engine::new(params());
        e.start(0, None);
        assert!(run_busy(&mut e, 5, 600, 100, true, true));
        let lifted = (605..=1500).step_by(5).any(|t| {
            let mut i = busy_at(t, 58, true);
            i.readings.cpu_load = Some(35);
            cools(&e.tick(&i), false)
        });
        assert!(lifted);
    }

    #[test]
    fn a_board_sensor_that_moves_a_little_does_not_switch_off_the_load_guard() {
        let mut e = Engine::new(params());
        e.start(0, None);
        let cooled = (5..=900).step_by(5).any(|t| {
            let mut i = busy_at(t, 100, true);
            i.readings.temp_c = Some(if t < 100 { 28 } else { 31 });
            cools(&e.tick(&i), true)
        });
        assert!(cooled);
        assert_eq!(e.cooling_by, HeatSource::Load);
    }

    #[test]
    fn load_cooldown_lifts_when_the_user_is_back_and_the_lid_state_is_unknown() {
        let mut e = Engine::new(params());
        e.start(0, None);
        let at = |t, idle| {
            let mut i = busy_at(t, 100, true);
            i.lid_closed = None;
            i.idle_secs = idle;
            i
        };
        assert!((5..=900).step_by(5).any(|t| cools(&e.tick(&at(t, 600)), true)));
        assert!(cools(&e.tick(&at(905, 2)), false));
    }

    #[test]
    fn heat_guard_survives_the_clock_going_back() {
        let mut e = Engine::new(params());
        e.start(1000, None);
        e.tick(&cpu_at(1005, 90));
        // NTP moved the clock back: no instant cooling, no panic
        assert!(!cools(&e.tick(&cpu_at(900, 90)), true));
    }

    #[test]
    fn real_cpu_temperature_wins_over_load() {
        let mut e = Engine::new(params());
        e.start(0, None);
        for t in (5..900).step_by(5) {
            let mut i = busy_at(t, 100, true);
            i.readings.cpu_temp_c = Some(70);
            assert!(!cools(&e.tick(&i), true));
        }
    }

    #[test]
    fn heat_guard_does_not_lift_on_the_board_sensor_when_cpu_temp_drops_out() {
        let mut e = Engine::new(params());
        e.start(0, None);
        e.tick(&cpu_at(5, 88));
        assert!(cools(&e.tick(&cpu_at(35, 88)), true));
        // LibreHardwareMonitor gone: only the cool board sensor is left
        let mut board_only = cpu_at(400, 0);
        board_only.readings.cpu_temp_c = None;
        assert!(!cools(&e.tick(&board_only), false));
        board_only.readings.temp_c = None;
        board_only.now = 900;
        assert!(!cools(&e.tick(&board_only), false));
        assert!(e.cooling);
        // back, and cool
        assert!(cools(&e.tick(&cpu_at(905, 70)), false));
    }

    #[test]
    fn heat_guard_lifts_when_turned_off_while_cooling() {
        let mut e = Engine::new(params());
        e.start(0, None);
        e.tick(&cpu_at(5, 90));
        e.tick(&cpu_at(35, 90));
        e.params.hot_cpu_temp_c = 0;
        assert!(cools(&e.tick(&cpu_at(40, 90)), false));
    }

    #[test]
    fn heat_guard_spike_timer_restarts_after_sleep() {
        let mut e = Engine::new(params());
        e.start(0, None);
        e.tick(&cpu_at(5, 90));
        // the PC slept; first reading after waking must not count as 30 s of heat
        assert!(!cools(&e.tick(&cpu_at(1000, 90)), true));
        assert!(cools(&e.tick(&cpu_at(1030, 90)), true));
    }

    #[test]
    fn extending_an_open_ended_session_keeps_it_open_ended() {
        let mut e = Engine::new(params());
        e.start(0, None);
        e.extend(10, 3600);
        assert_eq!(e.secs_left(10), None);
    }

    #[test]
    fn extending_keeps_a_low_battery_countdown() {
        let mut e = Engine::new(params());
        e.start(0, Some(7200));
        let mut i = inp(10, Some(true));
        i.battery = Some(Battery { percent: 14, on_ac: false, secs_left: None, rate_mw: None });
        e.tick(&i);
        assert!(e.countdown.is_some());
        let acts = e.extend(20, 3600);
        assert_eq!(e.countdown.as_ref().unwrap().reason, EndReason::Battery);
        assert!(acts.iter().any(|a| matches!(a, Action::Notify { warning: true, .. })));
    }

    #[test]
    fn heat_guard_off_at_zero() {
        let mut e = Engine::new(Params { hot_cpu_temp_c: 0, ..params() });
        e.start(0, None);
        e.tick(&cpu_at(5, 99));
        assert!(!cools(&e.tick(&cpu_at(60, 99)), true));
    }

    #[test]
    fn heartbeat_shows_the_screen_state() {
        let mut e = Engine::new(params());
        e.start(0, Some(3600));
        let mut i = inp(HEARTBEAT_SECS, Some(true));
        i.display = Some(0);
        let acts = e.tick(&i);
        assert!(acts.iter().any(|a| matches!(a, Action::Log(l) if l.contains("lid=closed  display=off"))), "{acts:?}");
        assert_eq!(display_name(Some(2)), "dimmed");
        assert_eq!(display_name(None), "?");
    }

    fn claude(now: u64, busy: usize, lid_closed: bool) -> Inputs {
        let mut i = inp(now, Some(lid_closed));
        i.claude_busy = busy;
        i
    }

    #[test]
    fn while_claude_ends_after_quiet_time_and_sleeps_if_away() {
        let mut e = Engine::new(params());
        e.start_while_claude(0);
        assert!(e.tick(&claude(5, 1, true)).is_empty() || e.is_on());
        // busy for an hour: keeps going
        for t in (10..3600).step_by(5) {
            assert!(!restores(&e.tick(&claude(t, 2, true))));
        }
        // quiet, but not long enough; then busy again resets the clock
        e.tick(&claude(3600, 0, true));
        e.tick(&claude(4000, 0, true));
        e.tick(&claude(4005, 1, true));
        e.tick(&claude(4010, 0, true));
        assert!(e.countdown.is_none());
        // 10 quiet minutes: the usual lid-closed countdown
        e.tick(&claude(4610, 0, true));
        assert_eq!(e.countdown.as_ref().unwrap().reason, EndReason::ClaudeDone);
        let acts = e.tick(&claude(4670, 0, true));
        assert_eq!(acts, vec![Action::Restore(EndReason::ClaudeDone), Action::Suspend(AfterAction::Sleep)]);
    }

    #[test]
    fn while_claude_with_the_lid_open_only_restores() {
        let mut e = Engine::new(params());
        e.start_while_claude(0);
        e.tick(&claude(5, 0, false));
        let acts = e.tick(&claude(605, 0, false));
        assert!(acts.contains(&Action::Restore(EndReason::ClaudeDone)));
        assert!(!acts.iter().any(|a| matches!(a, Action::Suspend(_))));
    }

    #[test]
    fn keep_working_after_claude_is_done_waits_for_quiet_again() {
        let mut e = Engine::new(params());
        e.start_while_claude(0);
        e.tick(&claude(5, 0, true));
        e.tick(&claude(605, 0, true));
        assert!(e.countdown.is_some());
        e.cancel_countdown(610);
        assert!(e.countdown.is_none() && e.is_on());
        assert!(!restores(&e.tick(&claude(615, 0, true))));
        e.tick(&claude(1214, 0, true));
        assert!(e.countdown.is_none());
        e.tick(&claude(1215, 0, true));
        assert!(e.countdown.is_some());
    }

    #[test]
    fn plus_an_hour_while_claude_is_done_waits_for_quiet_again() {
        let mut e = Engine::new(params());
        e.start_while_claude(0);
        e.tick(&claude(5, 0, true));
        e.tick(&claude(605, 0, true));
        assert!(e.countdown.is_some());
        e.extend(610, 3600);
        assert!(e.countdown.is_none());
        e.tick(&claude(615, 0, true));
        assert!(e.countdown.is_none(), "must not count down again right away");
    }

    #[test]
    fn claude_working_again_cancels_the_countdown() {
        let mut e = Engine::new(params());
        e.start_while_claude(0);
        e.tick(&claude(5, 0, true));
        e.tick(&claude(605, 0, true));
        assert!(e.countdown.is_some());
        let acts = e.tick(&claude(630, 1, true));
        assert!(!restores(&acts));
        assert!(e.countdown.is_none() && e.is_on());
        assert!(!e.tick(&claude(700, 1, true)).iter().any(|a| matches!(a, Action::Suspend(_))));
    }

    #[test]
    fn while_claude_still_has_the_battery_floor() {
        let mut e = Engine::new(params());
        e.start_while_claude(0);
        let mut i = claude(10, 3, true);
        i.battery = Some(Battery { percent: 12, on_ac: false, secs_left: None, rate_mw: None });
        e.tick(&i);
        assert_eq!(e.countdown.as_ref().unwrap().reason, EndReason::Battery);
    }

    #[test]
    fn span_formatting() {
        assert_eq!(fmt_span(30), "under a minute");
        assert_eq!(fmt_span(61), "2 min");
        assert_eq!(fmt_span(3700), "1h 01m");
    }
}
