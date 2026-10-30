"""Spike jetable : diarisation via OpenRouter.

Usage : KEYFILE=<clé> python3 -I spike_diar.py <whole|chunks> <audio.ogg> <outdir> <model:tag> [...]
"""
import base64, collections, json, os, re, subprocess, sys, time
import requests

API = "https://openrouter.ai/api/v1"
KEY = open(os.environ["KEYFILE"]).read().strip()


def call(path, model, tag, opts):
    data = base64.b64encode(open(path, "rb").read()).decode()
    body = {"model": model, "input_audio": {"data": data, "format": "ogg"}, "language": "fr",
            "response_format": "verbose_json", "timestamp_granularities": ["segment", "word"],
            "provider": {"options": {tag: opts}}}
    t0 = time.time()
    r = requests.post(f"{API}/audio/transcriptions", headers={"Authorization": f"Bearer {KEY}"}, json=body, timeout=300)
    lat = round(time.time() - t0, 1)
    try:
        j = r.json()
    except ValueError:
        j = {"raw": r.text[:300]}
    return r.status_code, lat, j


def summarize(j):
    words = j.get("words") or []
    segs = j.get("segments") or []
    wsp = [w.get("speaker") for w in words]
    ssp = [s.get("speaker") for s in segs]
    turns = []  # (speaker, start, end, text) en regroupant les mots consécutifs
    for w in words:
        sp = w.get("speaker")
        if turns and turns[-1][0] == sp:
            turns[-1][2] = w["end"]
            turns[-1][3].append(w["word"])
        else:
            turns.append([sp, w["start"], w["end"], [w["word"]]])
    return {
        "segments": len(segs), "words": len(words),
        "speakers_words": dict(collections.Counter(wsp)), "speakers_segs": dict(collections.Counter(ssp)),
        "turns": len(turns), "cost": (j.get("usage") or {}).get("cost"),
        "turn_sample": [(t[0], round(t[1], 1), " ".join(t[3])[:70]) for t in turns[:12]],
        "_turns": turns,
    }


def cut(src, outdir, a, b):
    p = os.path.join(outdir, f"chunk_{a:.0f}_{b:.0f}.ogg")
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-ss", str(a), "-t", str(b - a), "-i", src, "-c", "copy", p], check=True)
    return p


def silences(src):
    p = subprocess.run(["ffmpeg", "-hide_banner", "-nostats", "-i", src, "-af", "silencedetect=noise=-30dB:d=0.4",
                        "-f", "null", "-"], capture_output=True, text=True)
    s = [float(x) for x in re.findall(r"silence_start: (-?[\d.]+)", p.stderr)]
    e = [float(x) for x in re.findall(r"silence_end: ([\d.]+)", p.stderr)]
    return [(x + y) / 2 for x, y in zip(s, e)]


def plan(total, target, sil, window=30):
    cuts, pos = [0.0], 0.0
    while total - pos > target * 1.25:
        goal = pos + target
        near = [s for s in sil if abs(s - goal) <= window]
        pos = min(near, key=lambda s: abs(s - goal)) if near else goal
        cuts.append(pos)
    cuts.append(total)
    return list(zip(cuts[:-1], cuts[1:]))


def main():
    mode, src, outdir = sys.argv[1:4]
    os.makedirs(outdir, exist_ok=True)
    for spec in sys.argv[4:]:
        model, tag = spec.split(":")
        opts = {"diarize": True}
        name = model.replace("/", "_")
        if mode == "whole":
            st, lat, j = call(src, model, tag, opts)
            json.dump(j, open(os.path.join(outdir, f"whole_{name}.json"), "w"), ensure_ascii=False, indent=1)
            if st != 200:
                print(model, st, lat, json.dumps(j)[:300]); continue
            s = summarize(j); s.pop("_turns")
            print(json.dumps({"model": model, "status": st, "latency": lat, **s}, ensure_ascii=False, indent=1))
        else:
            total = float(subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", src],
                                         capture_output=True, text=True).stdout)
            whole = json.load(open(os.path.join(outdir, f"whole_{name}.json")))
            ref = [(w["start"], w["end"], w.get("speaker")) for w in whole.get("words", [])]
            cost = 0
            for a, b in plan(total, 300, silences(src)):
                st, lat, j = call(cut(src, outdir, a, b), model, tag, opts)
                cost += (j.get("usage") or {}).get("cost") or 0
                # correspondance locuteur du morceau -> locuteur du fichier entier, par recouvrement temporel des mots
                votes = collections.defaultdict(collections.Counter)
                for w in j.get("words", []):
                    mid = (w["start"] + w["end"]) / 2 + a
                    best = min(ref, key=lambda r: abs((r[0] + r[1]) / 2 - mid)) if ref else None
                    if best:
                        votes[w.get("speaker")][best[2]] += 1
                mapping = {k: v.most_common(3) for k, v in votes.items()}
                print(json.dumps({"model": model, "chunk": [round(a, 1), round(b, 1)], "status": st, "latency": lat,
                                  "chunk_speakers -> whole_speakers (votes)": {str(k): v for k, v in mapping.items()}},
                                 ensure_ascii=False))
            print(f"# coût morceaux {model}: {cost:.4f} $")


main()
