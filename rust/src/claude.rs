//! "While Claude works": Claude Code hooks run `working-mode.exe --claude-hook`
//! on its events, and the tray app counts the Claude sessions that are busy.
//!
//! Hooks run async, so several can run at once for one session. Each fact is
//! its own file holding a timestamp, and each event writes only its own
//! files, so no read-modify-write can lose another hook's update:
//!   <session>.turn  a turn is running (UserPromptSubmit .. Stop)
//!   <session>.bg    the turn stopped but left background work running
//!   <session>.last  the last event of any kind (tools in subagents, too)

use crate::config::{data_dir, write_atomic};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// A turn with no event for this long was interrupted (that sends no "Stop")
/// or died. A turn calls tools every few minutes; one Bash call is <= 10 min.
const STALE_TURN_SECS: u64 = 30 * 60;
/// Background work (a long build, a subagent) may run for hours without a
/// hook event; when it ends, Claude takes a new turn, which ends with a Stop.
const STALE_BACKGROUND_SECS: u64 = 4 * 3600;
/// Activity this recent counts as busy even outside a turn: subagents keep
/// calling tools after the turn's Stop.
const RECENT_SECS: u64 = 120;
/// Sessions untouched this long are deleted.
const FORGET_SECS: u64 = 24 * 3600;

pub fn dir() -> PathBuf {
    data_dir().join("claude")
}

/// The hook: one event as JSON on stdin. Always exits 0 and prints nothing,
/// so it can never block or confuse Claude.
pub fn hook() -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
    if let Ok(v) = serde_json::from_str::<Value>(&input) {
        if let (Some(id), Some(event)) = (v["session_id"].as_str(), v["hook_event_name"].as_str()) {
            let d = dir();
            let _ = std::fs::create_dir_all(&d);
            record(&d, id, event, waits_for_background(&v), crate::config::now());
        }
    }
    0
}

/// Session ids become file names: only plain id characters get through.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// "Stop" lists background tasks (shells, subagents): while any still runs,
/// the session is paused, not done.
fn waits_for_background(v: &Value) -> bool {
    const ENDED: [&str; 6] = ["completed", "failed", "killed", "stopped", "cancelled", "error"];
    v["background_tasks"]
        .as_array()
        .is_some_and(|tasks| tasks.iter().any(|t| !t["status"].as_str().is_some_and(|s| ENDED.contains(&s))))
}

fn record(dir: &Path, id: &str, event: &str, background: bool, now: u64) {
    if !valid_id(id) {
        return;
    }
    let file = |kind: &str| dir.join(format!("{id}.{kind}"));
    let set = |kind: &str| {
        write_atomic(&file(kind), &now.to_string());
    };
    let clear = |kind: &str| {
        let _ = std::fs::remove_file(file(kind));
    };
    match event {
        "SessionEnd" => {
            for kind in ["turn", "bg", "last"] {
                clear(kind);
            }
            return;
        }
        "UserPromptSubmit" => {
            set("turn");
            clear("bg");
        }
        "Stop" => {
            clear("turn");
            if background {
                set("bg");
            } else {
                clear("bg");
            }
        }
        _ => {}
    }
    set("last");
}

/// What one session's files say.
#[derive(Default)]
struct Marks {
    turn: Option<u64>,
    bg: Option<u64>,
    last: Option<u64>,
}

/// How many Claude sessions are working right now. Also forgets old ones.
pub fn busy_sessions(dir: &Path, now: u64) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut sessions: HashMap<String, Marks> = HashMap::new();
    for e in entries.flatten() {
        let path = e.path();
        let (Some(id), Some(kind)) = (path.file_stem().and_then(|s| s.to_str()), path.extension().and_then(|s| s.to_str())) else {
            continue;
        };
        let Some(t) = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u64>().ok()) else { continue };
        let m = sessions.entry(id.to_string()).or_default();
        match kind {
            "turn" => m.turn = Some(t),
            "bg" => m.bg = Some(t),
            "last" => m.last = Some(t),
            _ => {}
        }
    }
    let mut busy = 0;
    for (id, m) in sessions {
        // a turn or a wait is alive while events keep coming
        let latest = [m.turn, m.bg, m.last].into_iter().flatten().max().unwrap_or(0);
        let age = now.saturating_sub(latest);
        if (m.turn.is_some() && age < STALE_TURN_SECS) || (m.bg.is_some() && age < STALE_BACKGROUND_SECS) || age < RECENT_SECS {
            busy += 1;
        } else if age > FORGET_SECS {
            for kind in ["turn", "bg", "last"] {
                let _ = std::fs::remove_file(dir.join(format!("{id}.{kind}")));
            }
        }
    }
    busy
}

/// The Claude Code events the hook listens to.
const EVENTS: [&str; 6] = ["UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop", "SubagentStop", "SessionEnd"];

/// Claude Code's user settings, where the hooks go.
pub fn settings_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".claude").join("settings.json"))
}

fn is_ours(handler: &Value) -> bool {
    handler["command"].as_str().is_some_and(|c| c.contains("--claude-hook"))
}

/// `"C:/path/working-mode.exe" --claude-hook`, quoted for bash: inside double
/// quotes only $ and ` mean something, and a Windows path can't hold a quote.
fn command(exe: &Path) -> String {
    let path = exe.display().to_string().replace('\\', "/").replace('$', "\\$").replace('`', "\\`");
    format!("\"{path}\" --claude-hook")
}

/// Claude Code runs "shell": "bash" hooks with Git Bash; without it they fail.
pub fn git_bash_found() -> bool {
    let env = std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH").map(PathBuf::from);
    let installs = ["ProgramFiles", "ProgramW6432", "LOCALAPPDATA"]
        .iter()
        .filter_map(|v| std::env::var_os(v).map(PathBuf::from))
        .flat_map(|d| [d.join(r"Git\bin\bash.exe"), d.join(r"Programs\Git\bin\bash.exe")]);
    env.into_iter().chain(installs).any(|p| p.is_file())
}

/// The hook command for this exe. Always under Git Bash: PowerShell neither
/// hands its stdin to a windowless exe nor waits for one (tried).
fn handler(exe: &Path) -> Value {
    serde_json::json!({
        "type": "command",
        "shell": "bash",
        "command": command(exe),
        "async": true,
        "timeout": 10,
    })
}

fn read_settings(settings: &Path) -> Result<Value, String> {
    let text = match std::fs::read_to_string(settings) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".into(),
        Err(e) => return Err(format!("can't read {}: {e}", settings.display())),
    };
    serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("{} isn't valid JSON ({e}) - left it alone", settings.display()))
}

/// Whether every event has our hook, pointing at `exe` (a moved exe means
/// hooks that run nothing: set up again).
pub fn hooks_installed(settings: &Path, exe: &Path) -> bool {
    let Ok(root) = read_settings(settings) else { return false };
    let want = command(exe);
    let current = |h: &Value| h["command"].as_str() == Some(want.as_str());
    EVENTS.iter().all(|e| {
        root["hooks"][e]
            .as_array()
            .is_some_and(|groups| groups.iter().any(|g| g["hooks"].as_array().is_some_and(|hs| hs.iter().any(current))))
    })
}

/// Whether any of our hooks is there, current or not.
pub fn any_hooks(settings: &Path) -> bool {
    read_settings(settings).is_ok_and(|root| {
        root["hooks"].as_object().is_some_and(|events| {
            events.values().filter_map(Value::as_array).flatten().any(|g| g["hooks"].as_array().is_some_and(|hs| hs.iter().any(is_ours)))
        })
    })
}

/// Adds our hooks to Claude Code's settings (`exe` = Some) or takes them out
/// (None). Everything else in the file stays as it was, in its order; the
/// file before the change is kept next to it.
pub fn set_hooks(settings: &Path, exe: Option<&Path>) -> Result<(), String> {
    let mut root = read_settings(settings)?;
    let Some(obj) = root.as_object_mut() else { return Err(format!("{} isn't a JSON object - left it alone", settings.display())) };
    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let Some(hooks) = hooks.as_object_mut() else { return Err("its \"hooks\" isn't an object - left it alone".into()) };
    for event in EVENTS {
        let groups = hooks.entry(event).or_insert_with(|| serde_json::json!([]));
        let Some(groups) = groups.as_array_mut() else { return Err(format!("its hooks.{event} isn't a list - left it alone")) };
        // drop ours (an older exe path, too), and groups that leaves empty
        for g in groups.iter_mut() {
            if let Some(hs) = g["hooks"].as_array_mut() {
                hs.retain(|h| !is_ours(h));
            }
        }
        groups.retain(|g| g["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
        if let Some(exe) = exe {
            groups.push(serde_json::json!({"hooks": [handler(exe)]}));
        }
        if groups.is_empty() {
            hooks.shift_remove(event);
        }
    }
    if hooks.is_empty() {
        obj.shift_remove("hooks");
    }
    let out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    if settings.exists() {
        // the file as it was before Working Mode first touched it: kept once, never overwritten
        let backup = settings.with_extension("json.before-working-mode");
        if !backup.exists() {
            std::fs::copy(settings, &backup).map_err(|e| format!("couldn't back up {}: {e}", settings.display()))?;
        }
    } else if let Some(parent) = settings.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // a symlinked settings file (dotfiles) is written through, not replaced by a plain file
    let link = std::fs::symlink_metadata(settings).is_ok_and(|m| m.file_type().is_symlink());
    let written = if link { std::fs::write(settings, &out).is_ok() } else { write_atomic(settings, &out) };
    if !written {
        return Err(format!("couldn't write {}", settings.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wm-claude-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_turn_is_busy_until_stop_plus_a_little() {
        let d = temp_dir("turn");
        record(&d, "s1", "UserPromptSubmit", false, 1000);
        assert_eq!(busy_sessions(&d, 1000 + 1200), 1, "a long tool call with no events is still busy");
        record(&d, "s1", "PostToolUse", false, 5000);
        record(&d, "s1", "Stop", false, 5010);
        assert_eq!(busy_sessions(&d, 5010 + 60), 1, "just stopped: subagents may still run");
        assert_eq!(busy_sessions(&d, 5010 + 300), 0);
    }

    #[test]
    fn stop_with_background_work_still_running_stays_busy() {
        let d = temp_dir("background");
        let running = serde_json::json!({"background_tasks": [{"id": "b1", "type": "shell", "status": "running"}]});
        let done = serde_json::json!({"background_tasks": [{"id": "b1", "type": "shell", "status": "completed"}]});
        assert!(waits_for_background(&running));
        assert!(!waits_for_background(&done));
        assert!(!waits_for_background(&serde_json::json!({})));
        record(&d, "s1", "UserPromptSubmit", false, 0);
        record(&d, "s1", "Stop", true, 10);
        assert_eq!(busy_sessions(&d, 600), 1);
        // a long build in the background: still busy after 3 hours without events
        assert_eq!(busy_sessions(&d, 3 * 3600), 1);
        record(&d, "s1", "Stop", false, 3 * 3600 + 10);
        assert_eq!(busy_sessions(&d, 3 * 3600 + 300), 0);
    }

    #[test]
    fn a_late_hook_cannot_revive_a_turn_or_an_ended_session_for_long() {
        let d = temp_dir("late");
        record(&d, "s1", "UserPromptSubmit", false, 0);
        record(&d, "s1", "Stop", false, 10);
        // a subagent's async hook landing after the Stop
        record(&d, "s1", "PostToolUse", false, 11);
        assert_eq!(busy_sessions(&d, 11 + RECENT_SECS), 0);
        record(&d, "s1", "SessionEnd", false, 20);
        record(&d, "s1", "PostToolUse", false, 21);
        assert_eq!(busy_sessions(&d, 21 + RECENT_SECS), 0);
    }

    #[test]
    fn subagent_tools_after_stop_count_as_busy() {
        let d = temp_dir("subagent");
        record(&d, "s1", "UserPromptSubmit", false, 0);
        record(&d, "s1", "Stop", false, 10);
        record(&d, "s1", "PreToolUse", false, 900);
        assert_eq!(busy_sessions(&d, 950), 1);
        assert_eq!(busy_sessions(&d, 1200), 0);
    }

    #[test]
    fn a_dead_session_stops_counting_and_is_forgotten() {
        let d = temp_dir("dead");
        record(&d, "s1", "UserPromptSubmit", false, 0);
        assert_eq!(busy_sessions(&d, STALE_TURN_SECS + 1), 0);
        assert!(d.join("s1.last").exists());
        assert_eq!(busy_sessions(&d, FORGET_SECS + 1), 0);
        assert!(!d.join("s1.last").exists() && !d.join("s1.turn").exists());
    }

    #[test]
    fn sessions_are_counted_separately_and_end_cleans_up() {
        let d = temp_dir("many");
        record(&d, "a", "UserPromptSubmit", false, 0);
        record(&d, "b", "UserPromptSubmit", false, 0);
        assert_eq!(busy_sessions(&d, 10), 2);
        record(&d, "a", "SessionEnd", false, 20);
        assert_eq!(busy_sessions(&d, 30), 1);
        assert!(std::fs::read_dir(&d).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with("a.")));
    }

    #[test]
    fn hooks_merge_into_settings_and_come_out_cleanly() {
        let d = temp_dir("settings");
        let path = d.join("settings.json");
        let before = r#"{"model": "opus", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "notify.exe"}]}]}, "env": {"A": "1"}}"#;
        std::fs::write(&path, before).unwrap();
        let exe = Path::new(r"C:\Users\me\WorkingMode\working-mode.exe");
        set_hooks(&path, Some(exe)).unwrap();
        assert!(hooks_installed(&path, exe));
        assert!(!hooks_installed(&path, Path::new("C:/elsewhere/working-mode.exe")), "a moved exe needs a new setup");
        let text = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 2, "the user's own Stop hook stays");
        assert_eq!(v["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "\"C:/Users/me/WorkingMode/working-mode.exe\" --claude-hook");
        // the user's keys keep their order
        let at = |k: &str| text.find(&format!("\"{k}\"")).unwrap();
        assert!(at("model") < at("hooks") && at("hooks") < at("env"));
        // installing again doesn't duplicate
        set_hooks(&path, Some(exe)).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 2);
        // removing leaves the user's file as it was
        set_hooks(&path, None).unwrap();
        assert!(!hooks_installed(&path, exe));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v, serde_json::from_str::<Value>(before).unwrap());
        assert!(d.join("settings.json.before-working-mode").exists());
    }

    #[test]
    fn broken_settings_are_left_alone() {
        let d = temp_dir("broken");
        let path = d.join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(set_hooks(&path, Some(Path::new("C:/wm.exe"))).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        let missing = d.join("new").join("settings.json");
        set_hooks(&missing, Some(Path::new("C:/Program Files/wm.exe"))).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&missing).unwrap()).unwrap();
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "\"C:/Program Files/wm.exe\" --claude-hook");
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["shell"], "bash");
    }

    #[test]
    fn odd_session_ids_are_ignored() {
        let d = temp_dir("ids");
        for id in ["", "../x", r"..\x", "a/b", "c:x", &"x".repeat(101)] {
            record(&d, id, "UserPromptSubmit", false, 0);
        }
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
    }
}
