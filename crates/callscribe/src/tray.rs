//! The tray icon (StatusNotifierItem over D-Bus): shows that callscribe is running, and which apps
//! hold the microphone right now.

use std::sync::mpsc;

use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, ToolTip, Tray};

pub struct CallTray {
    /// apps capturing audio from a microphone right now
    pub mic_apps: Vec<String>,
}

impl Tray for CallTray {
    fn id(&self) -> String {
        "callscribe".into()
    }

    fn title(&self) -> String {
        "callscribe".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        [22, 44, 66].into_iter().map(waveform).collect()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip { title: "callscribe".into(), description: self.status(), ..Default::default() }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem { label: self.status(), enabled: false, ..Default::default() }.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

impl CallTray {
    fn status(&self) -> String {
        if self.mic_apps.is_empty() {
            "Watching for calls".into()
        } else {
            format!("Microphone in use: {}", self.mic_apps.join(", "))
        }
    }
}

/// Starts the tray icon on its own thread and returns where to send the apps holding the mic.
/// The icon shows up whenever a tray host (the bar) is running, including one that starts or
/// restarts later. Updates never block the caller: a stuck tray host can't stall the logger.
pub fn spawn() -> mpsc::Sender<Vec<String>> {
    let (tx, rx) = mpsc::channel::<Vec<String>>();
    std::thread::spawn(move || {
        // at login this runs before the bar exists: wait for its watcher instead of giving up
        let tray = match (CallTray { mic_apps: Vec::new() }).assume_sni_available(true).spawn() {
            Ok(tray) => tray,
            Err(e) => return eprintln!("no tray icon: {e}"),
        };
        while let Ok(mut apps) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                apps = newer;
            }
            tray.update(|t| t.mic_apps = apps);
        }
    });
    tx
}

/// Five vertical rounded bars, tallest in the middle, light grey on transparent: a size x size
/// ARGB32 (big-endian) icon. Drawn here so the repo needs no image files.
pub fn waveform(size: i32) -> Icon {
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
            let alpha = (0.5 - d * unit).clamp(0.0, 1.0);
            data.extend_from_slice(&[(alpha * 255.0) as u8, 0xe8, 0xe8, 0xec]);
        }
    }
    Icon { width: size, height: size, data }
}

#[cfg(test)]
mod tests {
    #[test]
    fn waveform_draws_bars_and_gaps() {
        let icon = super::waveform(24);
        let alpha = |x: usize, y: usize| icon.data[(y * 24 + x) * 4];
        assert_eq!(icon.data.len(), 24 * 24 * 4);
        assert!(alpha(12, 12) > 200, "middle of the tallest bar");
        assert!(alpha(12, 2) > 200, "the tallest bar reaches near the top");
        assert!(alpha(10, 12) < 64, "gap between bars");
        assert_eq!(alpha(4, 3), 0, "short outer bar");
    }
}
