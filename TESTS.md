# Working Mode: hardware test plan

Each test settles one open question from [DESIGN.md §6](DESIGN.md). A result means changing a value in `%LOCALAPPDATA%\WorkingMode\config.json` (Settings › Advanced), not changing code.

**Evidence = the log** (`%LOCALAPPDATA%\WorkingMode\log.txt`, menu › Open log). While ON it writes:

- `alive online=… battery=… lid=… idle=…` every minute
- `GAP: no tick for Ns` if the PC was suspended
- `lid closed` / `lid opened`, `system is suspending` / `system resumed`
- `internet check failed`, `reconnect …`, `Internet is back after …`

A good run has an `alive` line every minute, no `GAP`, and `online=yes`.

## Before any test (once)

1. Run `rust\build.cmd`, then start `rust\working-mode.exe`. A grey ⏸ icon appears in the tray.
2. Run `working-mode.exe --selftest` in a terminal and keep the output to compare against.
3. Phone: exempt the proxy/hotspot app from battery optimization ("Unrestricted"), and keep the phone charging.
4. If something goes wrong at any point: `working-mode.exe --restore`.

## Tests

| # | Test | Steps | Pass looks like | Settles |
|---|---|---|---|---|
| **T1** | Applies without admin | Start for 1 hour (lid open) | Green ▶ icon; the notification says "You can close the lid"; the menu has no "Not safe" line; `--selftest` shows `lid close action 0 / 0`, `unattended 0 / 0`, `Wi-Fi power saving 0 / 0` | whether the approach works at all on Home without admin |
| **T2** | Lid closed, battery, 30 min | Unplug, start 1 hour, close the lid on a hard surface, come back after 30 min | 30 `alive` lines, no `GAP`, `lid=closed`, `online=yes`; the log shows `lid closed` (proves lid events arrive) | the core promise. If `lid=?` throughout → lid events don't arrive and `away_idle_secs` is the fallback |
| **T3** | Lid closed, plugged in, 30 min | Same, plugged in | Same | AC side |
| **T4** | Heat and speed | During T2/T3, feel the keyboard/underside after 20 min. Optionally run a heavy job (e.g. a build) | Warm, not hot. If hot → `cpu_max` 80 | `cpu_max`, `turbo_off` |
| **T5** | End with lid closed | Settings › "When it ends with the lid closed" › Sleep. Start for 15 minutes, close the lid, come back after 20 min | After 60 s of countdown the log shows `session ended (Timer)` then `going to sleep`, and the PC sleeps. On opening the lid it wakes normally | `sleep_method`: if the log shows `SetSuspendState(sleep) refused`, or it hibernated instead, set `"sleep_method": "hibernate"` |
| **T6** | Wi-Fi drop and reconnect | While ON with the lid open: turn the phone hotspot/app off for 3 min, then on again | `internet check failed` → `reconnecting` → `Internet is back after …` with no manual action. Repeat after menu › Settings › "Make … reconnect automatically" | `reconnect_after_secs`, `net_check_secs`; whether the profile fix is needed |
| **T7** | Open the lid during the countdown | Let it end with the lid closed, open the lid within 60 s | `session ended`, no sleep | countdown UX |
| **T8** | Crash recovery | While ON: Task Manager › end `working-mode.exe`, then start it again | "Working Mode resumed" and the session continues. Exit normally → the icon goes grey and `--selftest` shows the original plan values | `state.json` recovery |
| **T9** | Reboot recovery | While ON (timer still running), restart Windows | The app autostarts and resumes. If the timer expired during the reboot, it restores instead | autostart |
| **T11** | Real CPU temperature | Install LibreHardwareMonitor, Options › Remote Web Server › Run (port 8085), Options › Run on Windows startup. Then `working-mode.exe --status` | `CPU temperature` shows a number close to LHM's "CPU Package"; `alive` lines show `cputemp=` | `hot_cpu_temp_c` (85), `hot_cpu_max` (60) |
| **T12** | Watchdog | While ON: Task Manager › end `working-mode.exe`, then wait up to 5 min (or `schtasks /run /tn WorkingModeWatchdog`) | Log shows `watchdog: … starting it` then `session resumed`; after a clean stop, `schtasks /query /tn WorkingModeWatchdog` finds nothing | that a crash in the bag is recovered |
| **T13** | Plan switched by something else | While ON: `powercfg /setactive SCHEME_BALANCED` | Within 5 s the log shows `power plan was switched … switching back` and the Working Mode plan is active again | plan drift |
| **T10** | Long run with Claude Code | Start 2–4 h, give Claude Code a long task, close the lid | The task finishes; the log is clean | the real goal |

## Reading results back

Send the log, or ask Claude to read `%LOCALAPPDATA%\WorkingMode\log.txt`. Then update `config.json` defaults in `src/config.rs` so the shipped defaults match what worked.
