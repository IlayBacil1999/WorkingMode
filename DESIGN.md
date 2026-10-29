# Working Mode: design (Rust version)

Builds on [RESEARCH.md](RESEARCH.md). Everything that can be decided without hardware tests is decided here. The parameters that only a real test can settle are listed in §6 and live in `config.json`, so a test result means a config change, not a code change.

## 1. The promise

> Start Working Mode, close the lid, walk away. The laptop keeps working and stays online, reconnecting on its own if the link drops. When the time is up, or when you say stop, it goes back to normal. If the lid is closed at that moment, it does what a closed lid normally does (sleep).

While on, the PC behaves exactly as usual, apart from our limits (CPU cap, turbo off).

## 2. States

```
            Start (1h/2h/4h/8h/until stopped)
   OFF  ───────────────────────────────────▶  ON ──── +1 hour ───┐
    ▲                                          │  ◀───────────────┘
    │                                          │ time up / Stop / battery low
    │                         lid OPEN (user here)│ lid CLOSED (user away)
    │◀──────────── restore only ───────────────┤
    │                                          ▼
    │                                    COUNTDOWN (60 s, cancelable)
    │                                          │   lid opened → restore only
    │◀── restore, then Sleep / Hibernate ──────┘   "Keep working" → back to ON (+1h)
```

- **"Away"** = lid closed (lid-switch event). If the lid state is unknown, it falls back to no input for 5 min.
- **Battery low** (default 15%, on battery): ends the session. If away, it **hibernates**, never just sleeps, so nothing is lost when the battery runs out.
- **Stop from the menu** means the user is there (lid open), so it only restores.
- **Restore** = switch back to the previous power plan, release the power request, delete the state file.

## 3. What "ON" changes

A dedicated plan named "Working Mode" is copied from Balanced once and reused. The user's own plan is never edited.

| Setting | AC/DC value | Why |
|---|---|---|
| Lid close action | Do nothing | the only thing that stops lid sleep |
| Sleep after / Hibernate after | Never | idle sleep |
| System unattended sleep timeout (hidden) | 0 | was 120 s on this machine |
| Networking connectivity in standby | Enable | fallback if standby happens anyway |
| Wi-Fi power saving | Maximum performance | was "medium saving" on battery |
| Max processor state | 90% (configurable) | heat |
| Processor boost mode | Disabled (configurable) | heat, biggest single lever |
| Turn off display after | 10 min | screen off when lid open + idle |

Plus a **power request** (SystemRequired + ExecutionRequired) held for the session.

After applying, the app **reads every critical value back**. If any failed, the icon shows a warning and the menu says "Not safe to close the lid: …". It never claims ON when it isn't.

## 4. Watchdogs while ON

| Watchdog | How | Interval |
|---|---|---|
| Internet | HTTP probe to `msftconnecttest.com` (fallback `gstatic generate_204`), not ping | 60 s |
| Reconnect | after 90 s offline, WLAN API connects to the profile in use at start; retries every 90 s | |
| Heartbeat | log line: online, battery, lid, idle | 60 s |
| Sleep detector | gap > 60 s between 5 s ticks ⇒ log `GAP` (the PC was suspended) | 5 s |
| Ending soon | notification 10 min before the end | |
| Lid | `GUID_LIDSWITCH_STATE_CHANGE` notifications, logged | event |

## 5. Safety

- **Crash/reboot recovery:** `state.json` is written on every change. On start: if a session is still due, it resumes. If it expired or is unknown, it restores.
- **Autostart:** an HKCU Run key, needed so recovery runs after a reboot. On by default; toggle in the menu.
- **Single instance:** a named mutex.
- **No admin needed** for anything (verify T1).
- **Emergency CLI:** `working-mode.exe --restore` puts normal settings back without the UI.
- `--uninstall` restores, deletes the plan, removes autostart and data.
- `--selftest` prints everything the app sees and changes nothing.

## 6. Open parameters (settled by tests, live in config.json)

| Key | Default | Settled by |
|---|---|---|
| `sleep_method` | `"suspend"` | T5: does SetSuspendState(sleep) work on Modern Standby, or must "Sleep" mean hibernate? |
| `end_with_lid_closed` | `"sleep"` | user preference: `sleep` / `hibernate` / `nothing` |
| `cpu_max` / `turbo_off` | 90 / true | T4 heat and speed |
| `unattended_zero` | true | T3 |
| `hold_power_request` | true | T2: needed? |
| `reconnect_after_secs` | 90 | T6 phone link drops |
| `net_check_secs` | 60 | T6 |
| `countdown_secs` | 60 | UX |
| `away_idle_secs` | 300 | only if the lid event never arrives (T2) |
| `battery_floor` | 15 | |
| `hot_load_pct` | 80 | T14: heat guard by CPU load, used when there's no CPU temperature and the board sensor never moves (on this Zenbook it's fixed at 28C) |

## 7. Code layout (`rust/`)

| File | Role |
|---|---|
| `engine.rs` | Pure state machine: timer, countdown, battery, network decisions. No OS calls, unit-tested |
| `power.rs` | powrprof plan create/write/read/verify, power request, battery, idle, sleep/hibernate |
| `wifi.rs` | WLAN API: current profile/SSID, profile connection mode, reconnect; HTTP internet probe |
| `config.rs` | config/state JSON, paths, log, autostart key |
| `app.rs` | tray icon, menu, notifications, lid events, glue between engine and OS |
| `claude.rs` | "While Claude works": the hook, busy-session count, settings.json hooks install/remove |
| `main.rs` | CLI (`--selftest`, `--restore`, `--uninstall`), single instance, message loop |

Stack: `windows-sys` (raw Win32, one native dependency) + `serde`/`serde_json`. No webview and no GUI framework. Native tray menu and balloon notifications (these show as toasts on Windows 11, no AppUserModelID needed).

## 8. While Claude works

A session with no end time that ends once no Claude Code session has worked for `claude_quiet_mins` (10). Then it ends like the timer: restore with the lid open, the 60 s countdown and sleep with it closed. Battery floor and heat guard work as usual.

- **How it knows:** Claude Code hooks in `~/.claude/settings.json` (UserPromptSubmit, Pre/PostToolUse, Stop, SubagentStop, SessionEnd) run `working-mode.exe --claude-hook`, async and under Git Bash. The tray menu adds them after a confirm box ("Start while Claude works (set up)..."), or `--claude-setup` / `--claude-remove`. The rest of the file is kept as is, and the first original is kept as `settings.json.before-working-mode`.
- **Busy session** (`claude.rs`, one file per fact so concurrent hooks don't clash): a turn is running and had an event in the last 30 min; or the turn stopped with background work still running (up to 4 h); or any event in the last 2 min (subagents).
- **Known limits:** Esc sends no Stop, so an interrupted turn counts as busy for up to 30 min. Sessions open before the setup report only after a restart.
- **"+1 hour" / "Keep working"** during the end countdown: wait for quiet again. Claude working again during the countdown cancels it.

## 9. Later

- "Until HH:MM".
- Small status window: session timeline, drops, battery graph.
- Signed build.
