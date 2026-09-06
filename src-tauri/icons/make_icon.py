#!/usr/bin/env python3
"""Generate the yourmem app icon set.

Concept: 记忆层叠 (memory cascade) — three "session cards" cascading down-right
into the past on a dark macOS-style squircle; the newest card is a speech
bubble carrying the brand blue→purple gradient, older ones fade out.
Deliberately staggered so it never reads as a hamburger menu.

Run from src-tauri/icons/:  python3 make_icon.py
Requires: pillow, numpy; icns needs macOS iconutil.
"""

import os
import subprocess

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

HERE = os.path.dirname(os.path.abspath(__file__))
S = 4096  # master render size, downscaled for every output

# Palette: dark navy plate (ui/style.css bg family) + amber cards — amber
# deliberately diverges from the UI accent #7c8cff (blue→purple reads as
# generic "AI product"); amber = preserved memory, warm-on-cool contrast.
PLATE_TOP = (0x26, 0x2F, 0x49)
PLATE_BOTTOM = (0x0C, 0x0F, 0x17)
CARD_NEWEST_L = (0xFF, 0xD3, 0x4D)   # gold
CARD_NEWEST_R = (0xF5, 0x9E, 0x0B)   # amber
CARD_MID = (0xC9, 0x9A, 0x3E)        # dim gold
CARD_OLD = (0x8F, 0x6E, 0x2E)        # bronze
GLOW = (0xF5, 0xB3, 0x2E)

CARD_W, CARD_H, CARD_R = 1980, 430, 150
STEP_X, STEP_Y = 210, 560  # cascade offset per card; vertical gap = STEP_Y - CARD_H


def vgrad(size, top, bottom):
    """Vertical gradient as an RGBA numpy float array."""
    t = np.linspace(0.0, 1.0, size, dtype=np.float32)[:, None]
    arr = np.empty((size, size, 4), dtype=np.float32)
    for c in range(3):
        arr[:, :, c] = top[c] + (bottom[c] - top[c]) * t
    arr[:, :, 3] = 255.0
    return arr


def over(dst, src_rgb, src_a):
    """Alpha-compose src (HxWx3 float) with per-pixel alpha (HxW float) onto dst float RGBA."""
    a3 = src_a[:, :, None]
    out = dst.copy()
    out[:, :, :3] = src_rgb * a3 + out[:, :, :3] * (1.0 - a3)
    out[:, :, 3] = src_a * 255.0 + out[:, :, 3] * (1.0 - src_a)
    return out


def squircle_mask(size, box, n=5.0, soft=64.0):
    """Superellipse (continuous-corner) mask, float32 in [0,1], anti-aliased.

    soft = field units per pixel at the edge; 64 at 4096 ≈ 5px transition.
    """
    axis = np.linspace(0, size - 1, size, dtype=np.float32)
    x, y = np.meshgrid(axis, axis)
    cx, cy = (box[0] + box[2]) / 2.0, (box[1] + box[3]) / 2.0
    a, b = (box[2] - box[0]) / 2.0, (box[3] - box[1]) / 2.0
    f = (np.abs(x - cx) / a) ** n + (np.abs(y - cy) / b) ** n
    return np.clip((1.0 - f) * soft + 0.5, 0.0, 1.0)


def card_mask(size, box, with_tail=False):
    """Rounded-rect card as an L-image mask; newest card gets a speech tail."""
    img = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(img)
    d.rounded_rectangle(box, radius=CARD_R, fill=255)
    if with_tail:
        x0, _, _, y1 = box
        d.polygon(
            [(x0 + 320, y1), (x0 + 145, y1), (x0 + 155, y1 + 175)],
            fill=255,
        )
    return img


def gradient_fill(size, box, left, right):
    """Horizontal gradient mapped across the box columns (not the full canvas)."""
    x0, x1 = box[0], box[2]
    t = np.clip((np.arange(size, dtype=np.float32) - x0) / (x1 - x0), 0.0, 1.0)
    arr = np.empty((size, size, 4), dtype=np.float32)
    for c in range(3):
        arr[:, :, c] = left[c] + (right[c] - left[c]) * t[None, :]
    arr[:, :, 3] = 255.0
    return arr


def render(plate_inset=0.09):
    size = S
    # --- plate: dark squircle; macOS 规范内缩 9%（82% 画布），Windows 任务栏
    # 邻居多满幅图形，同款内缩视觉上小一圈（真机反馈 2026-09-04）→ 全出血变体 ---
    m = int(size * plate_inset)
    plate_box = (m, m, size - m, size - m)
    plate_a = squircle_mask(size, plate_box)
    canvas = vgrad(size, PLATE_TOP, PLATE_BOTTOM)
    canvas[:, :, 3] = plate_a * 255.0

    # diagonal sheen from top-left, clipped to the plate
    axis = np.linspace(0.0, 1.0, size, dtype=np.float32)
    x, y = np.meshgrid(axis, axis)
    sheen = np.clip(1.35 - 1.7 * (x + y) / 2.0, 0.0, 1.0) ** 2
    white = np.empty((size, size, 3), dtype=np.float32)
    white[:] = 255.0
    canvas = over(canvas, white, sheen * 0.09 * plate_a)

    # faint rim so the icon holds its edge on dark backgrounds
    rim_a = np.clip(plate_a * (1.0 - plate_a) * 3.2, 0.0, 1.0) * 0.16
    canvas = over(canvas, white, rim_a)

    img = Image.fromarray(canvas.astype(np.uint8), "RGBA")

    # --- card geometry: cascade down-right, centered ---
    cx, cy = size / 2, size / 2 - 20
    span_x = CARD_W + 2 * STEP_X
    span_y = 2 * STEP_Y + CARD_H
    bx0 = cx - span_x / 2
    by0 = cy - span_y / 2

    newest = (bx0, by0)
    mid = (bx0 + STEP_X, by0 + STEP_Y)
    old = (bx0 + 2 * STEP_X, by0 + 2 * STEP_Y)

    def box_at(origin):
        return (origin[0], origin[1], origin[0] + CARD_W, origin[1] + CARD_H)

    # soft glow behind the newest card
    glow = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    nb = box_at(newest)
    pad = 150
    ImageDraw.Draw(glow).rounded_rectangle(
        (nb[0] - pad, nb[1] - pad, nb[2] + pad, nb[3] + pad),
        radius=CARD_R + pad,
        fill=(*GLOW, 110),
    )
    glow = glow.filter(ImageFilter.GaussianBlur(140))
    img = Image.alpha_composite(img, glow)

    # --- cards, oldest first so the newest paints on top ---
    # (origin, fill color or None for gradient, alpha, dot/line alphas)
    dot_r = 95
    for origin, color, alpha, dot_rgb, dot_a, line_rgb, line_a in [
        (old, CARD_OLD, 115, (0xE0, 0xC8, 0x9E), 150, (0xC9, 0xB4, 0x8E), 105),
        (mid, CARD_MID, 165, (0xFF, 0xE9, 0xC4), 210, (0xE8, 0xD2, 0xAA), 160),
        (newest, None, 255, (255, 255, 255), 255, (255, 255, 255), 235),
    ]:
        box = box_at(origin)
        clip = np.asarray(
            card_mask(size, box, with_tail=(origin is newest)), dtype=np.float32
        ) / 255.0

        layer = (
            gradient_fill(size, box, CARD_NEWEST_L, CARD_NEWEST_R)
            if color is None
            else np.broadcast_to(
                np.array((*color, alpha), dtype=np.float32)[None, None, :],
                (size, size, 4),
            ).copy()
        )
        layer[:, :, 3] *= clip
        img = Image.alpha_composite(
            img, Image.fromarray(layer.astype(np.uint8), "RGBA")
        )

        # leading dot + preview line: reads as a message row
        detail = Image.new("RGBA", (size, size), (0, 0, 0, 0))
        d = ImageDraw.Draw(detail)
        dcx, dcy = box[0] + 250, box[1] + CARD_H / 2
        d.ellipse(
            (dcx - dot_r, dcy - dot_r, dcx + dot_r, dcy + dot_r),
            fill=(*dot_rgb, dot_a),
        )
        lh = 124
        d.rounded_rectangle(
            (dcx + dot_r + 120, dcy - lh / 2, dcx + dot_r + 120 + 920, dcy + lh / 2),
            radius=lh / 2,
            fill=(*line_rgb, line_a),
        )
        det_np = np.asarray(detail, dtype=np.float32)
        det_np[:, :, 3] *= clip
        img = Image.alpha_composite(
            img, Image.fromarray(det_np.astype(np.uint8), "RGBA")
        )

    return img


def emit(master, icns=True):
    def scaled(px):
        return master.resize((px, px), Image.LANCZOS)

    for name, px in {
        "icon.png": 512,
        "32x32.png": 32,
        "128x128.png": 128,
        "128x128@2x.png": 256,
    }.items():
        scaled(px).save(os.path.join(HERE, name))

    # icns via iconutil（macOS only；其他平台保留已有 icns 不动）
    if icns:
        iconset = os.path.join(HERE, "icon.iconset")
        os.makedirs(iconset, exist_ok=True)
        for px in [16, 32, 64, 128, 256, 512, 1024]:
            scaled(px).save(os.path.join(iconset, f"icon_{px}x{px}.png"))
            scaled(px).save(os.path.join(iconset, f"icon_{px // 2}x{px // 2}@2x.png"))
        subprocess.run(["iconutil", "-c", "icns", iconset], check=True, cwd=HERE)
        subprocess.run(["rm", "-rf", iconset], check=True)

    # Windows ico
    scaled(256).save(
        os.path.join(HERE, "icon.ico"),
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    print("wrote icon.png, 32x32.png, 128x128.png, 128x128@2x.png"
          + (", icon.icns" if icns else "") + ", icon.ico")


if __name__ == "__main__":
    import sys

    # Windows/Linux 端 PNG+ico 用满幅变体（任务栏视觉与邻居一致）；
    # macOS 的 icns 保留 Big Sur 内缩规范（仅 darwin 能跑 iconutil）
    win_master = render(0.030)
    emit(win_master, icns=(sys.platform == "darwin"))
    if sys.platform == "darwin":
        emit_mac = render(0.09)
        emit(emit_mac, icns=True)
