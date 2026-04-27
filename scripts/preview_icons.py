"""Preview the 5 tray icon variants side by side.

Mirrors the math in `compose_with_dot()` in src-tauri/src/lib.rs so we can
visually inspect the dot placement and sizing without launching the full app.

Run:
    python3 scripts/preview_icons.py

Outputs scripts/preview.png.
"""
from __future__ import annotations

import math
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
BASE_PATH = ROOT / "src-tauri" / "icons" / "128x128.png"
OUT_PATH = ROOT / "scripts" / "preview.png"


def compose_with_dot(base: Image.Image, color: tuple[int, int, int]) -> Image.Image:
    base = base.convert("RGBA")
    pixels = list(base.getdata())
    w, h = base.size
    radius = min(w, h) * 0.13
    ring = max(1.0, min(w, h) * 0.025)
    pad = min(w, h) * 0.05 + ring
    cx = w - radius - pad
    cy = h - radius - pad
    r_outer = radius
    r_ring_outer = radius + ring

    out = pixels[:]
    for y in range(h):
        for x in range(w):
            dx = x + 0.5 - cx
            dy = y + 0.5 - cy
            dist = math.hypot(dx, dy)
            idx = y * w + x
            r, g, b, a = out[idx]
            if dist >= r_ring_outer:
                continue
            disc_cov = max(0.0, min(1.0, r_outer + 0.5 - dist))
            ring_cov = (
                max(0.0, min(1.0, r_ring_outer - dist)) * (1.0 - disc_cov)
                if dist > r_outer
                else 0.0
            )
            if disc_cov > 0.0:
                ai = int(disc_cov * 255)
                inv = 255 - ai
                r2 = (color[0] * ai + r * inv) // 255
                g2 = (color[1] * ai + g * inv) // 255
                b2 = (color[2] * ai + b * inv) // 255
                a2 = min(255, ai + (a * inv) // 255)
                r, g, b, a = r2, g2, b2, a2
            if ring_cov > 0.0:
                keep = int((1.0 - ring_cov) * 255)
                a = (a * keep) // 255
            out[idx] = (r, g, b, a)

    new = Image.new("RGBA", (w, h))
    new.putdata(out)
    return new


def main() -> None:
    base = Image.open(BASE_PATH).convert("RGBA")
    variants = [
        ("Idle", base),
        ("Starting", compose_with_dot(base, (0xFF, 0x9F, 0x0A))),
        ("Running USB", compose_with_dot(base, (0x30, 0xD1, 0x58))),
        ("Running Sim", compose_with_dot(base, (0x0A, 0x84, 0xFF))),
        ("Crashed", compose_with_dot(base, (0xFF, 0x45, 0x3A))),
    ]

    cell_w, cell_h = base.size
    label_h = 24
    pad = 16
    total_w = cell_w * len(variants) + pad * (len(variants) + 1)
    total_h = cell_h + label_h + pad * 2

    # Render once on a light bg, once on a dark bg, stacked vertically.
    out = Image.new("RGBA", (total_w, total_h * 2 + pad), (255, 255, 255, 255))
    light = Image.new("RGBA", (total_w, total_h), (245, 245, 247, 255))
    dark = Image.new("RGBA", (total_w, total_h), (30, 30, 32, 255))

    try:
        from PIL import ImageDraw, ImageFont

        font = ImageFont.load_default()
    except Exception:  # pragma: no cover
        font = None
        ImageDraw = None

    def paste_row(canvas: Image.Image, fg: tuple[int, int, int]) -> None:
        x = pad
        for label, img in variants:
            canvas.alpha_composite(img, (x, pad))
            if font is not None and ImageDraw is not None:
                draw = ImageDraw.Draw(canvas)
                draw.text((x, pad + cell_h + 4), label, fill=fg, font=font)
            x += cell_w + pad

    paste_row(light, (40, 40, 40))
    paste_row(dark, (230, 230, 230))

    out.alpha_composite(light, (0, 0))
    out.alpha_composite(dark, (0, total_h + pad))
    out.save(OUT_PATH)
    print(f"wrote {OUT_PATH}")


if __name__ == "__main__":
    main()
