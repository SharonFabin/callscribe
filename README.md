# callscribe

Record your calls on Linux and transcribe them locally, in Hebrew and English, on an Intel GPU.

No meeting bot and no cloud. The goal: callscribe notices when a call app opens your microphone, records your side and
the call side as two separate tracks, and when the call ends transcribes both with
[ivrit.ai](https://huggingface.co/ivrit-ai)'s Hebrew Whisper or stock Whisper, through OpenVINO.

**Status: early.** Working today: the transcriber (including importing meetings recorded by
Yogurt) and the microphone event logger. Also the floating recording widget (not yet driven by
the recorder). In progress: the recorder and the meetings app.

## How it works (the recorder parts are being built)

- **Call detection** watches PipeWire's list of apps capturing audio (the same metadata the mic privacy
  indicator uses; no audio is read while waiting). A call starts when a call app opens the microphone and ends
  when it lets go of it, or when the calendar event is over and nobody is talking. Idea taken from Yogurt.
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
uv run transcriber/transcribe.py --yogurt        # import the last 7 days of Yogurt meetings into ~/Meetings
```

## Mic event logger

```sh
cargo build --release
install -Dm755 target/release/callscribe ~/.local/bin/callscribe
mkdir -p ~/.config/systemd/user
ln -s "$PWD/systemd/callscribe-log-mic.service" ~/.config/systemd/user/
systemctl --user enable --now callscribe-log-mic.service
```

It shows a tray icon (a waveform) whose tooltip says which apps are using the microphone. It appends every change in who captures audio (app, device, paused, muted) to
`~/.local/state/callscribe/mic-events.jsonl`. It uses about 3 MB of RAM and no CPU while idle.

## Recording widget

A small pill floating at the top of the screen while a call records: elapsed time, the meeting's title (Hebrew
titles render right to left), and a stop button. Built with [Rune](https://github.com/anomalyco/rune); on
Wayland it is a layer-shell overlay (XWayland where there is no layer-shell) that never takes keyboard focus. Hebrew
titles need the Noto Sans Hebrew font installed. It redraws once a second and sleeps in
between.

```sh
cargo run --release -p callscribe-widget -- --title "Weekly sync"   # prints "stop" when stopped
cargo run -p callscribe-widget -- --scene hebrew --snap pill.png     # headless render (debug builds)
```

## License

MIT
