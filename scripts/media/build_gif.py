#!/usr/bin/env python3
"""Assemble a directory of numbered PNG frames into an optimised animated GIF.

Written against Pillow rather than ffmpeg because the frames are stop-motion
UI captures, not video: flat colours, few of them, and long per-frame holds.
A shared adaptive palette plus per-frame difference boxes gets that down to a
size that is reasonable to put in a README, which a video encoder would not.

Usage: build_gif.py <frames-dir> <output.gif> [--width N] [--colors N]
"""

import argparse
import json
import pathlib
import sys

from PIL import Image, ImageChops

DEFAULT_WIDTH = 900
DEFAULT_COLORS = 128
# GIF stores per-frame delay in hundredths of a second, so anything finer is
# lost — and several browsers silently bump delays under 20ms to 100ms.
MIN_DELAY_MS = 30


def load_frames(frames_dir: pathlib.Path):
    files = sorted(frames_dir.glob("[0-9]*.png"))
    if not files:
        sys.exit(f"no frames in {frames_dir}")
    timing = frames_dir / "timing.json"
    if timing.exists():
        durations = json.loads(timing.read_text())["durations"]
    else:
        durations = [400] * len(files)
    if len(durations) != len(files):
        sys.exit(
            f"{frames_dir}: {len(files)} frames but {len(durations)} durations"
        )
    return [Image.open(f).convert("RGB") for f in files], durations


def quantise(frames, width: int, colors: int):
    """Scale to `width` and map every frame onto one shared palette.

    A per-frame palette is what makes naive GIF exports flicker: consecutive
    frames pick slightly different colours for the same unchanged pixels, so
    nothing can be stored as "unchanged".
    """
    if frames[0].width > width:
        height = round(frames[0].height * width / frames[0].width)
        # LANCZOS keeps the small type in a UI screenshot legible.
        frames = [f.resize((width, height), Image.LANCZOS) for f in frames]

    # Derive the palette from every frame at once, not just the first, or a
    # colour that appears only later gets dithered into noise.
    montage = Image.new("RGB", (frames[0].width, frames[0].height * len(frames)))
    for i, f in enumerate(frames):
        montage.paste(f, (0, i * f.height))
    palette = montage.quantize(colors=colors, method=Image.MEDIANCUT)

    return [f.quantize(palette=palette, dither=Image.NONE) for f in frames]


def changed_box(a: Image.Image, b: Image.Image):
    """Bounding box of the pixels that differ, or None if identical."""
    return ImageChops.difference(a.convert("RGB"), b.convert("RGB")).getbbox()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("frames_dir", type=pathlib.Path)
    ap.add_argument("output", type=pathlib.Path)
    ap.add_argument("--width", type=int, default=DEFAULT_WIDTH)
    ap.add_argument("--colors", type=int, default=DEFAULT_COLORS)
    args = ap.parse_args()

    frames, durations = load_frames(args.frames_dir)
    frames = quantise(frames, args.width, args.colors)
    durations = [max(d, MIN_DELAY_MS) for d in durations]

    # Drop frames identical to their predecessor, folding their time into the
    # frame that stays. Stop-motion captures often repeat.
    kept, kept_durations = [frames[0]], [durations[0]]
    for frame, duration in zip(frames[1:], durations[1:]):
        if changed_box(kept[-1], frame) is None:
            kept_durations[-1] += duration
        else:
            kept.append(frame)
            kept_durations.append(duration)

    args.output.parent.mkdir(parents=True, exist_ok=True)
    kept[0].save(
        args.output,
        save_all=True,
        append_images=kept[1:],
        duration=kept_durations,
        loop=0,
        optimize=True,
        # Leave each frame on screen; the next one paints over it. With a
        # shared palette this is what lets unchanged regions cost nothing.
        disposal=1,
    )

    size = args.output.stat().st_size
    print(
        f"{args.output.name}: {len(kept)} frames, "
        f"{sum(kept_durations) / 1000:.1f}s, {size / 1024:.0f} KiB"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
