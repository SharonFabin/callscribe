//! The tray icon (StatusNotifierItem over D-Bus): shows that callscribe is running, and which apps
//! hold the microphone right now.

use std::sync::mpsc;

use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, ToolTip, Tray};

/// What the tray's menu asks the daemon to do.
pub enum Action {
    StopRecording,
    Quit,
}

/// What the tray shows.
#[derive(Clone, Default)]
pub struct Status {
    /// apps capturing audio from a microphone right now
    pub mic_apps: Vec<String>,
    /// the meeting being recorded
    pub recording: Option<String>,
}

pub struct CallTray {
    status: Status,
    actions: mpsc::Sender<Action>,
}

impl Tray for CallTray {
    fn id(&self) -> String {
        "callscribe".into()
    }

    fn title(&self) -> String {
        "callscribe".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let dot = self.status.recording.is_some();
        [22, 44, 66].into_iter().map(|s| waveform(s, dot)).collect()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip { title: "callscribe".into(), description: self.line(), ..Default::default() }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> =
            vec![StandardItem { label: self.line(), enabled: false, ..Default::default() }.into(), MenuItem::Separator];
        if self.status.recording.is_some() {
            items.push(
                StandardItem {
                    label: "Stop recording".into(),
                    icon_name: "media-playback-stop".into(),
                    activate: Box::new(|t: &mut Self| drop(t.actions.send(Action::StopRecording))),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                // through the daemon, so a recording in progress is finished and kept
                activate: Box::new(|t: &mut Self| drop(t.actions.send(Action::Quit))),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

impl CallTray {
    fn line(&self) -> String {
        match (&self.status.recording, self.status.mic_apps.is_empty()) {
            (Some(title), _) => format!("Recording: {title}"),
            (None, true) => "Watching for calls".into(),
            (None, false) => format!("Microphone in use: {}", self.status.mic_apps.join(", ")),
        }
    }
}

/// Starts the tray icon on its own thread: returns where to send what it shows, and its menu's
/// actions arrive on `actions`. The icon shows up whenever a tray host (the bar) runs, including one
/// that starts or restarts later. Updates never block the caller: a stuck tray host can't stall it.
pub fn spawn(actions: mpsc::Sender<Action>) -> mpsc::Sender<Status> {
    let (tx, rx) = mpsc::channel::<Status>();
    std::thread::spawn(move || {
        // at login this runs before the bar exists: wait for its watcher instead of giving up
        let tray = match (CallTray { status: Status::default(), actions }).assume_sni_available(true).spawn() {
            Ok(tray) => tray,
            Err(e) => return eprintln!("no tray icon: {e}"),
        };
        while let Ok(mut status) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                status = newer;
            }
            tray.update(|t| t.status = status);
        }
    });
    tx
}

/// Five vertical rounded bars, tallest in the middle, light grey on transparent: a size x size
/// ARGB32 (big-endian) icon, with a red dot at the bottom right while recording. Drawn here so the
/// repo needs no image files.
pub fn waveform(size: i32, recording: bool) -> Icon {
    // bar x positions and half-heights on a 24-unit grid, stroke 2.4 units wide with round caps
    const BARS: [(f32, f32); 5] = [(4.0, 2.0), (8.0, 6.0), (12.0, 9.0), (16.0, 5.0), (20.0, 2.0)];
    let unit = size as f32 / 24.0;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (x, y) = ((px as f32 + 0.5) / unit, (py as f32 + 0.5) / unit);
            // distance from the nearest bar (a capsule: its centre line widened by 1.2), in units;
            // one pixel of antialiasing across the edge
            let d = BARS
                .iter()
                .map(|&(bx, h)| (x - bx).hypot(((y - 12.0).abs() - h).max(0.0)) - 1.2)
                .fold(f32::MAX, f32::min);
            let mut alpha = (0.5 - d * unit).clamp(0.0, 1.0);
            if recording {
                // the dot, and a clear ring around it so it reads on top of the bars
                let r = (x - 19.0).hypot(y - 19.0);
                let dot = (0.5 - (r - 4.2) * unit).clamp(0.0, 1.0);
                let ring = (0.5 - (r - 5.8) * unit).clamp(0.0, 1.0);
                if dot > 0.0 {
                    data.extend_from_slice(&[(dot * 255.0) as u8, 0xef, 0x44, 0x44]);
                    continue;
                }
                alpha *= 1.0 - ring;
            }
            data.extend_from_slice(&[(alpha * 255.0) as u8, 0xe8, 0xe8, 0xec]);
        }
    }
    Icon { width: size, height: size, data }
}

#[cfg(test)]
mod tests {
    #[test]
    fn waveform_draws_bars_and_gaps() {
        let icon = super::waveform(24, false);
        let alpha = |x: usize, y: usize| icon.data[(y * 24 + x) * 4];
        assert_eq!(icon.data.len(), 24 * 24 * 4);
        assert!(alpha(12, 12) > 200, "middle of the tallest bar");
        assert!(alpha(12, 2) > 200, "the tallest bar reaches near the top");
        assert!(alpha(10, 12) < 64, "gap between bars");
        assert_eq!(alpha(4, 3), 0, "short outer bar");
    }

    #[test]
    fn recording_adds_a_red_dot() {
        let icon = super::waveform(24, true);
        let px = |x: usize, y: usize| &icon.data[(y * 24 + x) * 4..(y * 24 + x) * 4 + 4];
        assert_eq!(px(19, 19), [255, 0xef, 0x44, 0x44]);
        assert_eq!(px(12, 12)[1], 0xe8, "the bars stay grey");
    }
}
