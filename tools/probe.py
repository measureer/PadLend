"""设备事实探针（设计文档 S0-3 / 附录 C.2）。

用法::

    .\\.venv\\Scripts\\python.exe tools\\probe.py
    .\\.venv\\Scripts\\python.exe tools\\probe.py --serial 192.168.68.10:46511
    .\\.venv\\Scripts\\python.exe tools\\probe.py --json

退出码：
* ``0`` 读到设备事实；
* ``2`` 没有 adb / 没有设备 / 未授权 —— 只打印可操作提示，**不抛栈**。

这是诊断脚本，按 7.3 允许使用 ``print``。
"""

from __future__ import annotations

import argparse
import contextlib
import json
import sys
from pathlib import Path

_ROOT = Path(__file__).resolve().parent.parent
if str(_ROOT) not in sys.path:
    sys.path.insert(0, str(_ROOT))

from core.errors import ErrorCode, PadLendError  # noqa: E402
from tablet.adb import Adb, DeviceEntry  # noqa: E402
from tablet.session import (  # noqa: E402
    DeviceFacts,
    parse_xscale,
    probe_diagnostics,
    read_facts,
)

NO_DEVICE_HINT = """\
没有发现可用的平板。请依次检查：
  1) USB 线已连接，平板已解锁；
  2) 开发者选项 → USB 调试 已打开；
  3) 平板弹出的「允许 USB 调试吗？」点了「允许」（可勾选「一律允许」）；
  4) 无线调试时先执行：adb connect <ip>:<port>
  5) 运行 `.venv\\Scripts\\python.exe tools\\probe.py` 复查。
"""


def _configure_stdout() -> None:
    """附录 D：统一按 UTF-8 输出（重定向时 cp936 会把中文写成乱码）。

    控制台（TTY）下 Python 走 WriteConsoleW，Unicode 本来就正常；
    这里的 reconfigure 只影响管道/重定向的字节编码。
    """
    for stream in (sys.stdout, sys.stderr):
        with contextlib.suppress(AttributeError, ValueError):  # pragma: no cover - 老终端
            stream.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[union-attr]


def _print_header(title: str) -> None:
    print()
    print(title)
    print("-" * 60)


def _format_devices(entries: list[DeviceEntry]) -> str:
    if not entries:
        return "  （空）"
    lines = []
    for entry in entries:
        kind = "USB" if entry.is_usb else "Wi-Fi"
        model = entry.model or entry.product or "-"
        lines.append(f"  {entry.serial:<28} {entry.state:<14} {kind:<6} {model}")
    return "\n".join(lines)


def _print_facts(facts: DeviceFacts) -> None:
    ratio = facts.width / facts.height if facts.height else 0.0
    print(f"  型号            : {facts.model}")
    print(f"  序列号          : {facts.serial}")
    print(f"  逻辑分辨率      : {facts.width} x {facts.height}（宽高比 {ratio:.4f}）")
    print(f"  朝向 orientation: {facts.orientation}  (0=0° 1=90° 2=180° 3=270°)")
    print(f"  XScale          : {facts.xscale:.3f}")
    print(f"  pointer_speed   : {facts.pointer_speed}（启动时会归零）")


def _print_diagnostics(diag: dict[str, str]) -> None:
    density = diag.get("density", "")
    if density:
        print(f"  密度            : {density.splitlines()[0]}")
    print()
    print("  UHID 与缩放的原始输出：")
    for key, value in diag.items():
        if key == "density":
            continue
        first = value.splitlines()[0] if value.strip() else "<空>"
        print(f"    {key:<12}: {first}")

    uhid_open = diag.get("uhid_open", "")
    groups = diag.get("uhid_groups", "")
    print()
    if "OPEN_OK" in uhid_open:
        print("  [OK]   /dev/uhid 可写（F1 成立）")
    else:
        print("  [FAIL] /dev/uhid 不可写 —— 无法创建虚拟鼠标/键盘（E102）")
    if "uhid" in groups:
        print("  [OK]   shell 用户在 uhid 组（3011）")
    else:
        print("  [WARN] shell 用户不在 uhid 组，执行 `adb shell id` 复核")


def _print_scale_check(diag: dict[str, str]) -> None:
    """附录 C.2：把 XScale 打出来，方便和 3.1 的 1.312 对照（H2）。"""
    scales = diag.get("input_scales", "")
    print()
    print("  dumpsys input 关键行：")
    for line in scales.splitlines()[:6]:
        print(f"    {line.strip()}")
    if "XScale" in scales:
        value = parse_xscale(scales)
        if abs(value - 1.312) < 0.02:
            hint = "（与基线 1.312 一致）"
        else:
            hint = "（与基线 1.312 不同，A1 前需核实）"
        print(f"  XScale 解析结果 : {value:.3f} {hint}")


def _print_json(entries: list[DeviceEntry], facts: DeviceFacts | None,
                diag: dict[str, str] | None) -> None:
    payload = {
        "adb_version": None,
        "devices": [
            {
                "serial": e.serial,
                "state": e.state,
                "usb": e.is_usb,
                "model": e.model,
                "product": e.product,
                "device": e.device,
            }
            for e in entries
        ],
        "facts": None if facts is None else {
            "serial": facts.serial,
            "model": facts.model,
            "width": facts.width,
            "height": facts.height,
            "orientation": facts.orientation,
            "xscale": facts.xscale,
            "pointer_speed": facts.pointer_speed,
        },
        "diagnostics": diag,
    }
    print(json.dumps(payload, ensure_ascii=False, indent=2))


def run(args: argparse.Namespace) -> int:
    adb = Adb(serial=args.serial or None, adb_path=args.adb)
    try:
        version = adb.version()
    except PadLendError as exc:
        print(f"[E100] {exc.user_message}")
        print("提示：把 platform-tools 解压到 vendor/adb/，")
        print("      或设置环境变量 PADLEND_ADB 指向 adb.exe。")
        return 2

    _print_header(f"PadLend probe — adb {version or '(版本未知)'}")
    try:
        entries = adb.devices()
    except PadLendError as exc:
        print(f"[!] 读取设备列表失败：{exc.user_message}")
        return 2

    print("设备列表（adb devices -l）：")
    print(_format_devices(entries))

    if args.json and not entries:
        _print_json(entries, None, None)
        return 2

    try:
        entry = adb.select_device(args.serial or None)
    except PadLendError as exc:
        print()
        print(f"[{exc.code.value}] {exc.user_message}")
        if exc.code is ErrorCode.DEVICE_OFFLINE:
            print(NO_DEVICE_HINT)
        return 2

    bound = adb.with_serial(entry.serial)
    try:
        facts = read_facts(bound)
        diag = probe_diagnostics(bound)
    except PadLendError as exc:
        print()
        print(f"[{exc.code.value}] 读取设备事实失败：{exc.user_message}")
        print("提示：确认平板已解锁、USB 调试已授权，然后重试。")
        return 2

    if args.json:
        _print_json(entries, facts, diag)
        return 0

    _print_header(f"设备事实 — {entry.serial}")
    _print_facts(facts)
    _print_diagnostics(diag)
    _print_scale_check(diag)

    print()
    print("基线对照（DESIGN 3.1）：MagicPad 3 / CGA-W00 期望 3200x2136、420dpi、XScale 1.312、"
          "pointer_speed -3。")
    print("下一步：python tools/calibrate.py --send 1000  （标定 px/count，H2）")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="读取平板设备事实（无设备时退出码 2）")
    parser.add_argument("--serial", default="", help="指定设备序列号（默认按 6.1.1 自动选择）")
    parser.add_argument("--adb", default="", help="adb.exe 路径（默认自动解析）")
    parser.add_argument("--json", action="store_true", help="输出 JSON（便于脚本消费）")
    return parser


def main(argv: list[str] | None = None) -> int:
    _configure_stdout()
    args = build_parser().parse_args(argv)
    try:
        return run(args)
    except KeyboardInterrupt:
        print("\n已取消。")
        return 130
    except PadLendError as exc:  # 兜底：任何未预期业务错误都不抛栈
        print(f"[{exc.code.value}] {exc.user_message}")
        if exc.code is ErrorCode.ADB_NOT_FOUND:
            print(NO_DEVICE_HINT)
        return 2
    except Exception as exc:  # pragma: no cover - 诊断脚本绝不抛栈
        print(f"[E900] 未预期错误：{type(exc).__name__}: {exc}")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
