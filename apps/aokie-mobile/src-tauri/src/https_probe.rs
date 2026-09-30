//! SP-M0 HTTPS probe: a debug and test hook, compiled only with the cargo feature `https-probe`.
//!
//! It exists to answer one question on a device or emulator: does a real HTTPS request from the
//! Rust side succeed, and if not, with what exact error? It is never part of a shipped build: the
//! feature is off by default. Every result goes to logcat under the tag `AokieProbe`, one line per
//! probe, so an install and `adb logcat -s AokieProbe` is the whole procedure. Nothing here reaches
//! the WebView or a Tauri command.
//!
//! The probes build their client exactly the way the app's own clients are built
//! (`managed_auth::native_client`: no redirects, a timeout, the crate's `rustls` feature), so what
//! they show is what discovery, OAuth, the mobile API and the relay carrier would get.

use std::time::{Duration, Instant};

use reqwest::redirect::Policy;

/// The logcat tag every probe line carries.
pub const TAG: &str = "AokieProbe";

/// What a healthy platform verifier does with a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// A publicly trusted certificate chain: the request must complete.
    Accept,
    /// A chain that no system trust anchor vouches for: the request must fail. This is the
    /// negative control that separates "the verifier works" from "nothing is verified".
    Reject,
}

/// One probe target: a label for the log, a URL and the expected outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub label: &'static str,
    pub url: String,
    pub expect: Expect,
}

/// The fixed probe list. `AOKIE_PROBE_LOCAL_URL` (read at build time) adds a local HTTPS endpoint
/// signed by a scratch CA that the device does not trust: it must be rejected as well.
pub fn targets() -> Vec<Target> {
    let mut list = vec![
        Target {
            label: "public",
            url: "https://example.com/".into(),
            expect: Expect::Accept,
        },
        Target {
            label: "expired",
            url: "https://expired.badssl.com/".into(),
            expect: Expect::Reject,
        },
    ];
    if let Some(local) = option_env!("AOKIE_PROBE_LOCAL_URL") {
        if !local.is_empty() {
            list.push(Target {
                label: "scratch-ca",
                url: local.to_string(),
                expect: Expect::Reject,
            });
        }
    }
    list
}

/// `error <- cause <- root cause`: reqwest's own `Display` stops at "error sending request", the
/// certificate or handshake reason is several sources down.
pub fn describe_error(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        out.push_str(" <- ");
        out.push_str(&next.to_string());
        source = next.source();
    }
    out
}

/// Writes one line to logcat (and stderr off Android, so a host run shows it too).
pub fn logcat(message: &str) {
    #[cfg(target_os = "android")]
    {
        use std::ffi::{c_char, c_int, CString};

        #[link(name = "log")]
        extern "C" {
            fn __android_log_write(priority: c_int, tag: *const c_char, text: *const c_char)
                -> c_int;
        }
        const ANDROID_LOG_INFO: c_int = 4;
        let tag = CString::new(TAG).expect("tag has no NUL");
        // Logcat lines are cut at about 4 KiB and a NUL would end one early.
        let text = CString::new(message.replace('\0', " ")).expect("NUL removed");
        // SAFETY: both pointers are valid NUL-terminated strings for the duration of the call.
        unsafe {
            __android_log_write(ANDROID_LOG_INFO, tag.as_ptr(), text.as_ptr());
        }
    }
    #[cfg(not(target_os = "android"))]
    eprintln!("{TAG}: {message}");
}

async fn probe_once(target: Target) -> String {
    let started = Instant::now();
    let client = match reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(error) => return format!("client_build_failed error=\"{}\"", describe_error(&error)),
    };
    match client.get(&target.url).send().await {
        Ok(response) => format!(
            "request_ok status={} ms={}",
            response.status().as_u16(),
            started.elapsed().as_millis()
        ),
        Err(error) => format!(
            "request_failed ms={} error=\"{}\"",
            started.elapsed().as_millis(),
            describe_error(&error)
        ),
    }
}

/// Runs every probe once, each in its own task so a panic inside the TLS stack (the unfixed
/// Android verifier panics on its first handshake) is reported as a result instead of taking the
/// probe down with it.
async fn run() {
    logcat(&format!(
        "start version={} os={} features=https-probe",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    ));
    for target in targets() {
        let label = target.label;
        let url = target.url.clone();
        let expect = target.expect;
        let outcome = match tauri::async_runtime::spawn(probe_once(target)).await {
            Ok(line) => line,
            Err(join_error) => format!("task_failed error=\"{join_error}\""),
        };
        let got_ok = outcome.starts_with("request_ok");
        let verdict = match (expect, got_ok) {
            (Expect::Accept, true) | (Expect::Reject, false) => "as_expected",
            (Expect::Accept, false) => "UNEXPECTED_FAILURE",
            (Expect::Reject, true) => "UNEXPECTED_SUCCESS",
        };
        logcat(&format!(
            "probe label={label} url={url} expect={expect:?} verdict={verdict} {outcome}"
        ));
    }
    logcat("done");
}

/// Starts the probe on Tauri's runtime and routes panics to logcat. Called once from `setup`.
pub fn spawn() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        logcat(&format!("panic {info}"));
        previous(info);
    }));
    tauri::async_runtime::spawn(run());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|inner| inner as _)
        }
    }

    #[test]
    fn describe_error_lists_every_cause_from_outermost_to_root() {
        let error = Layer(
            "error sending request",
            Some(Box::new(Layer(
                "client error (Connect)",
                Some(Box::new(Layer("invalid peer certificate: UnknownIssuer", None))),
            ))),
        );
        assert_eq!(
            describe_error(&error),
            "error sending request <- client error (Connect) <- invalid peer certificate: UnknownIssuer"
        );
    }

    #[test]
    fn the_probe_list_keeps_a_positive_and_a_negative_control() {
        let list = targets();
        assert!(
            list.iter()
                .any(|t| t.expect == Expect::Accept && t.url.starts_with("https://")),
            "the probe needs one publicly trusted target"
        );
        assert!(
            list.iter().any(|t| t.expect == Expect::Reject),
            "without a target that must fail, a verifier that accepts everything would pass"
        );
        assert!(list.iter().all(|t| t.url.starts_with("https://")));
    }
}
