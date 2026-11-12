"""从现有品牌 ico 生成 Tauri 2 需要的 PNG 图标（纯标准库，无需 Qt/Pillow）。

用法：``.\\.venv\\Scripts\\python.exe tools\\make_tauri_icons.py``
输出：``src-tauri/icons/{32x32,128x128,128x128@2x,icon}.png`` + ``icon.ico``。
"""

from __future__ import annotations

import shutil
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "assets" / "padlend.ico"
OUT = ROOT / "src-tauri" / "icons"


def _png_chunk(tag: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


def write_png(path: Path, width: int, height: int, rgba: bytes) -> None:
    """把 RGBA 像素写成 PNG（color type 6，8 bit，无交错）。"""
    stride = width * 4
    raw = b"".join(b"\x00" + rgba[y * stride : (y + 1) * stride] for y in range(height))
    payload = (
        b"\x89PNG\r\n\x1a\n"
        + _png_chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + _png_chunk(b"IDAT", zlib.compress(raw, 9))
        + _png_chunk(b"IEND", b"")
    )
    path.write_bytes(payload)


def dib_to_rgba(blob: bytes) -> tuple[int, int, bytes]:
    """把 ICO 里的 32bpp BITMAPINFOHEADER（含 XOR/AND 掩码）转成 RGBA。"""
    (_size, width, doubled_height, _planes, bit_count, compression, _image_size, *_) = (
        struct.unpack_from("<IiiHHIIiiII", blob, 0)
    )
    if bit_count != 32 or compression != 0:
        raise ValueError(f"不支持的 ICO 位图格式：bpp={bit_count} compression={compression}")
    height = doubled_height // 2
    stride = width * 4
    xor = blob[40 : 40 + stride * height]
    if len(xor) < stride * height:
        raise ValueError("ICO 位图数据不完整")
    rows: list[bytes] = []
    for y in range(height):
        # DIB 自下而上存储，且通道顺序为 BGRA。
        src = xor[(height - 1 - y) * stride : (height - 1 - y + 1) * stride]
        row = bytearray(stride)
        for x in range(width):
            b, g, r, a = src[x * 4 : x * 4 + 4]
            row[x * 4 : x * 4 + 4] = bytes((r, g, b, a))
        rows.append(bytes(row))
    return width, height, b"".join(rows)


def read_ico(path: Path) -> list[tuple[int, bytes]]:
    data = path.read_bytes()
    _reserved, image_type, count = struct.unpack_from("<HHH", data, 0)
    if image_type != 1:
        raise ValueError("不是 ICO 文件")
    entries: list[tuple[int, bytes]] = []
    for index in range(count):
        width, height, _colors, _reserved2, _planes, _bpp, size, offset = struct.unpack_from(
            "<BBBBHHII", data, 6 + 16 * index
        )
        blob = data[offset : offset + size]
        entries.append((width or 256, blob))
    return entries


def main() -> None:
    if not SOURCE.is_file():
        raise SystemExit(f"找不到品牌图标：{SOURCE}")
    OUT.mkdir(parents=True, exist_ok=True)
    entries = read_ico(SOURCE)
    png_largest = max(
        ((size, blob) for size, blob in entries if blob[:8] == b"\x89PNG\r\n\x1a\n"),
        key=lambda item: item[0],
        default=None,
    )

    for name, target in (("32x32.png", 32), ("128x128.png", 128)):
        match = next(((size, blob) for size, blob in entries if size == target), None)
        if match is None or match[1][:8] == b"\x89PNG\r\n\x1a\n":
            # 该尺寸只有 PNG 条目或缺失时用最接近的条目
            match = min(entries, key=lambda item: abs(item[0] - target))
        _, blob = match
        if blob[:8] == b"\x89PNG\r\n\x1a\n":
            (OUT / name).write_bytes(blob)
        else:
            width, height, rgba = dib_to_rgba(blob)
            write_png(OUT / name, width, height, rgba)
        print(f"generated {name}")

    # 256 与 512 用 ico 里最大的 PNG 条目；Windows 实际使用 icon.ico。
    if png_largest is None:
        size, blob = max(entries, key=lambda item: item[0])
        width, height, rgba = dib_to_rgba(blob)
        write_png(OUT / "128x128@2x.png", width, height, rgba)
        (OUT / "icon.png").write_bytes((OUT / "128x128@2x.png").read_bytes())
    else:
        for name in ("128x128@2x.png", "icon.png"):
            (OUT / name).write_bytes(png_largest[1])
            print(f"generated {name} ({png_largest[0]}px source)")

    shutil.copy2(SOURCE, OUT / "icon.ico")
    print("copied icon.ico")


if __name__ == "__main__":
    main()
