# -*- coding: utf-8 -*-
"""
画出应用图标：和界面品牌标记同一套视觉——圆角黑底 + 键盘轮廓。

按 4x 超采样绘制，再逐尺寸缩小，避免小尺寸下边缘发糊。
输出多尺寸 .ico（16/20/24/32/40/48/64/128/256），供 exe、任务栏、托盘共用。

加 --preview 会额外输出 assets/icon-preview.png（各尺寸并排），仅供人眼检查。
"""
import os
import sys

from PIL import Image, ImageDraw

OUT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "assets")
os.makedirs(OUT_DIR, exist_ok=True)

# 与 ui/index.html 的黑白灰配色一致：
# 深灰圆角底 + 白色键盘轮廓，小尺寸下依然清晰可辨
BASE_TOP = (36, 36, 36)
BASE_BOTTOM = (17, 17, 17)
INK = (250, 250, 250)

SS = 4          # 超采样倍数
BASE = 1024     # 基础画布（实际按 256*SS 缩放）


def rounded_mask(size, radius):
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    d.rounded_rectangle([0, 0, size - 1, size - 1], radius=radius, fill=255)
    return m


def make_master() -> Image.Image:
    """256 逻辑像素的母版，先放大到 BASE 再画。"""
    s = BASE
    k = s / 256.0

    # 圆角方形底 + 竖向渐变（近黑）
    grad = Image.new("RGB", (1, s))
    for y in range(s):
        t = y / (s - 1)
        grad.putpixel((0, y), tuple(
            round(BASE_TOP[i] + (BASE_BOTTOM[i] - BASE_TOP[i]) * t) for i in range(3)
        ))
    grad = grad.resize((s, s))

    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    img.paste(grad, (0, 0), rounded_mask(s, int(56 * k)))

    d = ImageDraw.Draw(img)

    # 键盘外框
    bx0, by0, bx1, by1 = 46 * k, 84 * k, 210 * k, 172 * k
    d.rounded_rectangle([bx0, by0, bx1, by1], radius=14 * k,
                        outline=INK, width=int(11 * k))

    # 三排按键点
    r = 7.5 * k
    tw = 9 * k                       # 描边宽度
    for row_y in (112, 140):
        for col_x in (78, 110, 142, 174):
            cx, cy = col_x * k, row_y * k
            d.ellipse([cx - r, cy - r, cx + r, cy + r], outline=INK, width=int(tw))

    # 空格键
    d.rounded_rectangle([92 * k, 150 * k, 164 * k, 158 * k], radius=4 * k, fill=INK)

    return img


def main():
    master = make_master()

    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    frames = []
    for sz in sizes:
        frames.append(master.resize((sz, sz), Image.LANCZOS))

    ico_path = os.path.join(OUT_DIR, "icon.ico")
    # PIL 的 ico 写入器会按给定尺寸重新编码，保留 alpha
    frames[-1].save(ico_path, format="ICO",
                    sizes=[(s, s) for s in sizes])

    # 单独存 256 PNG：编译时嵌入二进制，托盘图标用
    png256 = os.path.join(OUT_DIR, "icon-256.png")
    frames[-1].save(png256)

    # 预览图，方便人眼检查。只在显式要求时生成，避免每次构建都留下一个
    # 源码目录里的产物（已 gitignore，但没必要多一个文件）。
    if "--preview" in sys.argv:
        preview = Image.new("RGBA", (sum(sizes) + 10 * len(sizes), 256), (32, 32, 32, 255))
        x = 0
        for f in frames:
            preview.paste(f, (x, 128 - f.size[1] // 2), f)
            x += f.size[0] + 10
        preview.save(os.path.join(OUT_DIR, "icon-preview.png"))
        print("预览图: assets/icon-preview.png")

    print(f"ico: {os.path.getsize(ico_path)} bytes, sizes={sizes}")
    for sz, f in zip(sizes, frames):
        print(f"  {sz}x{sz}: {f.size}")


if __name__ == "__main__":
    main()
