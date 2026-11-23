//! The recording pill: a small capsule floating at the top of the screen while a call records.
//! Red dot, elapsed time, the meeting's title, and a stop button.
//!
//! `callscribe-widget [--title TITLE] [--since UNIX_MS]` shows it (on Wayland, a layer-shell overlay).
//! Stop prints `stop` on stdout and exits: the recorder that launched it reads that.
//! With `--offer` it starts as Yogurt's "take notes?" card instead: Record prints `record` and turns it
//! into the recording pill, the close button prints `dismiss` and exits.
//! `callscribe-widget --scene hebrew --snap pill.png` renders it headless.

#![forbid(unsafe_code)]

use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rune::tok::TextStyle;
use rune::{
    App, AppSpec, Availability, Color, Cx, El, OverlayInput, OverlayOptions, OverlaySpec, RunOptions, ScreenEdge, Tx, WindowSpec,
    el, oklch, oklcha, row, text,
};

const SIZE: (f32, f32) = (300.0, 44.0);
const TIMER: TextStyle = TextStyle::new(14.0, 600.0).features("'tnum' 1");
const TITLE: TextStyle = TextStyle::new(13.0, 500.0);

struct State {
    title: String,
    /// asking whether to record, not recording yet
    offering: bool,
    /// recording start, unix seconds
    since: f64,
    /// elapsed seconds shown; changes once a second
    shown: u64,
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn clock(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

fn init() -> State {
    let mut args = std::env::args().skip(1);
    let (mut title, mut since, mut offering) = (String::from("Recording"), now(), false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--offer" => offering = true,
            "--title" => title = args.next().unwrap_or(title),
            "--since" => since = args.next().and_then(|v| v.parse::<f64>().ok()).map_or(since, |ms| ms / 1000.0),
            _ => {}
        }
    }
    State { title, offering, since, shown: 0 }
}

/// Tells the daemon; if it is gone, carry on (println! would panic on a closed pipe).
fn say(word: &str) {
    let _ = writeln!(std::io::stdout(), "{word}");
}

fn view(s: &State, _: &mut Cx<State>) -> El {
    // rune's Color is linear light; OKLCH says what it looks like
    let (bg, red, dim) = (oklcha(0.2, 0.006, 265.0, 0.94), oklch(0.65, 0.22, 25.0), Color::WHITE.alpha(0.6));
    let pill = row().key("pill").hit().w_full().h_full().px(14.0).gap(10.0).radius(SIZE.1 / 2.0).fill(bg).items_center();
    if s.offering {
        let record = row().key("record").hit().role("button").label("Record this call").h(28.0).px(12.0).gap(6.0)
            .radius(14.0).fill(red).items_center()
            .child(el().size(8.0, 8.0).radius(4.0).fill(Color::WHITE))
            .child(text("Record").font(TITLE).color(Color::WHITE))
            .on_press(|s: &mut State| {
                say("record");
                (s.offering, s.since, s.shown) = (false, now(), 0);
            });
        let close = el().key("dismiss").hit().role("button").label("Don't record").size(28.0, 28.0).radius(14.0)
            .fill(Color::WHITE.alpha(0.1)).center()
            .child(text("\u{d7}").font(TIMER).color(dim))
            .on_press(|_: &mut State| {
                say("dismiss");
                std::process::exit(0);
            });
        return pill
            .child(text(&s.title).key("title").font(TITLE).color(Color::WHITE).single_line().truncate().grow(1.0))
            .child(record)
            .child(close);
    }
    let stop = el().key("stop").hit().role("button").label("Stop recording").size(28.0, 28.0).radius(14.0)
        .fill(Color::WHITE.alpha(0.1)).center()
        .child(el().size(10.0, 10.0).radius(2.0).fill(red))
        .on_press(|_: &mut State| {
            say("stop");
            std::process::exit(0);
        });
    pill.child(el().key("dot").size(10.0, 10.0).radius(5.0).fill(red))
        .child(text(clock(s.shown)).key("timer").font(TIMER).color(Color::WHITE))
        .child(text(&s.title).key("title").font(TITLE).color(dim).single_line().truncate().grow(1.0))
        .child(stop)
}

/// Redraw only when the shown second changes, then sleep until the next one: idle between ticks.
fn poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    if s.offering {
        return false; // no clock on the card; Record wakes it
    }
    let elapsed = (now() - s.since).max(0.0);
    let changed = elapsed as u64 != s.shown;
    s.shown = elapsed as u64;
    tx.wake_after(Duration::from_secs_f64(1.0 - elapsed.fract()));
    changed
}

fn spec() -> AppSpec<State> {
    AppSpec {
        poll: Some(poll),
        scenes: &[
            ("offer", |s| (s.title, s.offering) = ("Brave call".into(), true)),
            ("english", |s| (s.title, s.since, s.shown) = ("Weekly sync with the design team".into(), now() - 754.0, 754)),
            ("hebrew", |s| (s.title, s.since, s.shown) = ("פגישת היכרות עם הצוות".into(), now() - 3725.0, 3725)),
        ],
        // a named Hebrew family: through plain fallback Rune lays Hebrew out with wrong advances
        families: ("Inter, Noto Sans Hebrew, sans-serif", "ui-monospace"),
        ..AppSpec::new("callscribe", SIZE, init, view)
    }
}

fn main() {
    rune::run_windows_with(RunOptions { accessory: true }, |_| {
        let mut app = App::new(spec()).windowless();
        let pill = app.overlay(
            OverlaySpec { edge: ScreenEdge::Top, margin: 12.0, ..OverlaySpec::new("callscribe", SIZE, view) }
                // without layer-shell (GNOME), show through XWayland rather than not at all
                .options(OverlayOptions { input: OverlayInput::Regions, xwayland_input: true, ..Default::default() }),
            init(),
        );
        pill.show();
        // headless renders (Rune's agent tooling) make no overlay: nothing to check there
        let headless = std::env::args().any(|a| {
            ["--snap", "--tree", "--trace", "--frames", "--script", "--video", "--strip", "--html"].contains(&a.as_str())
        });
        if headless {
            return vec![WindowSpec::from_app("pill", app)];
        }
        // the app is windowless and only Stop ends it: if the pill can't be shown, fail instead of
        // running invisibly with the recorder waiting for a "stop" that can never come
        let check = pill.clone();
        std::thread::spawn(move || {
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(100));
                match check.availability() {
                    Availability::Available => return,
                    Availability::Unavailable => {
                        eprintln!("callscribe-widget: can't show the overlay: {}", check.failure().unwrap_or_default());
                        std::process::exit(1);
                    }
                    Availability::Unknown => {}
                }
            }
            eprintln!("callscribe-widget: can't show the overlay: timed out");
            std::process::exit(1);
        });
        vec![WindowSpec::from_app("pill", app)]
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn clock_format() {
        assert_eq!(super::clock(0), "00:00");
        assert_eq!(super::clock(754), "12:34");
        assert_eq!(super::clock(3725), "1:02:05");
    }
}
