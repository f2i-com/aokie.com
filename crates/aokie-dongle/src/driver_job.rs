//! A driver step run off the plugin's command loop: the WinUSB install, its restore and the certificate removal
//! on Windows, the udev rule on Linux.
//!
//! Each waits on a person: Windows' UAC prompt and the elevated helper behind it (a minute or two), or the
//! desktop's password prompt (pkexec). Run on the command loop, one held every other command up for as long, and
//! the setup screen's call to it gave up after the host's 20 seconds: the screen offered Install again while the
//! first helper was still running, and a second click raised a second elevated install. So `dongle.installDriver`
//! (and the others) start the step here and answer at once; `dongle.list` reports [`current`], which the screen
//! polls until the step is done or failed. One step runs at a time: another is refused while one runs, whatever
//! asks for it, and the step itself holds the installer's lock until the helper has exited.

use std::sync::Mutex;

use serde::Serialize;

/// What a driver step is doing, or did last (until the plugin restarts).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverJob {
    /// `install`, `restore` or `removeCerts`.
    pub kind: &'static str,
    pub vid: u16,
    pub pid: u16,
    /// `running`, `done` or `failed`.
    pub state: &'static str,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Why it failed, in the installer's words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What it did, when it is done (the step's own answer, e.g. the driver a restore left the dongle on).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

impl DriverJob {
    pub fn is_running(&self) -> bool {
        self.state == "running"
    }
}

static CURRENT: Mutex<Option<DriverJob>> = Mutex::new(None);

/// The step running now, else the last one (None before the first).
pub fn current() -> Option<DriverJob> {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// What a step is, said to the person who asks for another while it runs.
fn doing(kind: &str) -> &'static str {
    match kind {
        "install" => "installing the dongle's driver",
        "restore" => "restoring the dongle's Windows driver",
        "removeCerts" => "removing its signing certificates",
        _ => "running a driver step",
    }
}

/// Start `work` on a thread of its own and answer with the step as it starts. Refused while another step runs:
/// the person may still be looking at its prompt. A panic in `work` ends the step as failed, never the plugin.
pub fn start<F>(kind: &'static str, vid: u16, pid: u16, work: F) -> Result<DriverJob, String>
where
    F: FnOnce() -> Result<serde_json::Value, String> + Send + 'static,
{
    let job = DriverJob {
        kind,
        vid,
        pid,
        state: "running",
        started_at: now(),
        finished_at: None,
        error: None,
        result: None,
    };
    {
        let mut slot = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(running) = slot.as_ref().filter(|j| j.is_running()) {
            return Err(format!(
                "Aokie is already {} (started {}): wait for it to finish. If Windows or the desktop shows a prompt, answer it first.",
                doing(running.kind),
                running.started_at
            ));
        }
        *slot = Some(job.clone());
    }
    let spawned = std::thread::Builder::new()
        .name(format!("aokie-driver-{kind}"))
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err("the driver step stopped unexpectedly (a panic); see the plugin's log".to_string()));
            finish(outcome);
        });
    if let Err(e) = spawned {
        finish(Err(format!("the driver step could not be started: {e}")));
        return Err(format!("the driver step could not be started: {e}"));
    }
    Ok(job)
}

fn finish(outcome: Result<serde_json::Value, String>) {
    let mut slot = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(job) = slot.as_mut() {
        job.finished_at = Some(now());
        match outcome {
            Ok(result) => {
                job.state = "done";
                job.result = Some(result);
            }
            Err(error) => {
                job.state = "failed";
                job.error = Some(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn wait_until_finished() -> DriverJob {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let job = current().expect("a step was started");
            if !job.is_running() {
                return job;
            }
            assert!(Instant::now() < deadline, "the step never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // One test: the slot is the process's, as it is the plugin's.
    #[test]
    fn one_step_at_a_time_answered_at_once_and_reported_when_done() {
        let (release, wait) = mpsc::channel::<()>();
        let started = start("install", 0x0a5c, 0x21ec, move || {
            wait.recv().ok();
            Ok(serde_json::json!({"backend": "aokie-helper"}))
        })
        .unwrap();
        assert_eq!((started.kind, started.state), ("install", "running"));
        assert!(current().unwrap().is_running(), "reported while it runs");

        // A second click, a second screen or a relayed command: refused while the first runs.
        let refused = start("install", 0x0a5c, 0x21ec, || Ok(serde_json::Value::Null)).unwrap_err();
        assert!(refused.contains("already installing"), "{refused}");
        let refused = start("restore", 0x0a5c, 0x21ec, || Ok(serde_json::Value::Null)).unwrap_err();
        assert!(refused.contains("already installing"), "{refused}");

        release.send(()).unwrap();
        let done = wait_until_finished();
        assert_eq!(done.state, "done");
        assert_eq!(done.result, Some(serde_json::json!({"backend": "aokie-helper"})));
        assert!(done.finished_at.is_some());
        let json = serde_json::to_value(&done).unwrap();
        assert_eq!(json["startedAt"], serde_json::json!(done.started_at));
        assert!(json.get("error").is_none());

        // The next step may start once it is over; a failure is reported with its reason.
        start("restore", 0x0a5c, 0x21ec, || Err("the dongle is still on WinUSB".to_string())).unwrap();
        let failed = wait_until_finished();
        assert_eq!((failed.kind, failed.state), ("restore", "failed"));
        assert_eq!(failed.error.as_deref(), Some("the dongle is still on WinUSB"));

        // A panic in the step fails it; the plugin goes on.
        start("removeCerts", 0, 0, || panic!("boom")).unwrap();
        let panicked = wait_until_finished();
        assert_eq!(panicked.state, "failed");
        assert!(panicked.error.unwrap().contains("panic"));
    }
}
