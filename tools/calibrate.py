"""指针标定（设计文档 6.4.4 / H2）。

流程（全部可重复、退出时一定还原）：

1. ``settings put system pointer_location 1`` 打开指针坐标覆盖层；
2. 起一条正常的 TabletSession（内部会把 pointer_speed 归零，退出时还原）；
3. 截一张 before.png；
4. 连发 ``--send`` 个 ``dx=+1`` 的鼠标报告；
5. 截一张 after.png；
6. 关闭覆盖层、停止会话。

因为不引入 OCR 依赖，px/count 由你从两张截图上的 X 读数差算出，再用
``--measured-x`` 回填即可换算。示例::

    python tools\\calibrate.py --send 1000
    # 看 .calibration/before.png 与 after.png 顶部覆盖层的 X 读数
    python tools\\calibrate.py --send 1000 --measured-x 1312

退出码：0 成功；2 没有设备 / 无法启动（打印提示，不抛栈）。
"""

from __future__ import annotations

import argparse
import contextlib
import sys
import time
from pathlib import Path

_ROOT = Path(__file__).resolve().parent.parent
if str(_ROOT) not in sys.path:
    sys.path.insert(0, str(_ROOT))

import constants as C  # noqa: E402
from core.errors import PadLendError  # noqa: E402
from tablet.adb import Adb  # noqa: E402
from tablet.session import TabletSession  # noqa: E402

BASELINE_XSCALE = 1.312  # DESIGN 3.1 实测基线
POINTER_LOCATION_KEY = "pointer_location"


def _stdout() -> None:
    """附录 D：统一按 UTF-8 输出（重定向时 cp936 会把中文写成乱码）。"""
    for stream in (sys.stdout, sys.stderr):
        with contextlib.suppress(AttributeError, ValueError):  # pragma: no cover - 老终端
            stream.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[union-attr]


def _read_setting(adb: Adb, key: str) -> str:
    rc, out = adb.shell_result("settings", "get", "system", key)
    return out.strip() if rc == 0 else ""


def _set_setting(adb: Adb, key: str, value: str) -> None:
    adb.shell("settings", "put", "system", key, value)


def _screencap(adb: Adb, path: Path) -> bool:
    """`screencap -p` 直接取 PNG 字节存盘。"""
    try:
        data = adb.exec_out("screencap", "-p")
    except PadLendError:
        return False
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return len(data) > 0


def _report(send: int, measured_x: int, measured_y: int) -> int:
    print()
    print("标定结果")
    print("-" * 60)
    print(f"  发送计数        : {send} 个 dx=+1 / dy=+1 报告")
    if measured_x > 0:
        ratio_x = measured_x / send
        print(f"  X 位移          : {measured_x} px → {ratio_x:.4f} px/count")
        delta = abs(ratio_x - BASELINE_XSCALE) / BASELINE_XSCALE * 100
        verdict = "一致" if delta <= 5 else "偏差 >5%"
        print(f"  对基线 {BASELINE_XSCALE} : 偏差 {delta:.2f}% —— {verdict}")
        if delta <= 5:
            print("  结论：gain 可通过 XScale 直接换算（H2 按设计成立）。")
        else:
            print("  结论：需把实测值写进 config.motion.gain，并复核 pointer_speed=0。")
    if measured_y > 0:
        print(f"  Y 位移          : {measured_y} px → {measured_y / send:.4f} px/count")
    if measured_x <= 0 and measured_y <= 0:
        print("  尚未回填读数：看 .calibration/before.png 与 after.png 顶部覆盖层的 X/Y，")
        print("  然后用 --measured-x <差值> 再跑一次（不会重复发送时可加 --no-move）。")
    return 0


def run(args: argparse.Namespace) -> int:
    out_dir = Path(args.out).expanduser()
    try:
        adb = Adb(serial=args.serial or None, adb_path=args.adb)
        adb.version()
        entry = adb.select_device(args.serial or None)
        bound = adb.with_serial(entry.serial)
    except PadLendError as exc:
        print(f"[{exc.code.value}] {exc.user_message}")
        print("提示：先运行 tools/probe.py 确认设备可见。")
        return 2

    print(f"设备 {entry.serial}（{entry.model or 'unknown'}）")
    if args.no_move and (args.measured_x <= 0 and args.measured_y <= 0):
        print("--no-move 需要同时给 --measured-x 或 --measured-y。")
        return 2

    old_location = _read_setting(bound, POINTER_LOCATION_KEY)
    session: TabletSession | None = None
    try:
        if not args.no_move:
            _set_setting(bound, POINTER_LOCATION_KEY, "1")
            print("已打开 pointer_location 覆盖层。")

            session = TabletSession(adb=bound, port=args.port or None)
            print("启动会话（会临时把 pointer_speed 归零）…")
            facts = session.start()
            print(f"  分辨率 {facts.width}x{facts.height}，XScale {facts.xscale:.3f}")

            time.sleep(1.0)
            before = out_dir / "before.png"
            if _screencap(bound, before):
                print(f"  截图 1/2: {before}")
            else:
                print("  [!] 截图失败（继续，不影响发送）")

            print(f"发送 {args.send} 个 dx=+1 报告（间隔 {args.step_delay_ms} ms）…")
            for index in range(args.send):
                session.send_mouse(0, 1, 0, 0)
                if args.step_delay_ms > 0:
                    time.sleep(args.step_delay_ms / 1000.0)
                if (index + 1) % 250 == 0:
                    print(f"  ... {index + 1}/{args.send}")
            time.sleep(1.0)

            after = out_dir / "after.png"
            if _screencap(bound, after):
                print(f"  截图 2/2: {after}")
            else:
                print("  [!] 截图失败")
        else:
            print("--no-move：跳过发送，只做换算。")

        return _report(args.send, args.measured_x, args.measured_y)
    except PadLendError as exc:
        print(f"[{exc.code.value}] {exc.user_message}")
        return 2
    except Exception as exc:  # pragma: no cover - 诊断脚本不抛栈
        print(f"[E900] 未预期错误：{type(exc).__name__}: {exc}")
        return 2
    finally:
        # 还原：覆盖层、会话（含 pointer_speed / keep screen on / 端口转发）
        if session is not None:
            try:
                session.stop()
            except PadLendError as exc:  # pragma: no cover - 清理尽力而为
                print(f"[!] 停止会话时出错：{exc.user_message}")
        try:
            _set_setting(bound, POINTER_LOCATION_KEY, old_location or "0")
            print(f"已还原 pointer_location={old_location or '0'}。")
        except PadLendError:  # pragma: no cover
            print("[!] 还原 pointer_location 失败，请手动执行：")
            print("    settings put system pointer_location 0")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="标定 UHID 鼠标 px/count（DESIGN 6.4.4）")
    parser.add_argument("--serial", default="", help="设备序列号（默认自动选择）")
    parser.add_argument("--adb", default="", help="adb.exe 路径（默认自动解析）")
    parser.add_argument(
        "--port", type=int, default=0, help=f"scrcpy 端口（默认 {C.DEFAULT_SERVER_PORT}）"
    )
    parser.add_argument("--send", type=int, default=1000, help="发送的报告个数（默认 1000）")
    parser.add_argument("--step-delay-ms", type=int, default=10, help="每个报告间隔毫秒（默认 10）")
    parser.add_argument("--out", default=".calibration", help="截图输出目录（默认 .calibration）")
    parser.add_argument("--measured-x", type=int, default=0, help="两次截图 X 读数差（像素）")
    parser.add_argument("--measured-y", type=int, default=0, help="两次截图 Y 读数差（像素）")
    parser.add_argument("--no-move", action="store_true", help="只换算，不重新发送/截图")
    return parser


def main(argv: list[str] | None = None) -> int:
    _stdout()
    args = build_parser().parse_args(argv)
    try:
        return run(args)
    except KeyboardInterrupt:
        print("\n已取消。")
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
