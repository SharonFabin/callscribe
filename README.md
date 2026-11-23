# callscribe

Record your calls on Linux and transcribe them locally, in Hebrew and English, on an Intel GPU.

No meeting bot and no cloud. callscribe notices when a call app opens your microphone and offers to record;
it records your side and the call side as two separate tracks, and when the call ends transcribes both with
[ivrit.ai](https://huggingface.co/ivrit-ai)'s Hebrew Whisper or stock Whisper, through OpenVINO.

**Status: early, in daily use.** Working: call detection, recording, the floating card and pill, the tray icon,
transcription (including importing meetings recorded by Yogurt). Next: calendar titles and attendees, screenshots
of the call window, a meetings app.

## How it works

The call rules are Yogurt's, read from its source:

- **Call detection** watches PipeWire's list of apps capturing audio (the same metadata the mic privacy
  indicator uses; no audio is read while waiting). When a call app (Zoom, Teams, Slack, Discord, Webex, or a
  browser whose window title has "Meet", "Teams" or "Zoom") opens the microphone, a card at the top of the
  screen offers to record. Nothing records without a click; closing the card skips that call.
- **The call ends** when the call app has been off the microphone for 4 seconds; the recording stops by itself
  and is transcribed at the lowest priority. The pill's stop button and the tray menu stop it by hand.
- **Two tracks, no diarization.** Your microphone and the call audio are recorded separately, so every line is
  correctly "Me" or "Them".
- **Echo removal.** Without headphones the mic hears the call through the speakers. The echo arrives about 60 ms
  after the call audio and is about a tenth as loud, so mic frames that are much quieter than the call audio of
  the last 200 ms are dropped before transcription.
- **No silence for Whisper to fill.** Only speech is transcribed (silence makes Whisper invent words like
  "thank you"), then timestamps are mapped back to the call's timeline.
- **Language per track.** Whisper large-v3-turbo votes over up to twelve 30 s windows of each track. Mostly
  English goes to stock Whisper; anything else goes to ivrit.ai's model, which keeps English terms in English.
  A track too short to vote follows the other side.
- **Loop guards.** A temperature fallback when Whisper repeats itself, and a cap on repeated characters.

## Transcriber

Needs [uv](https://docs.astral.sh/uv/), ffmpeg and an OpenVINO-capable device (an Intel GPU, or `--device CPU`).

```sh
transcriber/setup_models.sh                      # once: ~2.5 GB of OpenVINO int8 models
uv run transcriber/transcribe.py <meeting dir>   # me.ogg + them.ogg (+ meta.json) -> transcript.md
uv run transcriber/transcribe.py --pending       # every folder in ~/Meetings with audio but no transcript
uv run transcriber/transcribe.py --yogurt        # import the last 7 days of Yogurt meetings into ~/Meetings
```

## The daemon

Needs PipeWire (with its PulseAudio interface, for `pactl`), ffmpeg, and a tray (StatusNotifierItem) in your bar.
Window titles come from Hyprland's `hyprctl`; on other desktops any browser using the microphone counts as a call.

```sh
cargo build --release
install -Dm755 target/release/callscribe target/release/callscribe-widget -t ~/.local/bin
mkdir -p ~/.config/systemd/user
ln -s "$PWD/systemd/callscribe.service" ~/.config/systemd/user/   # edit CALLSCRIBE_TRANSCRIBER first
systemctl --user enable --now callscribe.service
```

It starts with the graphical session (`graphical-session.target`, which uwsm and other systemd-managed sessions
reach). Calls go to `~/Meetings/<date> <title> [<id>]/` (me.ogg, them.ogg, meta.json, transcript.md); set
`CALLSCRIBE_MEETINGS` to change that. The tray icon (a waveform, with a red dot while recording) says which apps
use the microphone. Every change in who captures audio is also logged to
`~/.local/state/callscribe/mic-events.jsonl`. It uses about 7 MB of RAM and no CPU between calls.

## Recording widget

The card that offers to record, which becomes a pill while the call records: elapsed time, the meeting's title
(Hebrew titles render right to left), and a stop button. Built with [Rune](https://github.com/anomalyco/rune); on
Wayland it is a layer-shell overlay (XWayland where there is no layer-shell) that never takes keyboard focus. Hebrew
titles need the Noto Sans Hebrew font installed. It redraws once a second and sleeps in
between.

```sh
cargo run --release -p callscribe-widget -- --title "Weekly sync"   # prints "stop" when stopped
cargo run -p callscribe-widget -- --scene hebrew --snap pill.png     # headless render (debug builds)
```

## License

MIT
