"""Draw the tutorial's sample picture (app/src-tauri/assets/tutorial-sample.jpg).

A simple painted room: window, table, and a blue vase that the tutorial asks
the user to mask and replace. Drawn from code so the repo owns it outright.

    python dev/make-tutorial-sample.py   (from app/, needs Pillow)
"""

from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

W = H = 1024
OUT = (
    Path(__file__).resolve().parent.parent
    / "src-tauri"
    / "assets"
    / "tutorial-sample.jpg"
)


def lerp(
    a: tuple[int, int, int], b: tuple[int, int, int], t: float
) -> tuple[int, int, int]:
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))  # type: ignore[return-value]


def vgradient(
    d: ImageDraw.ImageDraw, box: tuple[int, int, int, int], top, bottom
) -> None:
    x0, y0, x1, y1 = box
    for y in range(y0, y1):
        d.line(
            [(x0, y), (x1, y)], fill=lerp(top, bottom, (y - y0) / max(1, y1 - y0 - 1))
        )


img = Image.new("RGB", (W, H))
d = ImageDraw.Draw(img)

# Wall and floor.
vgradient(d, (0, 0, W, 700), (236, 214, 178), (214, 186, 146))
vgradient(d, (0, 700, W, H), (150, 104, 70), (112, 74, 48))

# Window: sky, clouds, a hill, frame.
wx0, wy0, wx1, wy1 = 560, 110, 900, 470
vgradient(d, (wx0, wy0, wx1, wy1), (120, 176, 232), (196, 226, 246))
for cx, cy, r in [
    (640, 190, 34),
    (676, 178, 44),
    (716, 192, 32),
    (800, 260, 26),
    (830, 252, 34),
]:
    d.ellipse((cx - r, cy - r * 0.7, cx + r, cy + r * 0.7), fill=(250, 250, 252))
hill = Image.new("L", (W, H), 0)
ImageDraw.Draw(hill).ellipse((480, 380, 980, 640), fill=255)
clip = Image.new("L", (W, H), 0)
ImageDraw.Draw(clip).rectangle((wx0, wy0, wx1, wy1), fill=255)
img.paste((118, 170, 96), (0, 0), ImageChops.multiply(hill, clip))
d = ImageDraw.Draw(img)
d.rectangle((wx0, wy0, wx1, wy1), outline=(250, 244, 232), width=18)
d.line(
    [((wx0 + wx1) // 2, wy0), ((wx0 + wx1) // 2, wy1)], fill=(250, 244, 232), width=12
)
d.line(
    [(wx0, (wy0 + wy1) // 2), (wx1, (wy0 + wy1) // 2)], fill=(250, 244, 232), width=12
)
d.rectangle((wx0 - 30, wy1 + 4, wx1 + 30, wy1 + 26), fill=(246, 238, 222))

# Table.
d.polygon([(90, 620), (934, 620), (1000, 700), (24, 700)], fill=(168, 112, 66))
d.rectangle((24, 700, 1000, 736), fill=(128, 82, 46))
for x in (70, 930):
    d.rectangle((x, 736, x + 34, 1000), fill=(118, 76, 42))

# Shadow, then the vase (the thing the user masks), then flowers.
shadow = Image.new("L", (W, H), 0)
ImageDraw.Draw(shadow).ellipse((300, 640, 540, 690), fill=120)
img.paste((96, 60, 34), (0, 0), shadow.filter(ImageFilter.GaussianBlur(10)))
d = ImageDraw.Draw(img)
vx = 400
for i in range(40):  # body, shaded left to right
    t = i / 39
    col = lerp((40, 80, 160), (110, 160, 226), 1 - abs(t - 0.35) * 1.4)
    d.ellipse((vx - 110 + i, 430 + i // 3, vx + 110 - i, 660 - i // 4), fill=col)
d.rectangle((vx - 36, 330, vx + 36, 470), fill=(58, 104, 186))
d.rectangle((vx - 30, 330, vx - 12, 470), fill=(92, 140, 210))
d.ellipse((vx - 50, 316, vx + 50, 346), fill=(70, 118, 198))
d.ellipse((vx - 38, 322, vx + 38, 340), fill=(30, 56, 110))
for sx, sy, ex, ey, col in [
    (vx - 8, 330, vx - 90, 190, (232, 96, 86)),
    (vx + 4, 330, vx + 20, 150, (246, 196, 72)),
    (vx + 10, 330, vx + 110, 210, (236, 128, 164)),
]:
    d.line([(sx, sy), (ex, ey)], fill=(70, 128, 62), width=7)
    for k in range(6):
        dx, dy = [(0, -26), (24, -8), (15, 22), (-15, 22), (-24, -8), (0, 0)][k]
        r = 20 if k < 5 else 14
        c = col if k < 5 else (250, 230, 120)
        d.ellipse((ex + dx - r, ey + dy - r, ex + dx + r, ey + dy + r), fill=c)

img = img.filter(ImageFilter.SMOOTH_MORE)
OUT.parent.mkdir(parents=True, exist_ok=True)
img.save(OUT, "JPEG", quality=88, optimize=True, progressive=True)
print(f"{OUT} {OUT.stat().st_size} bytes")
