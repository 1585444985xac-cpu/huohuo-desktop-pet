#!/usr/bin/env python3
"""
Live2D 纹理降采样工具
=====================

把 public/model/character/huohuo.8192/ 下的 8192x8192 纹理降采样到目标分辨率。

背景：桌宠主窗口为 1200x1500 px，8192 纹理远超实际需要。降采样到 2048 后
      4 张纹理合计从 46 MB 降到约 4 MB。

安全性：
  - Live2D 的 UV 坐标是归一化的 (0.0~1.0)，不依赖纹理像素尺寸，
    因此降采样纹理不会破坏 moc3 模型，所有动作/表情照常工作。
  - 保留 RGBA 通道（alpha 是 Live2D 透明背景的关键），不改变颜色模式。
  - 使用 LANCZOS 重采样（PIL 中质量最高的降采样滤波器）。
  - 文件名与目录名保持不变，huohuo.model3.json 无需修改。

质量度量说明（重要）：
  Live2D 纹理图集中大片区域 alpha = 0（完全透明），这些区域下的 RGB 往往是
  无意义的残留数据。直接对 RGBA 求 PSNR 会被这些不可见像素严重污染，
  得到毫无参考价值的数字（实测会低到 2~3 dB）。
  本脚本因此使用 **alpha 感知的分块 PSNR**：只在预乘 alpha 后的 RGBA 上比较，
  即透明像素不参与误差计算 —— 这才对应"肉眼可见的差异"。

用法：
    python scripts/downscale-textures.py                  # 降到 2048
    python scripts/downscale-textures.py --size 4096      # 降到 4096
    python scripts/downscale-textures.py --dry-run        # 只报告，不写文件
    python scripts/downscale-textures.py --verify         # 对比备份，量化损失
"""

from __future__ import annotations

import argparse
import math
import shutil
import sys
from pathlib import Path

try:
    from PIL import Image
except ImportError:
    sys.exit("需要 Pillow：python -m pip install Pillow")

try:
    import numpy as np
except ImportError:
    sys.exit("需要 numpy：python -m pip install numpy")

ROOT = Path(__file__).resolve().parent.parent
TEX_DIR = ROOT / "public" / "model" / "character" / "huohuo.8192"

# 备份放在项目之外，避免把 46 MB 原件重新算进项目体积。
# 确认桌宠显示正常后即可删除：Remove-Item -Recurse -Force ..\_original-textures-8192
BACKUP_DIR = ROOT.parent / "_original-textures-8192"

TILE = 1024  # 分块比较的边长，控制峰值内存


def human(n: int | float) -> str:
    return f"{n / 1024 / 1024:.2f} MB"


def alpha_aware_psnr(a_path: Path, b_path: Path) -> tuple[float, float]:
    """
    比较两张图（b 会被双三次放大到 a 的尺寸），返回 (PSNR dB, 完全透明像素占比)。

    在预乘 alpha 的 RGBA 四通道上计算 MSE：alpha=0 的像素两个乘子都是 0，
    因此不可见区域不会污染结果。
    """
    with Image.open(a_path) as ia:
        w, h = ia.size
        a_img = ia.convert("RGBA")

    with Image.open(b_path) as ib:
        b_img = ib.convert("RGBA").resize((w, h), Image.BICUBIC)

    sq_err = 0.0
    n_vals = 0
    transparent = 0
    total_px = 0

    for y in range(0, h, TILE):
        for x in range(0, w, TILE):
            box = (x, y, min(x + TILE, w), min(y + TILE, h))
            ta = np.asarray(a_img.crop(box), dtype=np.float32)
            tb = np.asarray(b_img.crop(box), dtype=np.float32)

            alpha_a = ta[..., 3:4] / 255.0
            alpha_b = tb[..., 3:4] / 255.0

            # 预乘：透明像素的 RGB 被归零，不再参与误差
            premul_a = ta[..., :3] * alpha_a
            premul_b = tb[..., :3] * alpha_b

            # 四通道比较：预乘 RGB + alpha 本身
            diff = np.concatenate([premul_a - premul_b, alpha_a - alpha_b], axis=-1)
            sq_err += float((diff * diff).sum())
            n_vals += diff.size

            transparent += int((ta[..., 3] == 0).sum())
            total_px += ta.shape[0] * ta.shape[1]

    mse = sq_err / n_vals if n_vals else 0.0
    psnr = float("inf") if mse == 0 else 10 * math.log10((255.0**2) / mse)
    return psnr, (transparent / total_px if total_px else 0.0)


def cmd_downscale(args: argparse.Namespace) -> int:
    if not TEX_DIR.is_dir():
        sys.exit(f"找不到纹理目录：{TEX_DIR}")

    targets = sorted(TEX_DIR.glob("texture_*.png"))
    if not targets:
        sys.exit(f"{TEX_DIR} 下没有 texture_*.png")

    if not args.dry_run and not args.no_backup:
        BACKUP_DIR.mkdir(parents=True, exist_ok=True)
        print(f"原件备份到：{BACKUP_DIR}\n")

    before_total = after_total = 0

    for path in targets:
        before = path.stat().st_size
        before_total += before

        with Image.open(path) as im:
            mode, size = im.mode, im.size
            if size[0] != size[1] or size[0] <= args.size:
                print(f"  跳过 {path.name} ({mode} {size[0]}x{size[1]})")
                after_total += before
                continue

            print(f"{path.name}: {mode} {size[0]}x{size[1]}  {human(before)}")

            if args.dry_run:
                after_total += before
                continue

            if not args.no_backup:
                backup_path = BACKUP_DIR / path.name
                if not backup_path.exists():
                    shutil.copy2(path, backup_path)

            small = im.convert("RGBA").resize((args.size, args.size), Image.LANCZOS)
            small.save(path, "PNG", optimize=True, compress_level=9)

        after = path.stat().st_size
        after_total += after
        print(
            f"  -> {args.size}x{args.size}  {human(after)}"
            f"  (省 {100 * (1 - after / before):.1f}%)"
        )

    print(f"\n合计: {human(before_total)} -> {human(after_total)}")
    if not args.dry_run and before_total:
        saved = before_total - after_total
        print(f"节省: {human(saved)} ({100 * saved / before_total:.1f}%)")
        print("\n提示：用 --verify 可量化降采样后的可见损失。")
    return 0


def display_aware_psnr(a_path: Path, b_path: Path, display: int) -> tuple[float, float]:
    """
    把两张纹理都缩放到屏幕实际显示尺寸后再比较。

    这是判断"用户到底看不看得出差别"的关键指标：桌宠主窗口 1200x1500 px，
    纹理无论多大，最终都会被采样到约这个像素量。因此若在 display 尺寸下
    两者几乎一致，则原始分辨率的差异对观感没有影响。

    返回 (PSNR dB, 参与比较的不透明像素占比)。
    """
    a_img = Image.open(a_path).convert("RGBA").resize((display, display), Image.LANCZOS)
    b_img = Image.open(b_path).convert("RGBA").resize((display, display), Image.LANCZOS)

    a = np.asarray(a_img, dtype=np.float32)
    b = np.asarray(b_img, dtype=np.float32)

    alpha_a = a[..., 3:4] / 255.0
    alpha_b = b[..., 3:4] / 255.0

    diff = np.concatenate(
        [a[..., :3] * alpha_a - b[..., :3] * alpha_b, alpha_a - alpha_b], axis=-1
    )

    # 只在肉眼可见（基本不透明）的像素上比较，避免透明边缘的插值噪声主导结果
    visible = np.broadcast_to(a[..., 3:4] > 32, diff.shape)
    n = int(visible.sum())
    if n == 0:
        return float("inf"), 0.0

    mse = float((diff[visible] ** 2).sum()) / n
    psnr = float("inf") if mse == 0 else 10 * math.log10((255.0**2) / mse)
    return psnr, n / diff.size


def cmd_verify(args: argparse.Namespace) -> int:
    """对比备份原件与当前文件，量化肉眼可见的损失。"""
    if not BACKUP_DIR.is_dir():
        sys.exit(f"找不到备份目录：{BACKUP_DIR}\n（原件已被覆盖，无法校验）")

    print(f"{'纹理':<16}{'原始分辨率':>12}{'显示尺寸':>12}   评价")
    print("-" * 62)

    worst_native = float("inf")
    worst_display = float("inf")

    for cur in sorted(TEX_DIR.glob("texture_*.png")):
        orig = BACKUP_DIR / cur.name
        if not orig.exists():
            print(f"{cur.name:<16}{'—':>12}   备份缺失")
            continue

        native, transparent = alpha_aware_psnr(orig, cur)
        shown, _ = display_aware_psnr(orig, cur, args.display_size)
        worst_native = min(worst_native, native)
        worst_display = min(worst_display, shown)

        if shown >= 45:
            verdict = "显示尺寸下不可分辨"
        elif shown >= 40:
            verdict = "显示尺寸下几乎不可分辨"
        else:
            verdict = "显示尺寸下仍可见，建议提高分辨率"

        print(f"{cur.name:<16}{native:>10.1f}dB{shown:>10.1f}dB   {verdict}")

    print("-" * 62)
    print(f"透明像素占比参考：见下方；显示尺寸取 {args.display_size}px")
    print(f"最差 PSNR —— 原始分辨率: {worst_native:.1f} dB / 显示尺寸: {worst_display:.1f} dB")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="Live2D 纹理降采样与质量校验")
    ap.add_argument("--size", type=int, default=2048, help="目标边长，默认 2048")
    ap.add_argument("--dry-run", action="store_true", help="只报告，不写文件")
    ap.add_argument("--no-backup", action="store_true", help="不备份原件（危险）")
    ap.add_argument("--verify", action="store_true", help="对比备份，量化降采样损失")
    ap.add_argument(
        "--display-size",
        type=int,
        default=1500,
        help="屏幕显示尺寸(px)，用于 --verify；默认 1500 对应桌宠窗口高度",
    )
    args = ap.parse_args()

    if args.verify:
        return cmd_verify(args)
    return cmd_downscale(args)


if __name__ == "__main__":
    raise SystemExit(main())
