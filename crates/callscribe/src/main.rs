//! callscribe: records calls on Linux and hands them to local transcription.
//!
//! `callscribe run` is the always-on daemon, with Yogurt's behaviour: when a call app opens the
//! microphone it offers to record (a card at the top of the screen; nothing records without a click),
//! and it stops on its own once the call app has been off the microphone for 4 seconds. Each call is
//! a folder in ~/Meetings with me.ogg, them.ogg and meta.json, transcribed right after the call.
//! Every change in who captures audio also goes to `$XDG_STATE_HOME/callscribe/mic-events.jsonl`.

mod calls;
mod mic;
mod recorder;
mod tray;

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use recorder::Recording;

/// Yogurt's CALL_GONE_MS: how long the call app must be off the microphone before the call is over.
const CALL_GONE: Duration = Duration::from_secs(4);

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/state")).join("callscribe")
}

fn meetings_dir() -> PathBuf {
    std::env::var_os("CALLSCRIBE_MEETINGS").map(PathBuf::from).unwrap_or_else(|| home().join("Meetings"))
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

enum Event {
    Mic(Vec<mic::Capture>),
    /// `pactl subscribe` ended (PipeWire restarted) or a snapshot failed
    MicLost(String),
    /// a line the widget printed: record, dismiss or stop
    Widget(u64, String),
    WidgetGone(u64),
    Tray(tray::Action),
}

/// Watches capture streams: logs each change and sends the new list.
fn watch_mic(tx: Sender<Event>) -> io::Result<()> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let mut log = OpenOptions::new().create(true).append(true).open(dir.join("mic-events.jsonl"))?;
    // subscribe before the first snapshot; a stream opened in the few ms while pactl connects is
    // picked up at the next event
    let mut sub = Command::new("pactl").arg("subscribe").stdout(Stdio::piped()).spawn()?;
    let events = BufReader::new(sub.stdout.take().expect("piped stdout"));
    let mut before = mic::snapshot()?;
    for c in &before {
        writeln!(log, "{}", serde_json::json!({"ts": now_ms(), "kind": "present", "capture": c}))?;
    }
    let _ = tx.send(Event::Mic(before.clone()));
    let (ping, pings) = mpsc::channel();
    std::thread::spawn(move || {
        for line in events.lines().map_while(Result::ok) {
            if relevant(&line) && ping.send(()).is_err() {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        let result = (|| -> io::Result<()> {
            while pings.recv().is_ok() {
                // a burst of events (one stream opening sends several) gets one snapshot, not one each
                std::thread::sleep(Duration::from_millis(50));
                while pings.try_recv().is_ok() {}
                let after = mic::snapshot()?;
                for change in mic::diff(&before, &after) {
                    writeln!(log, "{}", serde_json::json!({"ts": now_ms(), "change": change}))?;
                }
                before = after.clone();
                if tx.send(Event::Mic(after)).is_err() {
                    return Ok(());
                }
            }
            Err(io::Error::other(format!("pactl subscribe ended: {:?}", sub.wait())))
        })();
        if let Err(e) = result {
            let _ = tx.send(Event::MicLost(e.to_string()));
        }
    });
    Ok(())
}

/// Whether a browser has a window that looks like a call (Yogurt asks the browser for its front tab's
/// title; Hyprland lists every window's). Off Hyprland it can't tell, so any browser mic use counts.
fn browser_call_window(app: &calls::CallApp) -> bool {
    let Ok(out) = Command::new("hyprctl").args(["clients", "-j"]).output() else { return true };
    let Ok(windows) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else { return true };
    let name = app.name.to_lowercase();
    windows.iter().any(|w| {
        let field = |k: &str| w.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_lowercase();
        field("class").contains(&name) && calls::web_call_title(&field("title"))
    })
}

/// The floating card: an offer to record that turns into the recording pill.
struct Widget {
    child: Child,
    id: u64,
}

impl Widget {
    fn spawn(id: u64, title: &str, tx: Sender<Event>) -> io::Result<Widget> {
        let exe = std::env::current_exe()?.with_file_name("callscribe-widget");
        let mut child = Command::new(exe).args(["--offer", "--title", title]).stdout(Stdio::piped()).spawn()?;
        let out: ChildStdout = child.stdout.take().expect("piped stdout");
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(Event::Widget(id, line.trim().to_owned()));
            }
            let _ = tx.send(Event::WidgetGone(id));
        });
        Ok(Widget { child, id })
    }

    fn close(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Active {
    rec: Recording,
    widget: Option<Widget>,
    /// the call app holds the microphone (as of the last change)
    on_mic: bool,
    /// when it last did
    last_on_mic: Instant,
}

impl Active {
    /// Yogurt's callGone: the call app has been off the microphone for CALL_GONE.
    fn call_gone(&mut self, now: Instant) -> bool {
        if self.on_mic {
            self.last_on_mic = now;
        }
        now.duration_since(self.last_on_mic) >= CALL_GONE
    }
}

struct Daemon {
    tx: Sender<Event>,
    tray: Sender<tray::Status>,
    next_id: u64,
    mic_apps: Vec<String>,
    /// the offer card on screen, and the call app it is for
    offer: Option<(Widget, &'static str)>,
    /// apps whose offer was closed: not offered again until they leave the microphone
    dismissed: HashSet<&'static str>,
    active: Option<Active>,
    /// the capture streams as of the last change
    captures: Vec<mic::Capture>,
}

impl Daemon {
    fn show(&self) {
        let recording = self.active.as_ref().map(|a| a.rec.title.clone());
        let _ = self.tray.send(tray::Status { mic_apps: self.mic_apps.clone(), recording });
    }

    fn on_mic(&mut self, captures: &[mic::Capture]) {
        let mut apps: Vec<String> =
            captures.iter().filter(|c| !c.monitor && !c.app.is_empty()).map(|c| c.app.clone()).collect();
        apps.sort();
        apps.dedup();
        self.mic_apps = apps;
        self.captures = captures.to_vec();
        // the window title only decides whether to offer: once recording (or dismissed), a browser call
        // lasts while the browser holds the mic, whichever tab is in front
        let on_mic = calls::on_call(captures, |_| true);
        if let Some(a) = &mut self.active {
            a.on_mic = on_mic.contains(&a.rec.app);
            a.call_gone(Instant::now()); // marks the moment it left
        } else {
            self.dismissed.retain(|app| on_mic.contains(app));
            let on_call = calls::on_call(captures, browser_call_window);
            let wanted = on_call.iter().copied().find(|app| !self.dismissed.contains(app));
            match (&self.offer, wanted) {
                (Some((_, shown)), Some(app)) if *shown == app => {}
                (_, Some(app)) => {
                    self.close_offer();
                    self.next_id += 1;
                    match Widget::spawn(self.next_id, &format!("{app} call"), self.tx.clone()) {
                        Ok(w) => self.offer = Some((w, app)),
                        Err(e) => eprintln!("can't show the offer: {e}"),
                    }
                }
                (Some(_), None) => self.close_offer(),
                (None, None) => {}
            }
        }
        self.show();
    }

    /// A browser on the mic whose window doesn't (yet) look like a call: its title may change without
    /// any audio event (a lobby page turning into the meeting), so look again every couple of seconds.
    fn browser_waiting(&self) -> bool {
        self.active.is_none()
            && self.offer.is_none()
            && self.captures.iter().filter_map(calls::call_app).any(|a| a.browser && !self.dismissed.contains(a.name))
    }

    fn close_offer(&mut self) {
        if let Some((w, _)) = self.offer.take() {
            w.close();
        }
    }

    fn on_widget(&mut self, id: u64, line: &str) {
        let offered = self.offer.as_ref().is_some_and(|(w, _)| w.id == id);
        match line {
            "record" if offered => {
                let (widget, app) = self.offer.take().expect("offered");
                match Recording::start(&meetings_dir(), &format!("{app} call"), app) {
                    Ok(rec) => {
                        eprintln!("recording {}", rec.folder.display());
                        let last_on_mic = Instant::now();
                        self.active = Some(Active { rec, widget: Some(widget), on_mic: true, last_on_mic });
                    }
                    Err(e) => {
                        eprintln!("can't record: {e}");
                        widget.close();
                    }
                }
            }
            "dismiss" if offered => {
                let (widget, app) = self.offer.take().expect("offered");
                self.dismissed.insert(app);
                widget.close();
            }
            "stop" if self.active.as_ref().and_then(|a| a.widget.as_ref()).is_some_and(|w| w.id == id) => {
                self.stop("stop button")
            }
            _ => {}
        }
        self.show();
    }

    fn on_widget_gone(&mut self, id: u64) {
        if self.offer.as_ref().is_some_and(|(w, _)| w.id == id) {
            // the card died without an answer: don't keep re-offering this call
            let (_, app) = self.offer.take().expect("offered");
            self.dismissed.insert(app);
        }
        if let Some(a) = &mut self.active
            && a.widget.as_ref().is_some_and(|w| w.id == id)
        {
            a.widget = None; // the recording goes on; the tray menu can still stop it
        }
    }

    fn stop(&mut self, why: &str) {
        let Some(a) = self.active.take() else { return };
        // stopped by hand while the app still holds the mic: don't offer this same call again
        self.dismissed.insert(a.rec.app);
        if let Some(w) = a.widget {
            w.close();
        }
        match a.rec.stop() {
            Ok(folder) => {
                eprintln!("stopped ({why}): {}", folder.display());
                transcribe(Some(&folder));
            }
            Err(e) => eprintln!("stopping the recording failed: {e}"),
        }
        self.show();
    }
}

/// Transcribes a finished call (or, with None, every meeting folder still without a transcript) in the
/// background, at the lowest priority. Needs CALLSCRIBE_TRANSCRIBER, the path of transcriber/transcribe.py.
/// It runs as its own transient systemd unit, so quitting or restarting the daemon doesn't kill it.
fn transcribe(folder: Option<&std::path::Path>) {
    let Some(script) = std::env::var_os("CALLSCRIBE_TRANSCRIBER") else {
        return eprintln!("CALLSCRIBE_TRANSCRIBER is not set: not transcribing");
    };
    let mut cmd = Command::new("systemd-run");
    cmd.args(["--user", "--collect", "--quiet", "-p", "Nice=19"]);
    if std::env::var_os("CALLSCRIBE_MEETINGS").is_some() {
        cmd.arg("--setenv=CALLSCRIBE_MEETINGS");
    }
    cmd.args(["uv", "run"]).arg(script);
    match folder {
        Some(f) => cmd.arg(f),
        None => cmd.arg("--pending"),
    };
    match cmd.stdin(Stdio::null()).spawn() {
        // reaped by a thread, so no zombie is left behind
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => eprintln!("can't start the transcriber: {e}"),
    }
}

fn run() -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let (actions, action_rx) = mpsc::channel();
    let tray = tray::spawn(actions);
    let forward = tx.clone();
    std::thread::spawn(move || {
        for a in action_rx {
            if forward.send(Event::Tray(a)).is_err() {
                break;
            }
        }
    });
    watch_mic(tx.clone())?;
    transcribe(None); // anything a crash or reboot left untranscribed
    let mut d = Daemon {
        tx,
        tray,
        next_id: 0,
        mic_apps: Vec::new(),
        offer: None,
        dismissed: HashSet::new(),
        active: None,
        captures: Vec::new(),
    };
    loop {
        // while recording, wake every second to notice the call app has left the mic
        let wait = if d.active.is_some() {
            Some(Duration::from_secs(1))
        } else {
            d.browser_waiting().then_some(Duration::from_secs(2))
        };
        let event = match wait {
            Some(wait) => match rx.recv_timeout(wait) {
                Ok(e) => Some(e),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(e) => Some(e),
                Err(_) => break,
            },
        };
        match event {
            Some(Event::Mic(captures)) => d.on_mic(&captures),
            Some(Event::Widget(id, line)) => d.on_widget(id, &line),
            Some(Event::WidgetGone(id)) => d.on_widget_gone(id),
            Some(Event::Tray(tray::Action::StopRecording)) => d.stop("tray"),
            Some(Event::Tray(tray::Action::Quit)) => {
                d.stop("quit");
                d.close_offer();
                return Ok(());
            }
            Some(Event::MicLost(why)) => {
                d.stop("audio server restarted");
                d.close_offer();
                // an error exit: systemd restarts us against the new audio server
                return Err(io::Error::other(why));
            }
            None if d.browser_waiting() => d.on_mic(&d.captures.clone()),
            None => {}
        }
        if d.active.as_mut().is_some_and(|a| a.call_gone(Instant::now())) {
            d.stop("call app left the microphone");
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let result = match std::env::args().nth(1).as_deref() {
        Some("run") => run(),
        _ => {
            eprintln!("usage: callscribe run");
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
