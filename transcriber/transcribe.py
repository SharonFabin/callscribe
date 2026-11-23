# /// script
# requires-python = ">=3.11,<3.14"
# dependencies = ["openvino-genai==2026.4.*", "openvino==2026.4.*", "numpy"]
# ///
"""Transcribe recorded calls locally on an Intel GPU (OpenVINO): Hebrew with ivrit.ai's Whisper, English with
stock Whisper large-v3-turbo.

A meeting is a folder with me.ogg (your microphone) and them.ogg (the call audio), and optionally meta.json
({"title", "started" (unix ms)}). Each track is its own speaker, so there is no diarization. Per track:
  1. keep only its own speech: an energy gate, and for the mic, drop the other side's echo from the speakers
  2. vote on the language over 30s windows of that speech; mostly English goes to the stock model, anything
     else to ivrit (it keeps English terms in English, and told "Hebrew" it even writes plain English well)
  3. transcribe with a temperature fallback against repetition loops
Writes transcript.md into the folder, skipping folders that have one.

Usage: uv run transcribe.py <meeting dir>... [--force] [--device GPU]
       uv run transcribe.py --yogurt [id prefix ...] [--days 7]   (import Yogurt meetings, read-only)
Models: run setup_models.sh once.
"""
import argparse
import json
import os
import re
import sqlite3
import subprocess
import sys
import time
import traceback
from bisect import bisect_left, bisect_right
from datetime import datetime
from pathlib import Path

import numpy as np
from numpy.lib.stride_tricks import sliding_window_view

MODELS = Path(os.environ.get("CALLSCRIBE_MODELS", Path.home() / ".cache/callscribe/models"))
OV_CACHE = MODELS.parent / "ov-cache"  # compiled-kernel cache, makes later starts fast
HEBREW_MODEL = MODELS / "ivrit-large-v3-ov-int8"
STOCK_MODEL = MODELS / "whisper-large-v3-turbo-ov-int8"
MEETINGS = Path(os.environ.get("CALLSCRIBE_MEETINGS", Path.home() / "Meetings"))
YOGURT = Path.home() / ".config/Yogurt"

SR = 16000
FRAME = 320  # 20ms
ECHO_FRAMES = 10  # 200ms: the echo reaches the mic ~60ms after the call audio (measured) and rings on
WINDOW = 30 * SR
SETTLE = 120  # seconds an audio file must be unchanged before it counts as finished
ENGLISH_SHARE = 0.8  # of language-vote windows; below it the track is treated as Hebrew
VOTE_WINDOWS = 12


def decode(path: Path):
    """16kHz mono float32 via ffmpeg; None if undecodable. Raises if ffmpeg was killed (retry next run)."""
    r = subprocess.run(["ffmpeg", "-nostdin", "-loglevel", "error", "-i", str(path), "-f", "f32le",
                        "-ac", "1", "-ar", str(SR), "-"], capture_output=True)
    if r.returncode < 0:
        raise RuntimeError(f"ffmpeg killed by signal {-r.returncode}")
    if r.returncode:
        print(f"{path}: {r.stderr.decode(errors='replace').strip()}", file=sys.stderr)
        return None
    return np.frombuffer(r.stdout, np.float32)


def diversity(chunks) -> float:
    """Lowest share of distinct words in any 30s window with 20+ words. Repetition loops score ~0.1,
    real speech stays above 0.6. (Whisper's gzip-ratio test misfires on Hebrew, which compresses well.)"""
    windows = {}
    for start, text in chunks:
        windows[int(start // 30)] = windows.get(int(start // 30), "") + " " + text
    return min([len(set(w)) / len(w) for w in (t.split() for t in windows.values()) if len(w) >= 20] or [1.0])


def frame_rms(audio, n):
    """RMS per 20ms frame over n frames; a shorter track counts as silent at the end."""
    a = np.zeros(n * FRAME, np.float32)
    a[:min(len(audio), n * FRAME)] = audio[:n * FRAME]
    return np.sqrt((a ** 2).reshape(n, FRAME).mean(1))


def speech_spans(audio, other=None, floor=0.008, echo=0.6, gap=0.5, shortest=0.25, pad=0.15):
    """(start, end) seconds where this track has its own speech: louder than `floor` and, while the other
    track is active, at least `echo` times its energy (speaker echo in the mic measured ~0.1x, 90% under
    0.37x). Talking over each other is lost. ponytail: energy gate, not real echo cancellation."""
    n = len(audio) // FRAME
    r = frame_rms(audio, n)
    voiced = r > floor
    if other is not None:
        o = frame_rms(other, n)
        ref = sliding_window_view(np.concatenate([np.zeros(ECHO_FRAMES), o]), ECHO_FRAMES + 1).max(1)
        voiced &= ~((ref > 0.003) & (r < echo * ref))
    spans, f = [], FRAME / SR
    for i in np.flatnonzero(voiced):
        if spans and i * f - spans[-1][1] < gap:
            spans[-1][1] = (i + 1) * f
        else:
            spans.append([i * f, (i + 1) * f])
    return [(max(0.0, a - pad), b + pad) for a, b in spans if b - a >= shortest]


def squeeze(audio, spans, sep=0.3):
    """Only the spans, 0.3s of silence between (Whisper invents words in long silences), and a table back
    to original time: [(start in squeezed, start in original, end in original)]."""
    parts, table, t = [], [], 0.0
    gap = np.zeros(int(sep * SR), np.float32)
    for a, b in spans:
        clip = audio[int(a * SR):int(b * SR)]
        table.append((t, a, b))
        parts += [clip, gap]
        t += (len(clip) + len(gap)) / SR
    return (np.concatenate(parts) if parts else np.zeros(0, np.float32)), table


def unsqueeze(t, table, end=False):
    """Original time of squeezed time t. Times in a separator stay at their span's end; a chunk end exactly
    at the next span's start belongs to the span before (Whisper snaps ends to the next chunk's start)."""
    starts = [s for s, _, _ in table]
    i = max(0, (bisect_left if end else bisect_right)(starts, t) - 1)
    s, a, b = table[i]
    return min(a + t - s, b)


def vote_windows(audio, count=VOTE_WINDOWS):
    """Up to `count` full 30s windows spread over the audio (only full ones: short clips misdetect)."""
    n = (len(audio) - WINDOW) // WINDOW + 1 if len(audio) >= WINDOW else 0
    picks = np.linspace(0, n - 1, min(n, count)).round().astype(int) if n else []
    return [audio[i * WINDOW:(i + 1) * WINDOW] for i in picks]


MIN_VOTES = 2  # full windows a track needs to decide its own language


def pick_language(own, other=(), partial="") -> str:
    """'en' or 'he' for a track. `own`/`other`: languages heard in full 30s windows of this track and the
    other one; `partial`: what was heard in this track's whole audio when it is shorter than a window.
    A track with enough speech votes for itself: 'en' if at least ENGLISH_SHARE of it sounds English.
    A short one follows the other side, which usually spoke more. Both short: its partial guess. (ivrit
    translates short English into Hebrew, and the stock model misreads short Hebrew, so a short track
    alone is a guess either way.)"""
    for votes in (own, other):
        if len(votes) >= MIN_VOTES:
            return "en" if sum(v == "en" for v in votes) / len(votes) >= ENGLISH_SHARE else "he"
    return "en" if partial == "en" else "he"


class Models:
    """Loaded on first use (~1.5 GB each); the stock model also does language detection."""

    def __init__(self, device):
        self.device, self.loaded = device, {}

    def __getitem__(self, lang):
        if lang not in self.loaded:
            import openvino_genai as ov_genai
            path = STOCK_MODEL if lang == "en" else HEBREW_MODEL
            if not (path / "openvino_encoder_model.xml").exists():
                sys.exit(f"missing {path}: run setup_models.sh")
            self.loaded[lang] = ov_genai.WhisperPipeline(str(path), self.device, CACHE_DIR=str(OV_CACHE))
        return self.loaded[lang]

    def detect(self, window) -> str:
        return str(self["en"].generate(window, task="transcribe", max_new_tokens=4).language).strip("<|>")


def own_speech(audio, other=None, floor=0.008):
    """(squeezed audio, time table) of one track's own speech, or None when it has none."""
    if audio is None:
        return None
    squeezed, table = squeeze(audio, speech_spans(audio, other, floor))
    return (squeezed, table) if len(squeezed) >= SR // 2 else None


def transcribe(models, speech, lang):
    """[(start, end, text)] in original time for one track's squeezed speech."""
    audio, table = speech
    best = None
    for temp in (0, 0.2, 0.4, 0.6, 0.8, 1.0):  # greedy first; on a loop, sample and keep the least loopy
        sampling = {"do_sample": True, "temperature": temp, "rng_seed": 1} if temp else {}
        # ivrit is only ever told Hebrew: told English it breaks, while told Hebrew it writes English terms
        # (and clear English sentences) in English
        r = models[lang].generate(audio, language=f"<|{lang}|>", task="transcribe", return_timestamps=True,
                                  **sampling)
        score = diversity((c.start_ts, c.text) for c in r.chunks)
        if best is None or score > best[0]:
            best = (score, r)
        if score >= 0.3:
            break
    # a loop inside one word ("אהההה..." x500) passes the word-level guard: cap character runs at 3
    texts = [(c, re.sub(r"(.)\1{3,}", r"\1\1\1", c.text).strip()) for c in best[1].chunks]
    return [(unsqueeze(c.start_ts, table), unsqueeze(c.end_ts, table, end=True), t) for c, t in texts if t]


def stamp(seconds):
    m, s = divmod(round(seconds * 10), 600)
    return f"{m:02d}:{s / 10:04.1f}"


def merge(me, them, gap=3.0):
    """Chunks of both tracks in time order; a speaker's consecutive chunks join into one line."""
    lines = []
    for a, b, who, t in sorted([(a, b, "Me", t) for a, b, t in me] + [(a, b, "Them", t) for a, b, t in them]):
        if lines and lines[-1][1] == who and a - lines[-1][2] < gap:
            lines[-1][2] = b
            lines[-1][3] += " " + t
        else:
            lines.append([a, who, b, t])
    return "\n".join(f"[{stamp(a)}] **{who}:** {t}" for a, who, b, t in lines)


def transcribe_meeting(folder: Path, models, audio_dir: Path | None = None):
    """Write folder/transcript.md from audio_dir (default: the folder itself)."""
    t0 = time.time()
    audio_dir = audio_dir or folder
    me, them = (decode(audio_dir / f"{t}.ogg") if (audio_dir / f"{t}.ogg").exists() else None
                for t in ("me", "them"))
    if me is None and them is None:  # nothing written: retried next run
        return print(f"{folder.name}: no audio", file=sys.stderr)
    if any(time.time() - f.stat().st_mtime < SETTLE for f in audio_dir.glob("*.ogg")):
        return print(f"{folder.name}: audio still being written, next run", file=sys.stderr)
    meta = json.loads((folder / "meta.json").read_text()) if (folder / "meta.json").exists() else {}
    # thresholds measured on real calls: mic room noise ~0.004 RMS, speech > 0.01; the call track is
    # digital (silence is exactly 0), so 0.003 there
    speech = {"me": own_speech(me, other=them), "them": own_speech(them, floor=0.003)}
    votes = {k: [models.detect(w) for w in vote_windows(v[0])] if v else [] for k, v in speech.items()}
    langs, lines = {}, {}
    for k, other in (("me", "them"), ("them", "me")):
        if speech[k] is None:
            langs[k], lines[k] = "", []
            continue
        short = len(votes[k]) < MIN_VOTES and len(votes[other]) < MIN_VOTES
        langs[k] = pick_language(votes[k], votes[other], models.detect(speech[k][0]) if short else "")
        lines[k] = transcribe(models, speech[k], langs[k])
    me_lang, me_lines, them_lang, them_lines = langs["me"], lines["me"], langs["them"], lines["them"]
    title = meta.get("title") or folder.name
    when = f"{datetime.fromtimestamp(meta['started'] / 1000):%Y-%m-%d %H:%M}\n" if meta.get("started") else ""
    head = f"# {title}\n\n{when}languages: me {me_lang or '-'}, them {them_lang or '-'}\n\n"
    tmp = folder / "transcript.md.tmp"  # write-then-rename: a killed run never leaves half a transcript
    tmp.write_text(head + merge(me_lines, them_lines) + "\n", encoding="utf-8")
    tmp.replace(folder / "transcript.md")
    print(f"{folder.name}: me {me_lang or '-'}, them {them_lang or '-'} in {time.time() - t0:.0f}s",
          file=sys.stderr, flush=True)


def folder_name(started_ms, title, key, limit=150):
    """'<date> <title> [<key>]': no path/glob/control characters, at most `limit` bytes of title."""
    safe = "".join(c for c in title if c.isprintable() and c not in '/\\:*?"<>|[]').strip()
    safe = safe.encode()[:limit].decode(errors="ignore").strip()
    return f"{datetime.fromtimestamp(started_ms / 1000):%Y-%m-%d %H%M} {safe} [{key}]".replace("  ", " ")


def yogurt_meetings(ids, days):
    """(folder, audio dir) for finished Yogurt meetings, creating each folder's meta.json. Read-only on
    Yogurt's database (it may be writing: WAL makes that safe)."""
    db = sqlite3.connect(f"file:{YOGURT / 'yogurt.db'}?mode=ro", uri=True)
    rows = db.execute("select id, title, started_at from meetings where ended_at is not null "
                      "order by started_at desc").fetchall()
    rows = [r for r in rows if any(r[0].startswith(i) for i in ids)] if ids else \
        [r for r in rows if r[2] / 1000 > time.time() - days * 86400]
    for mid, title, started in rows:
        audio = YOGURT / "meetings" / mid
        if not any(audio.glob("*.ogg")):  # no folder for a meeting without audio
            print(f"yogurt {mid[:8]}: no audio", file=sys.stderr)
            continue
        # by id, not name: a renamed meeting keeps its folder (and transcript)
        folder = next(MEETINGS.glob(f"*[[]{mid[:8]}]"), None) or MEETINGS / folder_name(started, title or "", mid[:8])
        folder.mkdir(parents=True, exist_ok=True)
        if not (folder / "meta.json").exists():
            (folder / "meta.json").write_text(json.dumps({"title": title, "started": started, "yogurt": mid}))
        yield folder, audio


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("folders", nargs="*", type=Path)
    p.add_argument("--yogurt", nargs="*", metavar="ID", help="import Yogurt meetings (all recent, or these ids)")
    p.add_argument("--days", type=float, default=7, help="with --yogurt and no ids: meetings of the last N days")
    p.add_argument("--force", action="store_true", help="redo meetings that already have transcript.md")
    p.add_argument("--device", default="GPU", help="OpenVINO device (GPU, CPU)")
    args = p.parse_args()

    todo = [(f, None) for f in args.folders] + (list(yogurt_meetings(args.yogurt, args.days))
                                               if args.yogurt is not None else [])
    todo = [(f, a) for f, a in todo if args.force or not (f / "transcript.md").exists()]
    if not todo:
        return print("nothing to transcribe", file=sys.stderr)
    models, failed = Models(args.device), 0
    for folder, audio_dir in todo:
        try:
            transcribe_meeting(folder, models, audio_dir)
        except Exception:  # one bad meeting must not block the others
            failed += 1
            print(f"FAILED {folder}", file=sys.stderr)
            traceback.print_exc()
    sys.exit(1 if failed else 0)


def selftest():
    me = [(1.0, 1.5, "a"), (1.6, 2.0, "b"), (9.0, 9.5, "d"), (20.0, 21.0, "f")]
    them = [(2.5, 3.0, "c"), (9.6, 10.0, "e")]
    assert merge(me, them) == ("[00:01.0] **Me:** a b\n[00:02.5] **Them:** c\n[00:09.0] **Me:** d\n"
                               "[00:09.6] **Them:** e\n[00:20.0] **Me:** f")
    sec = lambda v: np.full(SR, v, np.float32)  # noqa: E731
    me = np.concatenate([sec(0.001), sec(0.05), sec(0.001), sec(0.02)])  # noise, speech, noise, echo
    them = np.concatenate([sec(0), sec(0), sec(0), sec(0.1)])
    spans = speech_spans(me, them)
    assert len(spans) == 1 and abs(spans[0][0] - 0.85) < 0.01 and abs(spans[0][1] - 2.15) < 0.01, spans
    squeezed, table = squeeze(me, spans)
    assert len(squeezed) == int(1.3 * SR) + int(0.3 * SR)
    assert abs(unsqueeze(0.5, table) - 1.35) < 1e-9
    table = [(0.0, 10.0, 12.0), (2.3, 100.0, 105.0)]
    assert unsqueeze(2.3, table, end=True) == 12.0 and unsqueeze(2.3, table) == 100.0
    assert unsqueeze(2.1, table) == 12.0  # inside the separator
    # a shorter other track is silence past its end, not a replay of its start
    assert speech_spans(np.concatenate([sec(0.001), sec(0.05)]), sec(0.5)) != []
    # echo that lags the call audio by 100ms outlasts it by 100ms: still echo, not speech
    them = np.concatenate([sec(0), sec(0.1), sec(0)])
    late = np.concatenate([sec(0.001)[:1600], sec(0.001), sec(0.02), sec(0.001)])[:len(them)]
    assert speech_spans(late, them, shortest=0.05) == []
    assert stamp(59.96) == "01:00.0" and stamp(61.04) == "01:01.0"
    assert folder_name(0, "a/b\x00[c] " + "ש" * 200, "k").endswith(" [k]")
    assert len(folder_name(0, "ש" * 200, "k").encode()) <= 150 + 30
    assert pick_language(["en"] * 9 + ["he"]) == "en" and pick_language(["en"] * 7 + ["he"] * 3) == "he"
    assert pick_language(["en"], ["he"] * 5) == "he" and pick_language([], ["en"] * 5) == "en"
    assert pick_language([], ["en"], partial="en") == "en" and pick_language([], []) == "he"
    assert len(vote_windows(np.zeros(WINDOW * 100, np.float32))) == VOTE_WINDOWS
    assert vote_windows(np.zeros(WINDOW - 1, np.float32)) == []
    marked = np.repeat(np.arange(35, dtype=np.float32), WINDOW)  # window i is all i
    picked = [int(w[0]) for w in vote_windows(marked)]
    assert picked[0] == 0 and picked[-1] == 34 and len(set(picked)) == VOTE_WINDOWS, picked


selftest()
if __name__ == "__main__":
    main()
