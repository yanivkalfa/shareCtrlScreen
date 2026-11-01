#!/usr/bin/env python3
"""Regenerates the PNG sizes the Linux bundles (deb, rpm, AppImage) install into
the hicolor icon theme, from icon.png. Run from any directory:
python3 native/app/icons/generate-linux-icons.py

Needs Pillow. The output is deterministic (same input, same bytes). The checked-in
icon.png master is 32x32, so the larger sizes are upscaled; replace icon.png with a
512x512 (or larger) master and rerun to get crisp icons.
"""
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
SIZES = {"32x32.png": 32, "128x128.png": 128, "256x256.png": 256}

master = Image.open(HERE / "icon.png").convert("RGBA")
for name, px in SIZES.items():
    master.resize((px, px), Image.LANCZOS).save(HERE / name, optimize=True)
    print(f"wrote {name} ({px}x{px})")
