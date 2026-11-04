"""把运行时的品牌图标渲染成 assets/padlend.ico（打包用的文件图标）。

窗口/任务栏图标走 ``ui.icons.brand_icon()``，exe 图标走 ``assets/padlend.ico``；
两边必须是同一份画法，否则资源管理器里看到的还是旧图标。

Windows 资源管理器读小尺寸图标时对「全 PNG 条的 ICO」支持不完整（只有 256 是
官方推荐的 PNG 档），所以这里 16…128 用传统的 BMP/DIB 条目（BITMAPINFOHEADER +
BGRA 行 + AND 掩码），只有 256 用 PNG。

用法::

    .\\.venv\\Scripts\\python.exe tools/make_icon.py
"""

from __future__ import annotations

import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

DIB_SIZES = (16, 20, 24, 32, 40, 48, 64, 96, 128)
PNG_SIZES = (256,)


def _pixmap(size: int):
    from PySide6.QtGui import QGuiApplication

    from ui.icons import brand_pixmap

    app = QGuiApplication.instance()
    own = app is None
    if own:
        app = QGuiApplication([sys.argv[0]])
    try:
        return brand_pixmap(size)
    finally:
        if own:
            app.quit()


def _dib_entry(size: int) -> bytes:
    """32 位 BMP 条目：BITMAPINFOHEADER(height×2) + 自下而上的 BGRA + AND 掩码。"""
    image = _pixmap(size).toImage().convertToFormat(_argb32())
    if image.width() != size or image.height() != size:
        image = image.scaled(size, size)

    header = struct.pack(
        "<IiiHHIIiiII",
        40,  # biSize
        size,
        size * 2,  # XOR + AND 两层
        1,  # biPlanes
        32,  # biBitCount
        0,  # BI_RGB
        size * size * 4,
        0,
        0,
        0,
        0,
    )
    rows = []
    for y in range(size - 1, -1, -1):  # BMP 自下而上
        line = image.constScanLine(y)
        rows.append(bytes(line)[: size * 4])
    xor = b"".join(rows)
    mask_row = ((size + 31) // 32) * 4  # 1bpp，行按 4 字节对齐
    and_mask = b"\x00" * (mask_row * size)  # 全 0 = 不透明
    return header + xor + and_mask


def _argb32():
    from PySide6.QtGui import QImage

    return QImage.Format.Format_ARGB32


def _png_entry(size: int) -> bytes:
    from PySide6.QtCore import QBuffer, QByteArray

    pixmap = _pixmap(size)
    data = QByteArray()
    buffer = QBuffer(data)
    buffer.open(QBuffer.OpenModeFlag.WriteOnly)
    if not pixmap.save(buffer, "PNG"):
        raise RuntimeError(f"渲染 {size}x{size} 图标失败")
    buffer.close()
    return bytes(data)


def build_ico(images: list[tuple[int, bytes]]) -> bytes:
    """images: [(size, payload)]，payload 是 DIB 或 PNG 原始字节。"""
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries = b""
    blobs = b""
    for size, data in images:
        byte = 0 if size >= 256 else size  # ICO 里 256 记作 0
        entries += struct.pack("<BBBBHHII", byte, byte, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
        blobs += data
    return header + entries + blobs


def main() -> int:
    images = [(size, _dib_entry(size)) for size in DIB_SIZES]
    images += [(size, _png_entry(size)) for size in PNG_SIZES]
    target = ROOT / "assets" / "padlend.ico"
    target.parent.mkdir(parents=True, exist_ok=True)
    payload = build_ico(images)
    target.write_bytes(payload)
    print(f"wrote {target} ({len(payload)} bytes)")
    print("sizes:", ", ".join(f"{s}({'png' if s >= 256 else 'dib'})" for s, _ in images))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
