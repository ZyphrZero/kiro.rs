#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成 Kiro Tray 托盘图标"""
import sys
import os
sys.stdout.reconfigure(encoding='utf-8')

from PIL import Image, ImageDraw, ImageFont
import io

def create_icon(size, color, bg_alpha=255):
    """创建单个尺寸的图标

    设计理念：现代简约风格，类似 Cloudflare 的橙色配色
    - 使用渐变圆形背景
    - 白色 'K' 字母，加粗且居中
    - 支持透明度和抗锯齿
    """
    img = Image.new('RGBA', (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    # 绘制渐变圆形背景
    center = size // 2
    radius = size // 2 - 2

    # 创建圆形遮罩
    mask = Image.new('L', (size, size), 0)
    mask_draw = ImageDraw.Draw(mask)
    mask_draw.ellipse([2, 2, size-2, size-2], fill=255)

    # 创建渐变背景
    gradient = Image.new('RGBA', (size, size), (0, 0, 0, 0))
    grad_draw = ImageDraw.Draw(gradient)

    # 从上到下的渐变
    r1, g1, b1 = color  # 顶部颜色
    r2 = min(255, int(r1 * 1.2))  # 底部稍亮
    g2 = min(255, int(g1 * 1.2))
    b2 = min(255, int(b1 * 1.2))

    for y in range(size):
        ratio = y / size
        r = int(r1 + (r2 - r1) * ratio)
        g = int(g1 + (g2 - g1) * ratio)
        b = int(b1 + (b2 - b1) * ratio)
        grad_draw.line([(0, y), (size, y)], fill=(r, g, b, bg_alpha))

    # 应用圆形遮罩
    gradient.putalpha(mask)
    img = Image.alpha_composite(img, gradient)

    # 绘制 'K' 字母
    draw = ImageDraw.Draw(img)

    # 尝试使用系统字体
    font_size = int(size * 0.55)
    try:
        # Windows 字体
        font = ImageFont.truetype("segoeui.ttf", font_size)
    except:
        try:
            font = ImageFont.truetype("arial.ttf", font_size)
        except:
            font = ImageFont.load_default()

    text = "K"

    # 计算文字位置（居中）
    bbox = draw.textbbox((0, 0), text, font=font)
    text_width = bbox[2] - bbox[0]
    text_height = bbox[3] - bbox[1]
    x = (size - text_width) // 2 - bbox[0]
    y = (size - text_height) // 2 - bbox[1]

    # 绘制白色文字
    draw.text((x, y), text, fill=(255, 255, 255, 255), font=font)

    return img

def create_status_icons():
    """创建三种状态的图标"""
    # 绿色（正常运行）
    green = (46, 160, 67)
    # 红色（错误）
    red = (207, 34, 46)
    # 橙色（警告/启动中）
    orange = (255, 153, 0)

    icons = {
        'ok': green,
        'bad': red,
        'wait': orange
    }

    script_dir = os.path.dirname(os.path.abspath(__file__))

    for name, color in icons.items():
        # ICO 文件需要多个尺寸
        sizes = [16, 20, 24, 32, 40, 48, 64, 256]
        images = []

        for size in sizes:
            img = create_icon(size, color)
            images.append(img)

        # 保存为 ICO 文件
        # Pillow 只会保留不大于主图的尺寸，所以主图必须是最大的那张
        output_path = os.path.join(script_dir, f"icon_{name}.ico")
        images[-1].save(
            output_path,
            format='ICO',
            sizes=[(img.width, img.height) for img in images],
            append_images=images[:-1]
        )
        print(f"✓ 已生成 {output_path}")

def create_preview():
    """创建预览图片"""
    preview = Image.new('RGBA', (400, 150), (240, 240, 240, 255))
    draw = ImageDraw.Draw(preview)

    # 标题
    try:
        title_font = ImageFont.truetype("segoeui.ttf", 16)
        label_font = ImageFont.truetype("segoeui.ttf", 12)
    except:
        title_font = ImageFont.load_default()
        label_font = ImageFont.load_default()

    draw.text((10, 10), "Kiro Tray 图标预览", fill=(0, 0, 0), font=title_font)

    # 三种状态
    colors = [
        ((46, 160, 67), "正常运行", 50),
        ((255, 153, 0), "启动中", 180),
        ((207, 34, 46), "错误", 310)
    ]

    for color, label, x_offset in colors:
        # 绘制图标
        icon = create_icon(48, color)
        preview.paste(icon, (x_offset, 50), icon)

        # 绘制标签
        bbox = draw.textbbox((0, 0), label, font=label_font)
        text_width = bbox[2] - bbox[0]
        draw.text((x_offset + 24 - text_width // 2, 110), label, fill=(0, 0, 0), font=label_font)

    script_dir = os.path.dirname(os.path.abspath(__file__))
    preview.save(os.path.join(script_dir, "icon_preview.png"))
    print("✓ 已生成预览图 icon_preview.png")

if __name__ == '__main__':
    print("正在生成 Kiro Tray 图标...")
    create_status_icons()
    create_preview()
    print("\n完成！")
