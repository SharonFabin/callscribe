//! Which apps have an audio input open right now, read from PipeWire's PulseAudio interface.
//!
//! This is metadata only (the same list the system's mic privacy indicator uses): no audio is read.

use std::collections::HashMap;
use std::io;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// One open capture stream: an app reading from a microphone (or from a speaker monitor).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Capture {
    pub id: u64,
    pub app: String,
    pub binary: Option<String>,
    pub pid: Option<u32>,
    /// the device it reads from, e.g. `alsa_input.pci-...` or `...monitor`
    pub source: String,
    /// true when it reads a speaker monitor (system audio), not a microphone
    pub monitor: bool,
    /// paused by the app (some apps cork the stream on mute instead of closing it)
    pub corked: bool,
    pub mute: bool,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Open,
    Close,
    Change,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Change {
    pub kind: Kind,
    #[serde(flatten)]
    pub capture: Capture,
}

#[derive(Deserialize)]
struct RawOutput {
    index: u64,
    source: u64,
    corked: bool,
    mute: bool,
    #[serde(default)]
    properties: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct RawSource {
    index: u64,
    name: String,
}

fn pactl_json(what: &str) -> io::Result<String> {
    let out = Command::new("pactl").args(["-f", "json", "list", what]).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "pactl list {what}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    String::from_utf8(out.stdout).map_err(io::Error::other)
}

/// The capture streams open right now.
pub fn snapshot() -> io::Result<Vec<Capture>> {
    parse(&pactl_json("source-outputs")?, &pactl_json("sources")?).map_err(io::Error::other)
}

/// Capture streams from `pactl -f json list source-outputs` and `... sources` output.
pub fn parse(outputs: &str, sources: &str) -> serde_json::Result<Vec<Capture>> {
    let sources: HashMap<u64, String> = serde_json::from_str::<Vec<RawSource>>(sources)?
        .into_iter()
        .map(|s| (s.index, s.name))
        .collect();
    let prop = |o: &RawOutput, k: &str| o.properties.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    Ok(serde_json::from_str::<Vec<RawOutput>>(outputs)?
        .iter()
        .map(|o| {
            let source = sources.get(&o.source).cloned().unwrap_or_else(|| o.source.to_string());
            Capture {
                id: o.index,
                app: prop(o, "application.name").unwrap_or_default(),
                binary: prop(o, "application.process.binary"),
                pid: prop(o, "application.process.id").and_then(|p| p.parse().ok()),
                monitor: source.ends_with(".monitor"),
                source,
                corked: o.corked,
                mute: o.mute,
            }
        })
        .collect())
}

/// What changed between two snapshots, in a stable order (closes, then changes and opens by id).
pub fn diff(before: &[Capture], after: &[Capture]) -> Vec<Change> {
    let mut changes: Vec<Change> = before
        .iter()
        .filter(|b| !after.iter().any(|a| a.id == b.id))
        .map(|b| Change { kind: Kind::Close, capture: b.clone() })
        .collect();
    for a in after {
        match before.iter().find(|b| b.id == a.id) {
            None => changes.push(Change { kind: Kind::Open, capture: a.clone() }),
            Some(b) if b != a => changes.push(Change { kind: Kind::Change, capture: a.clone() }),
            Some(_) => {}
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCES: &str = r#"[{"index": 7, "name": "alsa_input.pci-0000_00_1f.3.analog-stereo"},
                               {"index": 8, "name": "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor"}]"#;

    fn outputs(corked: bool) -> String {
        format!(
            r#"[{{"index": 42, "source": 7, "corked": {corked}, "mute": false,
                  "properties": {{"application.name": "Brave", "application.process.binary": "brave",
                                  "application.process.id": "1234", "node.rate": "1/48000"}}}},
                {{"index": 43, "source": 8, "corked": false, "mute": false, "properties": {{}}}}]"#
        )
    }

    #[test]
    fn parses_mic_and_monitor_streams() {
        let c = parse(&outputs(false), SOURCES).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].app.as_str(), c[0].binary.as_deref(), c[0].pid), ("Brave", Some("brave"), Some(1234)));
        assert!(!c[0].monitor && c[1].monitor);
        assert_eq!(c[1].app, "");
    }

    #[test]
    fn unknown_source_keeps_its_index() {
        let c = parse(r#"[{"index": 1, "source": 99, "corked": false, "mute": false}]"#, "[]").unwrap();
        assert_eq!(c[0].source, "99");
    }

    #[test]
    fn diff_reports_open_change_close() {
        let idle = parse("[]", SOURCES).unwrap();
        let live = parse(&outputs(false), SOURCES).unwrap();
        let corked = parse(&outputs(true), SOURCES).unwrap();
        let kinds = |d: Vec<Change>| d.into_iter().map(|c| (c.kind, c.capture.id)).collect::<Vec<_>>();
        assert_eq!(kinds(diff(&idle, &live)), [(Kind::Open, 42), (Kind::Open, 43)]);
        assert_eq!(kinds(diff(&live, &corked)), [(Kind::Change, 42)]);
        assert_eq!(kinds(diff(&live, &live)), []);
        assert_eq!(kinds(diff(&corked, &idle)), [(Kind::Close, 42), (Kind::Close, 43)]);
    }
}
