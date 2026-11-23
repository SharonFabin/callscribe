//! callscribe: call recording for Linux (in progress). For now, the microphone event logger.
//!
//! `callscribe log-mic` writes every change to the list of apps capturing audio to
//! `$XDG_STATE_HOME/callscribe/mic-events.jsonl`, to learn how call apps behave (mute, screen share, leave)
//! before the recorder decides when a call starts and ends.

mod mic;

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("callscribe")
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// A line of `pactl subscribe` that may change who captures audio. Opening, closing, pausing, muting and
/// moving a stream are all source-output events; source events are skipped (a call's automatic gain
/// control changes the mic volume constantly).
fn relevant(event: &str) -> bool {
    event.contains(" on source-output #")
}

fn log_mic() -> io::Result<()> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join("mic-events.jsonl");
    let mut log = OpenOptions::new().create(true).append(true).open(&path)?;
    eprintln!("logging capture streams to {}", path.display());

    // subscribe before the first snapshot; a stream opened in the few ms while pactl connects is
    // picked up at the next event
    let mut sub = Command::new("pactl").arg("subscribe").stdout(Stdio::piped()).spawn()?;
    let events = BufReader::new(sub.stdout.take().expect("piped stdout"));
    let mut before = mic::snapshot()?;
    for c in &before {
        writeln!(log, "{}", serde_json::json!({"ts": now_ms(), "kind": "present", "capture": c}))?;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in events.lines().map_while(Result::ok) {
            if relevant(&line) && tx.send(()).is_err() {
                break;
            }
        }
    });
    while rx.recv().is_ok() {
        // a burst of events (one stream opening sends several) gets one snapshot, not one each
        std::thread::sleep(Duration::from_millis(50));
        while rx.try_recv().is_ok() {}
        let after = mic::snapshot()?;
        for change in mic::diff(&before, &after) {
            writeln!(log, "{}", serde_json::json!({"ts": now_ms(), "change": change}))?;
        }
        before = after;
    }
    // pactl exits when PipeWire restarts: fail so systemd restarts us against the new server
    Err(io::Error::other(format!("pactl subscribe ended: {}", sub.wait()?)))
}

fn main() -> ExitCode {
    let result = match std::env::args().nth(1).as_deref() {
        Some("log-mic") => log_mic(),
        _ => {
            eprintln!("usage: callscribe log-mic");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("callscribe: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn relevant_events() {
        assert!(super::relevant("Event 'new' on source-output #206279"));
        assert!(!super::relevant("Event 'change' on source #35619"));
        assert!(!super::relevant("Event 'change' on sink #12"));
        assert!(!super::relevant("Event 'new' on client #206278"));
    }
}
