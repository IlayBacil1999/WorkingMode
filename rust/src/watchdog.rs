//! Out-of-process safety net. While a session is on, a per-user scheduled
//! task (no admin) runs `working-mode.exe --watchdog` every 5 minutes. If a
//! session is saved but the app is gone (crash, killed), it starts the app
//! again, which resumes the session or puts the normal plan back. Without
//! this, a dead app would leave "lid does nothing" on, with no timer and no
//! battery floor, until someone started it by hand.

use crate::app::run_hidden;
use crate::config::{data_dir, log, SavedState};

const TASK: &str = "WorkingModeWatchdog";

/// Creates (or replaces) the task. Called when a session starts.
pub fn install() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let xml = task_xml(&exe.display().to_string());
    let path = data_dir().join("watchdog-task.xml");
    // schtasks wants the encoding the XML declares: UTF-16 with a BOM
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    if std::fs::write(&path, bytes).is_err() {
        return false;
    }
    let ok = run_hidden("schtasks.exe", &["/create", "/tn", TASK, "/xml", &path.display().to_string(), "/f"]).unwrap_or(false);
    let _ = std::fs::remove_file(&path);
    ok
}

/// Called after a clean end: nothing left to guard.
pub fn remove() {
    let _ = run_hidden("schtasks.exe", &["/delete", "/tn", TASK, "/f"]);
}

/// The task's job. Returns the process exit code.
pub fn run(app_running: bool) -> i32 {
    if app_running {
        return 0;
    }
    if SavedState::load().is_none() {
        // nothing to guard (the app ended cleanly but couldn't remove the task)
        remove();
        return 0;
    }
    log("watchdog: a session is saved but the app isn't running - starting it");
    match std::env::current_exe().and_then(|exe| std::process::Command::new(exe).spawn()) {
        Ok(_) => 0,
        Err(e) => {
            log(&format!("watchdog: couldn't start the app: {e}"));
            1
        }
    }
}

fn task_xml(exe: &str) -> String {
    let exe = exe.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Working Mode: restarts the app if it dies during a session.</Description></RegistrationInfo>
  <Triggers>
    <TimeTrigger>
      <StartBoundary>2026-01-01T00:00:00</StartBoundary>
      <Repetition><Interval>PT5M</Interval></Repetition>
    </TimeTrigger>
  </Triggers>
  <Principals><Principal id="Author"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <ExecutionTimeLimit>PT1M</ExecutionTimeLimit>
    <Hidden>true</Hidden>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>--watchdog</Arguments></Exec></Actions>
</Task>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_xml_escapes_the_path_and_runs_on_battery() {
        let x = task_xml(r"C:\Tools & Stuff\working-mode.exe");
        assert!(x.contains(r"<Command>C:\Tools &amp; Stuff\working-mode.exe</Command>"));
        assert!(x.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(x.contains("<Arguments>--watchdog</Arguments>"));
    }
}
