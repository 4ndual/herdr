#!/usr/bin/env python3
"""Rasterize captured dash E2E frames (the exact pyte grids) into a QA video.

Reads OUTDIR/manifest.json + each <id>.txt grid, renders a labelled monospace
PNG per scenario (header banner with TC + label + PASS/FAIL), then encodes an
MP4 holding each frame a few seconds. The .txt grids remain the ground-truth
artifact for frame-by-frame transcription; this video is the visual companion.
"""
import os, sys, json, subprocess
from PIL import Image, ImageDraw, ImageFont

OUTDIR = os.environ.get("DASH_FRAMES", "/tmp/dash_frames")
MONO = "/System/Library/Fonts/Menlo.ttc"
FS = 18
PAD = 16
HEADER_H = 64
SECS_PER_FRAME = float(os.environ.get("SECS_PER_FRAME", "3.0"))

font = ImageFont.truetype(MONO, FS)
hfont = ImageFont.truetype(MONO, 22)
# Cell metrics from the monospace font.
bbox = font.getbbox("M")
CW = font.getlength("M")
CH = FS + 6

BG = (24, 24, 37)        # catppuccin mocha base
FG = (205, 214, 244)     # text
OKC = (166, 227, 161)    # green
BADC = (243, 139, 168)   # red
BANNER = (49, 50, 68)


def render_frame(fid, tc, label, ok, txt, out_png):
    lines = txt.split("\n")
    # drop the leading "# ..." comment line written into the .txt
    if lines and lines[0].startswith("# "):
        lines = lines[1:]
    cols = max((len(l) for l in lines), default=80)
    rows = len(lines)
    W = int(CW * cols) + 2 * PAD
    H = int(CH * rows) + 2 * PAD + HEADER_H
    img = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(img)
    d.rectangle([0, 0, W, HEADER_H], fill=BANNER)
    status = "PASS" if ok else "FAIL"
    d.text((PAD, 8), f"{tc}  {status}", font=hfont, fill=OKC if ok else BADC)
    d.text((PAD, 36), label[:120], font=ImageFont.truetype(MONO, 15), fill=FG)
    y = HEADER_H + PAD
    for line in lines:
        d.text((PAD, y), line, font=font, fill=FG)
        y += CH
    img.save(out_png)
    return out_png


def main():
    manifest = json.load(open(os.path.join(OUTDIR, "manifest.json")))
    pngs = []
    for fr in manifest:
        txt = open(fr["path"]).read()
        png = os.path.join(OUTDIR, fr["id"] + ".png")
        render_frame(fr["id"], fr["tc"], fr["label"], fr["ok"], txt, png)
        pngs.append(png)
        print("rendered", png)

    # Normalize all frames to the max canvas so ffmpeg gets a constant size.
    maxw = max(Image.open(p).width for p in pngs)
    maxh = max(Image.open(p).height for p in pngs)
    # ffmpeg needs even dimensions.
    maxw += maxw % 2
    maxh += maxh % 2
    listfile = os.path.join(OUTDIR, "frames.txt")
    with open(listfile, "w") as f:
        for p in pngs:
            im = Image.open(p)
            canvas = Image.new("RGB", (maxw, maxh), BG)
            canvas.paste(im, (0, 0))
            canvas.save(p)
            f.write(f"file '{p}'\n")
            f.write(f"duration {SECS_PER_FRAME}\n")
        f.write(f"file '{pngs[-1]}'\n")  # last frame needs a trailing entry

    out_mp4 = os.path.join(OUTDIR, "dash_e2e.mp4")
    cmd = ["ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", listfile,
           "-vf", "format=yuv420p", "-r", "30", out_mp4]
    subprocess.run(cmd, check=True, capture_output=True)
    print("VIDEO:", out_mp4)


if __name__ == "__main__":
    main()
