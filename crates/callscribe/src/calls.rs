//! Which capture streams are a call, with Yogurt's rules (its watch.ts and call-apps tables):
//! a call app on the microphone is a call; a browser counts only while one of its windows looks
//! like a web call (or, once calendars are wired in, a meeting with a link is on).

use crate::mic::Capture;

pub struct CallApp {
    pub name: &'static str,
    ids: &'static [&'static str],
    pub browser: bool,
}

const fn app(name: &'static str, ids: &'static [&'static str], browser: bool) -> CallApp {
    CallApp { name, ids, browser }
}

/// Yogurt's Linux call apps, matched against the stream's process binary.
pub const APPS: &[CallApp] = &[
    app("Zoom", &["zoom", "us.zoom.Zoom"], false),
    app("Teams", &["teams-for-linux", "com.github.IsmaelMartinez.teams_for_linux"], false),
    app("Slack", &["slack", "com.slack.Slack"], false),
    app("Discord", &["Discord", "discord", "com.discordapp.Discord"], false),
    app("Webex", &["CiscoCollabHost", "webex"], false),
    app("Chrome", &["chrome", "google-chrome", "com.google.Chrome"], true),
    app("Chromium", &["chromium", "chromium-browser", "org.chromium.Chromium"], true),
    app("Firefox", &["firefox", "firefox-bin", "org.mozilla.firefox"], true),
    app("Brave", &["brave", "com.brave.Browser"], true),
    app("Edge", &["msedge", "com.microsoft.Edge"], true),
    app("Vivaldi", &["vivaldi-bin"], true),
    app("Opera", &["opera"], true),
];

/// The call app a capture stream belongs to: by its process binary, else its application name.
pub fn call_app(c: &Capture) -> Option<&'static CallApp> {
    if c.monitor {
        return None;
    }
    let id = c.binary.as_deref().unwrap_or(&c.app);
    APPS.iter().find(|a| a.ids.iter().any(|i| id == *i || id.starts_with(&format!("{i}."))))
}

/// Yogurt's WEB_CALL, `/\b(meet|teams|zoom)\b/i`: a browser window title that looks like a call.
pub fn web_call_title(title: &str) -> bool {
    title
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| ["meet", "teams", "zoom"].iter().any(|k| w.eq_ignore_ascii_case(k)))
}

/// Call apps on the microphone now, by name, without repeats. `browser_call(app)` says whether that
/// browser shows a call right now.
pub fn on_call(captures: &[Capture], browser_call: impl Fn(&CallApp) -> bool) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = captures
        .iter()
        .filter_map(call_app)
        .filter(|a| !a.browser || browser_call(a))
        .map(|a| a.name)
        .collect();
    let mut seen = Vec::new();
    names.retain(|n| !seen.contains(n) && { seen.push(*n); true });
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(binary: &str, monitor: bool) -> Capture {
        Capture {
            id: 1,
            app: "Brave input".into(),
            binary: Some(binary.into()),
            pid: None,
            source: String::new(),
            monitor,
            corked: false,
            mute: false,
        }
    }

    #[test]
    fn apps_by_binary() {
        assert_eq!(call_app(&capture("brave", false)).map(|a| a.name), Some("Brave"));
        assert_eq!(call_app(&capture("zoom", false)).map(|a| a.name), Some("Zoom"));
        assert!(call_app(&capture("python3.14", false)).is_none(), "dictation is not a call");
        assert!(call_app(&capture("pw-record", false)).is_none());
        assert!(call_app(&capture("brave", true)).is_none(), "a speaker monitor is not the mic");
    }

    #[test]
    fn web_call_titles() {
        assert!(web_call_title("Meet - abc-defg-hij - Brave"));
        assert!(web_call_title("Google Meet - Brave"));
        assert!(web_call_title("Zoom Meeting"));
        assert!(!web_call_title("Meeting notes - Google Docs"), "whole words only, as \\b in Yogurt");
        assert!(!web_call_title("YouTube - Brave"));
    }

    #[test]
    fn browsers_need_a_call_window() {
        let two = [capture("brave", false), capture("brave", false)];
        assert_eq!(on_call(&two, |_| true), ["Brave"]);
        assert!(on_call(&two, |_| false).is_empty());
        assert_eq!(on_call(&[capture("slack", false)], |_| false), ["Slack"]);
        let mixed = [capture("brave", false), capture("zoom", false), capture("brave", false)];
        assert_eq!(on_call(&mixed, |_| true), ["Brave", "Zoom"]);
    }
}
