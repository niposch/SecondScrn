"""Generates the Windows app and tray icons (run: python tools/make_icons.py)."""
from pathlib import Path
from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "apps" / "windows" / "SecondScrn.App" / "Assets"
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def glyph(size, fg, bg=None, dot=None):
    s = 4  # supersample
    n = size * s
    im = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    if bg:
        d.rounded_rectangle([0, 0, n - 1, n - 1], radius=n // 5, fill=bg)
    pad = n * (0.16 if bg else 0.06)
    w = n - 2 * pad
    lw = max(s, int(n * 0.07))
    # Monitor
    mx0, my0, mx1, my1 = pad, pad + w * 0.12, pad + w * 0.66, pad + w * 0.62
    d.rounded_rectangle([mx0, my0, mx1, my1], radius=n * 0.04, outline=fg, width=lw)
    d.line([(mx0 + (mx1 - mx0) / 2, my1), (mx0 + (mx1 - mx0) / 2, my1 + w * 0.12)], fill=fg, width=lw)
    d.line([(mx0 + (mx1 - mx0) * 0.25, my1 + w * 0.13), (mx0 + (mx1 - mx0) * 0.75, my1 + w * 0.13)], fill=fg, width=lw)
    # Tablet
    tx0, ty0, tx1, ty1 = pad + w * 0.72, pad + w * 0.30, pad + w, pad + w * 0.88
    d.rounded_rectangle([tx0, ty0, tx1, ty1], radius=n * 0.04, outline=fg, width=lw)
    if dot:
        r = n * 0.17
        cx, cy = n - r - s, n - r - s
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=dot, outline=(0, 0, 0, 160), width=s)
    return im.resize((size, size), Image.LANCZOS)


def ico(path, render):
    imgs = [render(sz) for sz in SIZES]
    imgs[-1].save(path, sizes=[(i.width, i.height) for i in imgs], append_images=imgs[:-1])


OUT.mkdir(parents=True, exist_ok=True)
ico(OUT / "SecondScrn.ico", lambda sz: glyph(sz, (255, 255, 255, 255), bg=(30, 90, 168, 255)))
glyph(256, (255, 255, 255, 255), bg=(30, 90, 168, 255)).save(OUT / "SecondScrn.png")
states = {"idle": None, "connected": (60, 200, 90, 255), "warning": (245, 170, 30, 255), "error": (230, 60, 60, 255)}
for theme, fg in {"dark": (255, 255, 255, 255), "light": (20, 20, 20, 255)}.items():
    for name, dot in states.items():
        ico(OUT / f"tray-{name}-{theme}.ico", lambda sz, fg=fg, dot=dot: glyph(sz, fg, dot=dot))
print("icons written to", OUT)
