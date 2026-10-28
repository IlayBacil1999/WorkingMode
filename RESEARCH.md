# Working Mode: research findings and decisions

Goal: the laptop keeps working (Claude Code running, online through the phone) with the lid closed, for a chosen time or while Claude is busy. Afterwards everything goes back to normal, safely.

Sources: 4 research agents (Windows power internals, existing tools/UX, network/thermal/Claude Code, Rust stack) plus read-only checks on this machine. Items marked (verify) still need a hands-on test.

## 1. What this machine actually does today (checked)

| Setting | AC | Battery | Effect on lid-closed work |
|---|---|---|---|
| Sleep state | Modern Standby (S0, network connected), Hibernate. No S3 | | |
| Lid close action (hidden) | Sleep | Sleep | Closing the lid starts standby. **Must change** |
| Sleep after (STANDBYIDLE) | Never | 10 min | Sleeps on battery. **Must change** |
| Hibernate after | Never | 16 h | Change to never while working |
| System unattended sleep timeout (hidden) | **120 s** | **120 s** | Can put the PC to sleep 2 min after a wake with no user. **Must change** |
| Wi-Fi power saving mode | Max performance | **Medium saving** | Likely cause of Wi-Fi drops on battery. **Must change** |
| Network connectivity in standby (hidden) | Enable | Managed by Windows | Fallback only. Set to Enable |
| Max processor state | 100% | 100% | Thermal lever |
| Processor boost mode (hidden) | Present (0..5) | | Turning turbo off = biggest heat reduction |
| Power plans | Only "Balanced" | | Duplicating it works via powercfg (verify without admin) |
| Wi-Fi profile `DIRECT-w5-Ferry` | **Connect manually** | | If the link drops, Windows will **not** rejoin. Root cause #1 for "lost internet" |
| Internet check | ICMP ping blocked, HTTP works | | Never use ping as the health check |
| Proxy | None (direct) | | |

## 2. Key facts learned

1. **Keep-awake APIs don't stop lid sleep.** SetThreadExecutionState / PowerSetRequest only block *idle* sleep. Lid close is "user-initiated sleep" and clears them (Microsoft Learn, PowerSetRequest). PowerToys Awake, Caffeine, keepawake-rs and KeepAwake (Rust) all share this limitation. **Only the plan's lid action works.**
2. **Use power requests anyway**, for the idle side: `PowerCreateRequest` + `PowerSetRequest(SystemRequired, ExecutionRequired)`, cleared on exit. Better than SetThreadExecutionState on Modern Standby (forum reports; verify).
3. **Claude Code on Windows doesn't hold the PC awake.** Its sleep inhibitor is reportedly macOS-only (community issue reports, not official docs). So our tool is needed.
4. **Network drops after a long wait can hang a Claude Code turn.** History survives (`--continue`/`--resume`) but the interrupted turn doesn't. Network stability matters more than anything else.
5. **Lid state is observable:** `RegisterPowerSettingNotification(GUID_LIDSWITCH_STATE_CHANGE)` gives 0 = closed / 1 = open. Better than an idle timer for "is the user away?".
6. **Heat:** a closed lid blocks some vents on many laptops. Main levers: turbo off (PERFBOOSTMODE=0) first, then max processor state 80–90%. No reliable non-admin temperature reading exists.
7. **Windows Update** restarts can't be blocked without admin; active hours are set by hand in Settings.
8. **Phone side:** Android battery optimization stops tethering apps when the screen turns off. Exempt the app (Unrestricted) and keep the phone charging.

## 3. Decisions

| # | Decision | Why |
|---|---|---|
| D1 | Dedicated "Working Mode" plan (copy of Balanced), switch to it and back | The user's plan is never edited; restore = one switch; crash leaves a harmless plan |
| D2 | In that plan: lid=Do nothing, sleep/hibernate/unattended=never, Wi-Fi=max perf, network in standby=Enable, turbo=off, max CPU=90% (default) | Covers every sleep trigger found on this machine, plus heat |
| D3 | Also hold a power request while active | Belt and braces against idle sleep |
| D4 | "Away" = lid closed (lid event), fallback idle > 5 min | Hibernate only when nobody is there |
| D5 | Health check = NLM connectivity events plus HTTP probe (msftconnecttest); reconnect with WLAN API | Ping is blocked here; NLM can be fooled by captive portals |
| D6 | One-time setup check: offer to set the hotspot profile to auto-connect | Biggest real-world failure |
| D7 | Safety: battery floor → hibernate (60 s cancelable), crash/reboot restore, warning before end, +1h | From prototype, matches Amphetamine/Awake |
| D8 | Rust: `tray-icon` + `muda` + `tao`, `windows` crate (Power, WiFi, NLM), named mutex, HKCU Run, toast via AUMID shortcut | Single small exe, native, no webview, no admin |

## 4. New feature ideas from the research

- **"While Claude works" mode** (Amphetamine "while app runs" trigger): stay on while a Claude Code session is active, then end after N quiet minutes. The best fit for the actual use case.
- **"Until HH:MM"** in addition to durations (PowerToys Awake "expirable").
- **Honest status:** show ON only after every setting was confirmed applied; list any that failed.
- **Pre-flight checklist** on first start: hotspot auto-connect, power plan writable, hibernate available, phone tips.
- **Session log/report:** "Ran 3h12m, 2 Wi-Fi drops (reconnected), min battery 41%".

## 5. Still to verify on this machine (hands-on test)

1. Can a standard user duplicate a plan and write hidden settings (lid, boost, unattended) without admin?
2. Lid closed + plan active: does it stay fully awake (no standby) for 30+ min on battery and on AC, with Wi-Fi up?
3. Does setting unattended sleep = 0 matter in practice?
4. `shutdown /h` from a background app with the lid closed.
5. How often the phone link drops, and whether auto-connect fixes it.
