"""Spike jetable srtty : sonde l'endpoint STT d'OpenRouter.

Usage : KEYFILE=<fichier clé> python3 -I spike.py <phase> <media> <outdir>
Phases : probe | latency | formats | modes | e2e
"""
import base64, concurrent.futures as cf, difflib, json, os, re, subprocess, sys, time, unicodedata
import requests

API = "https://openrouter.ai/api/v1"
KEY = open(os.environ["KEYFILE"]).read().strip()
BUDGET = 1.00  # USD, plafond dur sur toute la session
SPENT_FILE = None

MIME = {"mp3": "audio/mpeg", "ogg": "audio/ogg", "flac": "audio/flac"}


import threading
_LOCK = threading.Lock()
_SPENT = None


def spent():
    global _SPENT
    with _LOCK:
        if _SPENT is None:
            try:
                _SPENT = float(open(SPENT_FILE).read() or 0)
            except FileNotFoundError:
                _SPENT = 0.0
        return _SPENT


def add_spent(c):
    global _SPENT
    spent()
    with _LOCK:
        _SPENT += c or 0
        with open(SPENT_FILE, "w") as f:
            f.write(str(_SPENT))


def cut(media, outdir, start, dur, fmt):
    """Extrait [start, start+dur] en mono 16 kHz, encodé selon fmt."""
    enc = {
        "mp3_64": ("mp3", ["-c:a", "libmp3lame", "-b:a", "64k"]),
        "mp3_32": ("mp3", ["-c:a", "libmp3lame", "-b:a", "32k"]),
        "opus_32": ("ogg", ["-c:a", "libopus", "-b:a", "32k", "-application", "voip"]),
        "opus_24": ("ogg", ["-c:a", "libopus", "-b:a", "24k", "-application", "voip"]),
        "opus_16": ("ogg", ["-c:a", "libopus", "-b:a", "16k", "-application", "voip"]),
        "flac": ("flac", ["-c:a", "flac"]),
    }[fmt]
    ext, args = enc
    path = os.path.join(outdir, "clips", f"{start:g}_{dur:g}_{fmt}.{ext}")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    if not os.path.exists(path):
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-ss", str(start), "-t", str(dur), "-i", media,
                        "-vn", "-ac", "1", "-ar", "16000", *args, path], check=True)
    return path, ext


def transcribe(path, ext, model, mode="multipart", words=True, prompt=None, provider_opts=None):
    if spent() >= BUDGET:
        raise SystemExit(f"budget atteint ({spent():.4f} $)")
    gran = ["segment", "word"] if words else ["segment"]
    data = open(path, "rb").read()
    t0 = time.time()
    try:
        if mode == "multipart":
            form = [("model", model), ("response_format", "verbose_json"), ("language", "fr")]
            form += [("timestamp_granularities[]", g) for g in gran]
            if prompt:
                form.append(("prompt", prompt))
            r = requests.post(f"{API}/audio/transcriptions", headers={"Authorization": f"Bearer {KEY}"},
                              files={"file": (os.path.basename(path), data, MIME[ext])}, data=form, timeout=180)
        else:
            body = {"model": model, "input_audio": {"data": base64.b64encode(data).decode(), "format": ext},
                    "response_format": "verbose_json", "timestamp_granularities": gran, "language": "fr"}
            if provider_opts:
                body["provider"] = {"options": provider_opts}
            r = requests.post(f"{API}/audio/transcriptions", headers={"Authorization": f"Bearer {KEY}"},
                              json=body, timeout=180)
        lat = time.time() - t0
        try:
            j = r.json()
        except ValueError:
            j = {"raw": r.text[:500]}
    except requests.RequestException as e:
        return {"status": "EXC", "error": repr(e)[:300], "latency": time.time() - t0, "bytes": len(data)}
    cost = (j.get("usage") or {}).get("cost")
    add_spent(cost)
    segs = j.get("segments") or []
    return {
        "status": r.status_code, "latency": round(lat, 2), "bytes": len(data), "cost": cost,
        "segments": len(segs), "words": len(j.get("words") or []), "language": j.get("language"),
        "duration": j.get("duration"), "error": None if r.ok else json.dumps(j)[:400],
        "first_seg": segs[0] if segs else None, "seg_keys": sorted(segs[0].keys()) if segs else None,
        "_json": j,
    }


def log(outdir, rec):
    raw = rec.pop("_json", None)
    with open(os.path.join(outdir, "results.jsonl"), "a") as f:
        f.write(json.dumps(rec, ensure_ascii=False) + "\n")
    if raw is not None:
        name = re.sub(r"[^A-Za-z0-9_.-]", "_", f"{rec['phase']}_{rec['model']}_{rec.get('mode')}_{rec.get('tag','')}")
        os.makedirs(os.path.join(outdir, "raw"), exist_ok=True)
        json.dump(raw, open(os.path.join(outdir, "raw", name + ".json"), "w"), ensure_ascii=False, indent=1)
    short = {k: v for k, v in rec.items() if k not in ("first_seg", "error")}
    print(json.dumps(short, ensure_ascii=False), ("ERR " + rec["error"][:160]) if rec.get("error") else "", flush=True)


def norm_words(t):
    t = unicodedata.normalize("NFKC", t.lower())
    return re.findall(r"\w+", t)


def similarity(a, b):
    return round(difflib.SequenceMatcher(None, norm_words(a), norm_words(b), autojunk=False).ratio(), 4)


def phase_probe(media, outdir, models):
    path, ext = cut(media, outdir, 60, 30, "mp3_64")
    jobs = [(m, mode) for m in models for mode in ("multipart", "json-base64")]
    with cf.ThreadPoolExecutor(4) as ex:
        futs = {ex.submit(transcribe, path, ext, m, mode): (m, mode) for m, mode in jobs}
        for fu in cf.as_completed(futs):
            m, mode = futs[fu]
            log(outdir, {"phase": "probe", "model": m, "mode": mode, **fu.result()})


def phase_latency(media, outdir, models):
    for m in models:
        for dur in (60, 180, 300, 600):
            path, ext = cut(media, outdir, 0, dur, "opus_32")
            log(outdir, {"phase": "latency", "model": m, "mode": "multipart", "tag": f"{dur}s", "clip_s": dur,
                         **transcribe(path, ext, m, words=True)})


def phase_formats(media, outdir, model):
    texts = {}
    for fmt in ("flac", "mp3_64", "mp3_32", "opus_32", "opus_24", "opus_16"):
        path, ext = cut(media, outdir, 120, 300, fmt)
        rec = transcribe(path, ext, model, words=False)
        texts[fmt] = (rec.get("_json") or {}).get("text", "")
        rec["sim_vs_flac"] = similarity(texts["flac"], texts[fmt]) if texts.get("flac") else None
        log(outdir, {"phase": "formats", "model": model, "mode": "multipart", "tag": fmt, **rec})


def phase_modes(media, outdir, models):
    path, ext = cut(media, outdir, 120, 300, "opus_32")
    for m in models:
        for mode in ("multipart", "json-base64"):
            log(outdir, {"phase": "modes", "model": m, "mode": mode, "tag": mode,
                         **transcribe(path, ext, m, mode=mode, words=False)})


def silences(media, noise="-30dB", d=0.4):
    p = subprocess.run(["ffmpeg", "-hide_banner", "-nostats", "-i", media, "-vn", "-af",
                        f"silencedetect=noise={noise}:d={d}", "-f", "null", "-"], capture_output=True, text=True)
    starts = [float(x) for x in re.findall(r"silence_start: (-?[\d.]+)", p.stderr)]
    ends = [float(x) for x in re.findall(r"silence_end: ([\d.]+)", p.stderr)]
    return [((s + e) / 2) for s, e in zip(starts, ends)]


def duration(media):
    out = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", media],
                         capture_output=True, text=True).stdout
    return float(out)


def plan_cuts(total, target, sil, window=30):
    cuts, pos = [0.0], 0.0
    while total - pos > target * 1.25:
        goal = pos + target
        near = [s for s in sil if abs(s - goal) <= window] if sil is not None else []
        nxt = min(near, key=lambda s: abs(s - goal)) if near else goal
        cuts.append(nxt)
        pos = nxt
    cuts.append(total)
    return list(zip(cuts[:-1], cuts[1:]))


def srt_ts(t):
    ms = int(round(t * 1000))
    h, ms = divmod(ms, 3600000)
    m, ms = divmod(ms, 60000)
    s, ms = divmod(ms, 1000)
    return f"{h:02}:{m:02}:{s:02},{ms:03}"


def phase_e2e(media, outdir, model, target=180):
    total = duration(media)
    sil = silences(media)
    for label, plan in (("silence", plan_cuts(total, target, sil)), ("fixed", plan_cuts(total, target, None))):
        chunks = [(i, a, b, *cut(media, outdir, a, b - a, "opus_32")) for i, (a, b) in enumerate(plan)]
        t0 = time.time()
        with cf.ThreadPoolExecutor(4) as ex:
            res = list(ex.map(lambda c: (c, transcribe(c[3], c[4], model, words=False)), chunks))
        wall = time.time() - t0
        segs, cost = [], 0.0
        for (i, a, b, _, _), r in res:
            cost += r.get("cost") or 0
            for s in (r.get("_json") or {}).get("segments", []):
                segs.append({"chunk": i, "start": s["start"] + a, "end": s["end"] + a, "text": s["text"].strip()})
        mono = all(segs[k]["start"] >= segs[k - 1]["start"] for k in range(1, len(segs)))
        overlaps = sum(1 for k in range(1, len(segs)) if segs[k]["start"] < segs[k - 1]["end"] - 0.01)
        past_end = sum(1 for (i, a, b, _, _), r in res for s in (r.get("_json") or {}).get("segments", [])
                       if s["end"] + a > b + 0.5)
        with open(os.path.join(outdir, f"e2e_{label}.srt"), "w") as f:
            for n, s in enumerate(segs, 1):
                f.write(f"{n}\n{srt_ts(s['start'])} --> {srt_ts(s['end'])}\n{s['text']}\n\n")
        joins = []
        for k in range(1, len(segs)):
            if segs[k]["chunk"] != segs[k - 1]["chunk"]:
                joins.append({"cut_at": round(plan[segs[k]["chunk"]][0], 2),
                              "before": segs[k - 1]["text"][-80:], "after": segs[k]["text"][:80]})
        rec = {"phase": "e2e", "model": model, "mode": "multipart", "tag": label, "chunks": len(plan),
               "cuts": [round(a, 1) for a, _ in plan], "segments": len(segs), "monotonic": mono,
               "overlaps": overlaps, "segs_past_chunk_end": past_end, "wall_s": round(wall, 1),
               "cost": round(cost, 6), "max_seg_s": round(max(s["end"] - s["start"] for s in segs), 2),
               "statuses": [r["status"] for _, r in res], "joins": joins}
        with open(os.path.join(outdir, "results.jsonl"), "a") as f:
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")
        print(json.dumps(rec, ensure_ascii=False, indent=1), flush=True)


if __name__ == "__main__":
    phase, media, outdir = sys.argv[1:4]
    os.makedirs(outdir, exist_ok=True)
    SPENT_FILE = os.path.join(outdir, "spent.txt")
    extra = sys.argv[4:]
    {"probe": lambda: phase_probe(media, outdir, extra),
     "latency": lambda: phase_latency(media, outdir, extra),
     "formats": lambda: phase_formats(media, outdir, extra[0]),
     "modes": lambda: phase_modes(media, outdir, extra),
     "e2e": lambda: phase_e2e(media, outdir, extra[0], int(extra[1]) if len(extra) > 1 else 180),
     }[phase]()
    print(f"# dépensé cumulé : {spent():.5f} $")
