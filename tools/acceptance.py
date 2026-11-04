"""端到端自动验收：不需要平板，也不需要人手点。

把「接管 → 单击 → 连点 → 双击 → 拖动 → 交还」这条真机链路在本机跑一遍，
用 ``--simulate`` 的假平板 + 真实的 Win32 遮罩 / ClipCursor：

* 断言接管后 ClipCursor 被钉在屏幕边缘的 1px 条上、遮罩窗口存在且吞掉
  ``WM_SETCURSOR``（遮罩藏光标的唯一机制）；
* 把鼠标事件喂进 ``Router.on_raw``（与 Raw Input 线程同一个入口），断言平板端
  收到按下+抬起、**点击不会把状态机踢出 CAPTURED**、拖动位移连续；
* 交还后断言 ClipCursor 解除、遮罩销毁、状态回到 IDLE。

光标是否"看得见"用 ``GetCursorInfo`` 观测；某些环境（无人值守的远程会话、
虚拟显示器）光标本来就不会动，这时该项标 ``SKIP`` 而不是 FAIL，其余检查照跑。

为什么不用 ``SendInput`` 真点一下：它要求进程处在可交互桌面上。远程/无头会话里
注入的鼠标既不动光标也不产生 Raw Input（实测 SendInput 返回 1 但 ``GetCursorPos``
不变），所以这里直接走事件入口，遮罩/裁剪这些真实系统状态仍然实测。

用法::

    .venv\\Scripts\\python.exe tools/acceptance.py
    .venv\\Scripts\\python.exe tools/acceptance.py -v          # 带日志
    .venv\\Scripts\\python.exe tools/acceptance.py --json out.json

退出码 0 = 全过（SKIP 不算失败）；1 = 有失败项。

注意：运行期间会真实隐藏并锁定鼠标约 2 秒。
"""

from __future__ import annotations

import argparse
import ctypes
import json
import logging
import sys
import time
from ctypes import wintypes
from pathlib import Path

_ROOT = Path(__file__).resolve().parent.parent
if str(_ROOT) not in sys.path:
    sys.path.insert(0, str(_ROOT))

from config import load_config  # noqa: E402
from core.events import RawDelta  # noqa: E402
from core.geometry import Align, Edge, Placement  # noqa: E402
from core.state import State  # noqa: E402
from main import Runtime  # noqa: E402

user32 = ctypes.WinDLL("user32", use_last_error=True)

CURSOR_SHOWING = 0x00000001
WM_SETCURSOR = 0x0020
PM_REMOVE = 0x0001
BTN_LEFT = 0x01


class CURSORINFO(ctypes.Structure):
    _fields_ = [
        ("cbSize", wintypes.DWORD),
        ("flags", wintypes.DWORD),
        ("hCursor", wintypes.HANDLE),
        ("ptScreenPos", wintypes.POINT),
    ]


def cursor_visible() -> bool | None:
    info = CURSORINFO()
    info.cbSize = ctypes.sizeof(CURSORINFO)
    if not user32.GetCursorInfo(ctypes.byref(info)):
        return None
    return bool(info.flags & CURSOR_SHOWING)


def cursor_pos() -> tuple[int, int]:
    pt = wintypes.POINT()
    user32.GetCursorPos(ctypes.byref(pt))
    return (pt.x, pt.y)


def clip_rect() -> tuple[int, int, int, int] | None:
    rect = wintypes.RECT()
    if not user32.GetClipCursor(ctypes.byref(rect)):
        return None
    return (rect.left, rect.top, rect.right, rect.bottom)


def cursor_observable() -> bool:
    """光标能不能被观测：挪一下看坐标是否变化。

    远程/无头会话里光标是"冻结"的，``GetCursorInfo`` 恒为可见，
    这种环境下光标可见性无从判断，只能 SKIP。
    """
    start = cursor_pos()
    user32.SetCursorPos(start[0] + 2, start[1] + 2)
    time.sleep(0.05)
    moved = cursor_pos()
    user32.SetCursorPos(*start)
    return moved != start


def pump(seconds: float = 0.25) -> None:
    """抽干本线程消息（遮罩窗口建在本线程，WM_SETCURSOR 要它处理）。"""
    deadline = time.monotonic() + seconds
    msg = wintypes.MSG()
    while time.monotonic() < deadline:
        while user32.PeekMessageW(ctypes.byref(msg), None, 0, 0, PM_REMOVE):
            user32.TranslateMessage(ctypes.byref(msg))
            user32.DispatchMessageW(ctypes.byref(msg))
        time.sleep(0.005)


def mask_swallows_setcursor(mask) -> tuple[bool, str]:  # noqa: ANN001
    """给每个遮罩窗口发 WM_SETCURSOR，确认窗口过程返回 1（=不让系统画光标）。"""
    hwnds = list(getattr(mask, "_windows", []) or [])
    if not hwnds:
        return False, "没有遮罩窗口"
    results = [
        int(user32.SendMessageW(ctypes.c_void_p(int(hwnd)), WM_SETCURSOR, 0, 0)) for hwnd in hwnds
    ]
    return all(r == 1 for r in results), f"hwnds={len(hwnds)} result={results}"


def default_placement(cfg, desktop):  # noqa: ANN001, ANN201
    """按配置的吸附边挑一段外轮廓（GUI 里由设置面板做同样的事）。"""
    segments = list(desktop.exterior_segments())
    if not segments:
        return None
    try:
        edge = Edge(str(cfg.placement.edge))
    except (ValueError, AttributeError):
        edge = Edge.RIGHT
    monitors = list(desktop.monitors)
    primary = next((m for m in monitors if getattr(m, "primary", False)), None) or (
        monitors[0] if monitors else None
    )
    candidates = [s for s in segments if s.edge is edge]
    if primary is not None:
        same = [s for s in candidates if s.monitor_handle == primary.handle]
        candidates = same or candidates
    best = max(candidates or segments, key=lambda s: s.length)
    try:
        align = Align(str(cfg.placement.align))
    except (ValueError, AttributeError):
        align = Align.PROPORTIONAL
    return Placement(segment=best, offset=float(cfg.placement.offset), align=align)


class Report:
    """记 PASS / FAIL / SKIP，最后汇总。"""

    def __init__(self) -> None:
        self.rows: list[tuple[str, str, str]] = []

    def check(self, ok: bool, name: str, detail: str = "") -> bool:
        return self._add("PASS" if ok else "FAIL", name, detail)

    def skip(self, name: str, detail: str = "") -> None:
        self._add("SKIP", name, detail)

    def _add(self, status: str, name: str, detail: str) -> bool:
        self.rows.append((status, name, detail))
        line = f"[{status}] {name}" + (f" — {detail}" if detail else "")
        print(line, flush=True)
        return status != "FAIL"

    @property
    def failed(self) -> list[tuple[str, str, str]]:
        return [row for row in self.rows if row[0] == "FAIL"]

    @property
    def skipped(self) -> list[tuple[str, str, str]]:
        return [row for row in self.rows if row[0] == "SKIP"]


def wait_for(predicate, timeout: float = 6.0, step: float = 0.05) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(step)
    return False


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="PadLend 端到端自动验收（假平板）")
    parser.add_argument(
        "--config",
        default=str(_ROOT / "build" / "acceptance" / "config.yaml"),
        help="验收用配置（默认建在 build 下，不碰真实配置）",
    )
    parser.add_argument("--timeout", type=float, default=15.0, help="每一步的等待上限（秒）")
    parser.add_argument("--json", default=None, help="把结果写成 JSON 供 CI / 汇报引用")
    parser.add_argument("-v", "--verbose", action="store_true", help="打印状态迁移与日志")
    args = parser.parse_args(argv)

    if args.verbose:
        logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s %(message)s")

    config_path = Path(args.config)
    config_path.parent.mkdir(parents=True, exist_ok=True)
    cfg = load_config(config_path)
    cfg.ui.wizard_done = True

    report = Report()
    runtime = Runtime(cfg, simulate=True)
    sim = runtime.session
    router = runtime.router
    mask = runtime.mask

    transitions: list[tuple[str, str, str]] = []
    router.on_state_changed = lambda old, new, reason: transitions.append(
        (str(old), str(new), str(reason))
    )

    def feed(buttons: int = 0, dx: int = 0, dy: int = 0, wheel: int = 0) -> None:
        router.on_raw(RawDelta(dx=dx, dy=dy, buttons=buttons, wheel=wheel, ts=time.time()))

    def click() -> None:
        feed(buttons=BTN_LEFT)
        pump(0.05)
        feed(buttons=0)
        pump(0.05)

    try:
        sim.start()
        runtime.start()
        placement = default_placement(cfg, runtime.desktop)
        if placement is not None:
            router.set_placement(placement)
        if not report.check(
            wait_for(lambda: router.placement is not None, args.timeout),
            "拿到 placement（平板已接入）",
            f"state={router.state}",
        ):
            return _finish(report, args)

        observable = cursor_observable()
        if observable:
            report.check(cursor_visible() is True, "接管前光标可见", f"visible={cursor_visible()}")
        else:
            report.skip("接管前光标可见", "本机光标不可观测（远程/无头会话）")

        sim.clear()
        transitions.clear()
        router.toggle_manual()
        if not report.check(
            wait_for(lambda: router.state is State.CAPTURED, args.timeout),
            "手动接管进入 CAPTURED",
            f"state={router.state}",
        ):
            return _finish(report, args)

        pump(0.3)
        report.check(len(mask._windows) > 0, "遮罩窗口已创建", f"windows={len(mask._windows)}")
        swallows, swallow_detail = mask_swallows_setcursor(mask)
        report.check(swallows, "遮罩吞掉 WM_SETCURSOR（藏光标的机制）", swallow_detail)

        clip = clip_rect()
        monitors = tuple(runtime.desktop.monitors)
        inside = any(
            clip is not None
            and m.rect.left <= clip[0]
            and clip[2] <= m.rect.right
            and m.rect.top <= clip[1]
            and clip[3] <= m.rect.bottom
            for m in monitors
        )
        report.check(bool(clip) and inside, "ClipCursor 钉在屏幕边缘条上", f"clip={clip}")
        report.check(
            not any(m.rect.as_tuple() == clip for m in monitors),
            "裁剪区不是整块屏幕（确实锁到了边缘条）",
            f"clip={clip}",
        )

        # -- 单击（用户报的 bug 场景）---------------------------------------
        sim.clear()
        transitions.clear()
        click()
        pump(0.3)
        report.check(sim.clicks >= 1, "平板收到单击（按下+抬起）", f"clicks={sim.clicks}")
        buttons_seen = [r.buttons for r in sim.reports]
        report.check(
            BTN_LEFT in buttons_seen and 0 in buttons_seen,
            "报告里既有按下也有抬起",
            f"buttons={buttons_seen[:8]}",
        )
        report.check(
            router.state is State.CAPTURED,
            "单击不会把状态机踢出 CAPTURED",
            f"state={router.state} transitions={transitions}",
        )
        report.check(
            len(mask._windows) > 0 and clip_rect() == clip,
            "单击后遮罩与裁剪区都还在",
            f"clip={clip_rect()} windows={len(mask._windows)}",
        )
        if observable:
            state = cursor_visible()
            report.check(state is False, "单击后光标仍然隐藏", f"visible={state}")
        else:
            report.skip("单击后光标仍然隐藏", "本机光标不可观测")

        # 现场自检：点击时分清"遮罩被拆 / 锁被收回 / 光标被别处显示回来"。
        diagnose = mask.diagnose()
        report.check(
            "windows=0" not in diagnose and "active=True" in diagnose,
            "单击后遮罩自检仍然是接管态",
            diagnose,
        )

        # -- 连点 / 双击 -----------------------------------------------------
        sim.clear()
        transitions.clear()
        for _ in range(3):
            click()
        pump(0.2)
        report.check(sim.clicks >= 3, "连点 3 次全部送达", f"clicks={sim.clicks}")
        report.check(router.state is State.CAPTURED, "连点后仍在 CAPTURED", f"state={router.state}")

        sim.clear()
        for _ in range(2):
            click()
            time.sleep(0.05)
        pump(0.2)
        report.check(sim.clicks >= 2, "双击两次按下都送达", f"clicks={sim.clicks}")
        report.check(router.state is State.CAPTURED, "双击后仍在 CAPTURED", f"state={router.state}")

        # -- 拖动 -----------------------------------------------------------
        sim.clear()
        transitions.clear()
        feed(buttons=BTN_LEFT)
        for _ in range(8):
            feed(buttons=BTN_LEFT, dx=6, dy=4)
            pump(0.02)
        feed(buttons=0)
        pump(0.3)
        held = [r for r in sim.reports if r.buttons & BTN_LEFT]
        report.check(len(held) >= 4, "拖动期间持续上报（按住状态）", f"held_reports={len(held)}")
        total = (sum(r.dx for r in sim.reports), sum(r.dy for r in sim.reports))
        report.check(total != (0, 0), "平板指针确实动了", f"dx={total[0]} dy={total[1]}")
        report.check(router.state is State.CAPTURED, "拖动后仍在 CAPTURED", f"state={router.state}")

        # -- RTT 指标（7.4） -------------------------------------------------
        snapshot = router.metrics_snapshot()
        rtt = snapshot.get("rtt_ms")
        report.check(
            rtt is not None and 0.0 <= float(rtt) < 200.0,
            "指标里有真实 RTT 样本（不再是 0/缺省）",
            f"rtt_ms={rtt}",
        )

        # -- 穿屏点一下：按住离开平板要补全松，再进来不能被当成拖拽 ----------
        sim.clear()
        feed(buttons=BTN_LEFT)
        pump(0.05)
        router.toggle_manual()  # 用户按住鼠标直接穿回电脑
        report.check(
            wait_for(lambda: router.state is State.IDLE, args.timeout),
            "按住离开平板后回到 IDLE",
            f"state={router.state}",
        )
        pump(0.2)
        # 心跳也是全零报告，这里只断言"离开之后没有任何带按下的位移"：
        # 也就是说平板不会停留在拖拽状态。
        moves_after = [r for r in sim.reports if r.dx or r.dy]
        report.check(
            all(r.buttons == 0 for r in moves_after),
            "按住离开平板后不再有拖拽位移（键已松开）",
            f"moves={len(moves_after)} pressed={sum(1 for r in moves_after if r.buttons)}",
        )
        sim.clear()
        router.toggle_manual()
        if not report.check(
            wait_for(lambda: router.state is State.CAPTURED, args.timeout),
            "再次穿进平板进入 CAPTURED",
            f"state={router.state}",
        ):
            return _finish(report, args)
        pump(0.3)
        first_reports = sim.reports[:3]
        report.check(
            bool(first_reports) and all(r.buttons == 0 for r in first_reports),
            "重新进入时平板没有残留按下（不会一点就触发按钮）",
            f"buttons={[hex(r.buttons) for r in first_reports]}",
        )
        report.check(
            bool(sim.reports)
            and sim.reports[0].buttons == 0
            and not sim.reports[0].dx
            and not sim.reports[0].dy,
            "进入时先补一条全松（清掉上一次会话遗留的按住）",
            f"first={sim.reports[0] if sim.reports else None}",
        )

        # -- 交还 -----------------------------------------------------------
        router.toggle_manual()
        ok = wait_for(lambda: router.state is State.IDLE, args.timeout)
        report.check(ok, "再次触发回到 IDLE", f"state={router.state}")
        pump(0.4)
        report.check(len(mask._windows) == 0, "交还后遮罩已销毁", f"windows={len(mask._windows)}")
        after_clip = clip_rect()
        report.check(after_clip != clip, "交还后 ClipCursor 已解除", f"clip={after_clip}")
        if observable:
            state = cursor_visible()
            report.check(state is True, "交还后光标恢复可见", f"visible={state}")
        else:
            report.skip("交还后光标恢复可见", "本机光标不可观测")
    finally:
        try:
            runtime.stop()
        except Exception as exc:  # noqa: BLE001 - 清理失败只报告
            print(f"[WARN] 清理失败：{exc!r}", flush=True)

    return _finish(report, args)


def _finish(report: Report, args: argparse.Namespace) -> int:
    failed = report.failed
    print()
    print(
        f"合计 {len(report.rows)} 项：失败 {len(failed)}，跳过 {len(report.skipped)}",
        flush=True,
    )
    for _status, name, detail in failed:
        print(f"  - {name} {detail}", flush=True)
    for _status, name, detail in report.skipped:
        print(f"  ~ {name}（{detail}）", flush=True)
    if args.json:
        payload = {
            "version": _version(),
            "rows": [{"status": s, "name": n, "detail": d} for s, n, d in report.rows],
            "failed": len(failed),
            "skipped": len(report.skipped),
        }
        text = json.dumps(payload, ensure_ascii=False, indent=2)
        Path(args.json).write_text(text, encoding="utf-8")
        print(f"报告已写入 {args.json}", flush=True)
    return 1 if failed else 0


def _version() -> str:
    try:
        import constants as C

        return C.VERSION
    except Exception:  # noqa: BLE001
        return "?"


if __name__ == "__main__":
    sys.exit(main())
