#!/usr/bin/env python3
"""Generate the Codex Account Switcher logo and platform icon assets."""

from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "assets"
PNG_TRANSPARENT = ASSETS / "codex-account-switcher-transparent.png"
DOCK_PNG = ASSETS / "codex-account-switcher-dock.png"
ICO = ASSETS / "codex-account-switcher.ico"
ICNS = ASSETS / "codex-account-switcher.icns"

# ── Brand palette ─────────────────────────────────────────────────────────────
DOCK_BG = (12, 10, 9, 255)  # #0C0A09 warm black
STROKE_PRIMARY = (250, 250, 249, 255)  # #FAFAF9
ACCENT = (20, 184, 166, 255)  # #14B8A6 teal — active account / switch
ACCENT_RIM = (45, 212, 191, 220)  # #2DD4BF softer teal for dock rim

# ── Logo geometry (tune here) ─────────────────────────────────────────────────
LOGO_MARGIN_RATIO = 0.14
STROKE_DIVISOR = 11  # lower = thicker strokes (was 18)
# Interlocking diagonal arcs (chain-link / ∞), NOT left-right parentheses.
ARC_PRIMARY_START = 208
ARC_PRIMARY_END = 338
ARC_ACCENT_START = 28
ARC_ACCENT_END = 158
DOT_X_RATIO = 0.68  # unused — kept for tuning reference
DOT_Y_RATIO = 0.66
DOT_RADIUS_DIVISOR = 22

# Center swap arrow (↔) — account switch symbol
SWAP_ARROW_Y_RATIO = 0.50
SWAP_ARROW_HALF_SPAN_RATIO = 0.24
SWAP_ARROW_SHAFT_DIV = 16
SWAP_ARROW_HEAD_RATIO = 0.62

# ── Dock frame ────────────────────────────────────────────────────────────────
DOCK_OUTER_MARGIN_DIV = 10
DOCK_CORNER_RADIUS_DIV = 5
DOCK_LOGO_INSET_RATIO = 0.13
DOCK_RIM_DIV = 140  # lower = thicker teal rim (was 170)


def draw_bidirectional_arrow(draw, size: int, color) -> None:
    """Bold ↔ swap arrow at center — readable even at menu-bar size."""
    cx = size // 2
    cy = int(size * SWAP_ARROW_Y_RATIO)
    half_span = int(size * SWAP_ARROW_HALF_SPAN_RATIO)
    shaft = max(12, size // SWAP_ARROW_SHAFT_DIV)
    head = max(shaft * 2, int(half_span * SWAP_ARROW_HEAD_RATIO))

    x_left = cx - half_span
    x_right = cx + half_span

    draw.line([(x_left + head, cy), (x_right - head, cy)], fill=color, width=shaft)
    draw.polygon(
        [(x_left, cy), (x_left + head, cy - head), (x_left + head, cy + head)],
        fill=color,
    )
    draw.polygon(
        [(x_right, cy), (x_right - head, cy - head), (x_right - head, cy + head)],
        fill=color,
    )


def render_logo(size: int = 1024, *, colored_accent: bool = True):
    """Interlocking orbits + centered swap arrow."""
    from PIL import Image, ImageDraw

    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(canvas)

    margin = int(size * LOGO_MARGIN_RATIO)
    bbox = (margin, margin, size - margin, size - margin)
    stroke = max(18, size // STROKE_DIVISOR)
    accent_color = ACCENT if colored_accent else STROKE_PRIMARY

    # Diagonal interlock: arcs cross at center like linked orbits, not "(" ")".
    draw.arc(
        bbox,
        start=ARC_PRIMARY_START,
        end=ARC_PRIMARY_END,
        fill=STROKE_PRIMARY,
        width=stroke,
    )
    draw.arc(
        bbox,
        start=ARC_ACCENT_START,
        end=ARC_ACCENT_END,
        fill=accent_color,
        width=stroke,
    )

    arrow_color = ACCENT if colored_accent else STROKE_PRIMARY
    draw_bidirectional_arrow(draw, size, arrow_color)

    return canvas


def compose_dock_icon(size: int = 1024):
    from PIL import Image, ImageDraw

    logo = render_logo(size, colored_accent=True)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(canvas)

    outer_margin = size // DOCK_OUTER_MARGIN_DIV
    corner_radius = size // DOCK_CORNER_RADIUS_DIV
    draw.rounded_rectangle(
        (outer_margin, outer_margin, size - outer_margin, size - outer_margin),
        radius=corner_radius,
        fill=DOCK_BG,
    )

    rim = max(4, size // DOCK_RIM_DIV)
    draw.rounded_rectangle(
        (outer_margin, outer_margin, size - outer_margin, size - outer_margin),
        radius=corner_radius,
        outline=ACCENT_RIM,
        width=rim,
    )

    inset = int(size * DOCK_LOGO_INSET_RATIO)
    logo_size = size - inset * 2
    logo_resized = logo.resize((logo_size, logo_size), Image.Resampling.LANCZOS)
    canvas.paste(logo_resized, (inset, inset), logo_resized)
    return canvas


def write_transparent_png() -> None:
    # Monochrome for macOS menu-bar template rendering
    render_logo(colored_accent=False).save(PNG_TRANSPARENT)
    print(f"Wrote {PNG_TRANSPARENT}")


def write_dock_png() -> None:
    compose_dock_icon().save(DOCK_PNG)
    print(f"Wrote {DOCK_PNG}")


def generate_ico(source_path: Path) -> None:
    from PIL import Image

    source = Image.open(source_path).convert("RGBA")
    sizes = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]
    source.save(ICO, format="ICO", sizes=sizes)
    print(f"Wrote {ICO}")


def generate_icns(source_path: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="cas-iconset-") as tmp:
        iconset = Path(tmp) / "icon.iconset"
        iconset.mkdir()

        mappings = {
            "icon_16x16.png": (16, 16),
            "icon_16x16@2x.png": (32, 32),
            "icon_32x32.png": (32, 32),
            "icon_32x32@2x.png": (64, 64),
            "icon_128x128.png": (128, 128),
            "icon_128x128@2x.png": (256, 256),
            "icon_256x256.png": (256, 256),
            "icon_256x256@2x.png": (512, 512),
            "icon_512x512.png": (512, 512),
            "icon_512x512@2x.png": (1024, 1024),
        }

        for name, size in mappings.items():
            subprocess.run(
                [
                    "sips",
                    "-z",
                    str(size[1]),
                    str(size[0]),
                    str(source_path),
                    "--out",
                    str(iconset / name),
                ],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )

        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(ICNS)], check=True)
    print(f"Wrote {ICNS}")


def remove_stale_iconset() -> None:
    stale = ASSETS / "icon.iconset"
    if stale.exists():
        shutil.rmtree(stale)
        print(f"Removed stale {stale}")


def main() -> int:
    ASSETS.mkdir(parents=True, exist_ok=True)
    remove_stale_iconset()
    write_transparent_png()
    write_dock_png()
    generate_ico(DOCK_PNG)
    if shutil.which("sips") and shutil.which("iconutil"):
        generate_icns(DOCK_PNG)
    else:
        print("Skipping .icns generation (sips/iconutil not available)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
