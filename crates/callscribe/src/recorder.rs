//! Records a call as two tracks, as Yogurt does: me.ogg (the microphone) and them.ogg (what the
//! speakers play), each its own ffmpeg reading PipeWire's PulseAudio interface, Opus at 32 kbit/s
//! mono (about 14 MB an hour per track).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Recording {
    pub folder: PathBuf,
    pub title: String,
    pub app: &'static str,
    /// unix ms
    pub started: u128,
    tracks: Vec<Child>,
}

fn default_device(what: &str) -> io::Result<String> {
    let out = Command::new("pactl").arg(format!("get-default-{what}")).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn ffmpeg(input: &str, out: &Path) -> io::Result<Child> {
    Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-f", "pulse", "-name", "callscribe", "-i", input])
        .args(["-ac", "1", "-c:a", "libopus", "-b:a", "32k", "-application", "voip", "-y"])
        .arg(out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
}

/// `<date> <title> [<key>]`, safe as one path component: no separators or control characters, the title
/// cut to 150 bytes. Same shape as the transcriber's Yogurt imports.
pub fn folder_name(local_time: &str, title: &str, key: &str) -> String {
    let safe: String = title.chars().filter(|c| !c.is_control() && !"/\\:*?\"<>|[]".contains(*c)).collect();
    let mut cut = safe.trim().to_owned();
    while cut.len() > 150 {
        cut.pop();
    }
    format!("{local_time} {} [{key}]", cut.trim()).replace("  ", " ")
}

impl Recording {
    /// Starts both tracks in a new folder under `meetings`.
    pub fn start(meetings: &Path, title: &str, app: &'static str) -> io::Result<Recording> {
        let started = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let local = Command::new("date").arg("+%Y-%m-%d %H%M").output()?;
        let local = String::from_utf8_lossy(&local.stdout).trim().to_owned();
        let folder = meetings.join(folder_name(&local, title, &format!("{:08x}", started as u32)));
        fs::create_dir_all(&folder)?;
        // ponytail: the devices are fixed at start (PipeWire pins even @DEFAULT_MONITOR@ to a node);
        // switching headsets mid-call leaves a silent track. Move the streams on default changes if that bites.
        let mic = default_device("source")?;
        let speakers = format!("{}.monitor", default_device("sink")?);
        let mut rec = Recording { folder, title: title.to_owned(), app, started, tracks: Vec::new() };
        let started_ok = (|| {
            rec.tracks.push(ffmpeg(&mic, &rec.folder.join("me.ogg"))?);
            rec.tracks.push(ffmpeg(&speakers, &rec.folder.join("them.ogg"))?);
            rec.write_meta(None)?;
            // a missing device or codec makes ffmpeg exit at once: say so rather than "record" nothing
            std::thread::sleep(Duration::from_millis(300));
            for t in &mut rec.tracks {
                if let Some(status) = t.try_wait()? {
                    return Err(io::Error::other(format!("ffmpeg exited at start: {status}")));
                }
            }
            Ok(())
        })();
        if let Err(e) = started_ok {
            for t in &mut rec.tracks {
                let _ = t.kill(); // a dropped Child keeps running
                let _ = t.wait();
            }
            return Err(e);
        }
        Ok(rec)
    }

    fn write_meta(&self, ended: Option<u128>) -> io::Result<()> {
        let meta = serde_json::json!({"title": self.title, "started": self.started, "ended": ended, "app": self.app});
        // write-then-rename: a crash never leaves half a meta.json
        let tmp = self.folder.join("meta.json.tmp");
        fs::write(&tmp, meta.to_string())?;
        fs::rename(tmp, self.folder.join("meta.json"))
    }

    /// Stops both tracks so their files are complete (SIGINT makes ffmpeg finish the file), and
    /// records when the call ended.
    pub fn stop(mut self) -> io::Result<PathBuf> {
        for t in &self.tracks {
            // ponytail: shells out to kill(1) rather than adding a signals crate for one SIGINT
            let _ = Command::new("kill").args(["-INT", &t.id().to_string()]).status();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        for t in &mut self.tracks {
            while t.try_wait()?.is_none() {
                if Instant::now() > deadline {
                    let _ = t.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        self.write_meta(Some(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis())))?;
        Ok(self.folder)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn folder_names_are_one_safe_component() {
        let n = super::folder_name("2026-11-23 1530", "a/b: [c]?", "0badf00d");
        assert_eq!(n, "2026-11-23 1530 ab c [0badf00d]");
        let long = super::folder_name("d", &"ש".repeat(200), "k");
        assert!(long.len() <= 150 + 6 && long.ends_with(" [k]"));
        assert_eq!(super::folder_name("d", "", "k"), "d [k]");
    }
}
