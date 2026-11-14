r"""Raw Input 计数对拍：**同时**拉起两个实现，注入同一批事件，比较计数。

M0 的出口标准就是它：python main.py --debug-raw 与 padlend-core --debug-raw
在同一段注入输入下必须给出等价的计数。

为什么必须同时跑：先后跑会被"用户自己动鼠标 / 远程桌面注入"污染，实测能差出几十个事件，
得出假的失败结论。同时跑时两边看到的是同一条事件流。

注入用 SendInput 的相对位移（MOUSEEVENTF_MOVE），注入前后保存并恢复光标位置。
"""

from __future__ import annotations

import argparse
import ctypes
from ctypes import wintypes
import json
import os
import re
import subprocess
import sys
import threading
import time
from pathlib import Path

MOUSEEVENTF_MOVE = 0x0001
INPUT_MOUSE = 0
RAW_LINE = re.compile(r"\[raw\]\s+(\d+)\s+events")


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", ctypes.c_long),
        ("dy", ctypes.c_long),
        ("mouseData", ctypes.c_ulong),
        ("dwFlags", ctypes.c_ulong),
        ("time", ctypes.c_ulong),
        ("dwExtraInfo", ctypes.c_void_p),
    ]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("wVk", ctypes.c_ushort),
        ("wScan", ctypes.c_ushort),
        ("dwFlags", ctypes.c_ulong),
        ("time", ctypes.c_ulong),
        ("dwExtraInfo", ctypes.c_void_p),
    ]


class HARDWAREINPUT(ctypes.Structure):
    _fields_ = [
        ("uMsg", ctypes.c_ulong),
        ("wParamL", ctypes.c_ushort),
        ("wParamH", ctypes.c_ushort),
    ]


class _INPUTUNION(ctypes.Union):
    _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT), ("hi", HARDWAREINPUT)]


class INPUT(ctypes.Structure):
    _anonymous_ = ("u",)
    _fields_ = [("type", ctypes.c_ulong), ("u", _INPUTUNION)]


def get_cursor() -> tuple[int, int]:
    point = wintypes.POINT()
    ctypes.windll.user32.GetCursorPos(ctypes.byref(point))
    return point.x, point.y


def set_cursor(x: int, y: int) -> None:
    ctypes.windll.user32.SetCursorPos(int(x), int(y))


def inject_moves(count: int, *, step: int = 2, delay: float = 0.01) -> None:
    """注入 count 次相对位移；dx/dy 交替方向，避免把光标推出屏幕。"""
    for index in range(count):
        direction = 1 if (index // 8) % 2 == 0 else -1
        move = INPUT()
        move.type = INPUT_MOUSE
        move.mi = MOUSEINPUT(step * direction, step // 2 * direction, 0, MOUSEEVENTF_MOVE, 0, None)
        sent = ctypes.windll.user32.SendInput(1, ctypes.byref(move), ctypes.sizeof(INPUT))
        if sent != 1:
            raise RuntimeError(f"SendInput 失败（GetLastError={ctypes.get_last_error()}）")
        time.sleep(delay)


def _start(argv: list[str], cwd: Path, env: dict) -> tuple[subprocess.Popen, list[str]]:
    proc = subprocess.Popen(  # noqa: S603 - 被测对象是本地构建产物
        argv, cwd=str(cwd), env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, encoding="utf-8", errors="replace", bufsize=1,
    )
    lines: list[str] = []

    def _reader() -> None:
        assert proc.stdout is not None
        for line in proc.stdout:
            lines.append(line.strip())

    threading.Thread(target=_reader, daemon=True).start()
    return proc, lines


def run_pair(targets: list[tuple[str, list[str], Path]], *, inject: int, warmup: float, tail: float) -> dict:
    env = dict(os.environ)
    env["PYTHONUNBUFFERED"] = "1"
    env.setdefault("PADLEND_HOME", str(Path(os.environ.get("TEMP", ".")) / "padlend-probe"))
    started = [(name, *_start(argv, cwd, env)) for name, argv, cwd in targets]
    results: dict[str, dict] = {}
    try:
        time.sleep(warmup)
        inject_moves(inject)
        time.sleep(tail)
    finally:
        for name, proc, lines in started:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
            per_second = [int(m.group(1)) for m in RAW_LINE.finditer("\n".join(lines))]
            results[name] = {"per_second": per_second, "total": sum(per_second)}
    return results


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="raw_input_probe", description="Raw Input 计数对拍")
    parser.add_argument("--rust", default="padlend-core/target/debug/padlend-core.exe")
    parser.add_argument("--python-exe", default=sys.executable)
    parser.add_argument("--inject", type=int, default=100, help="注入的相对移动次数")
    parser.add_argument("--warmup", type=float, default=3.0, help="启动后等待注册的时间")
    parser.add_argument("--tail", type=float, default=2.0, help="注入结束后继续采样的时间")
    parser.add_argument("--json", default=None, help="把结果写入该文件")
    parser.add_argument("--only", choices=("rust", "python"), default=None, help="只测一侧（冒烟用）")
    args = parser.parse_args(argv)

    repo = Path(__file__).resolve().parent.parent
    targets = [
        ("rust", [str(repo / args.rust), "--debug-raw"], repo / "padlend-core"),
        ("python", [args.python_exe, "main.py", "--debug-raw"], repo),
    ]
    if args.only:
        targets = [item for item in targets if item[0] == args.only]

    origin = get_cursor()
    try:
        print(f"[probe] 同时启动 {len(targets)} 个实现，注入 {args.inject} 次相对位移，光标原点 {origin}")
        for name, cmd, _cwd in targets:
            print(f"[probe] {name}: {' '.join(cmd)}")
        results = run_pair(targets, inject=args.inject, warmup=args.warmup, tail=args.tail)
    finally:
        set_cursor(*origin)

    for name, item in results.items():
        print(f"[probe] {name} -> {item['total']} events {item['per_second']}")

    rust = results.get("rust", {}).get("total")
    python_total = results.get("python", {}).get("total")
    if rust is not None and python_total is not None:
        delta = rust - python_total
        verdict = abs(delta) <= 2
    else:
        delta = None
        single = rust if rust is not None else python_total or 0
        verdict = single >= int(args.inject * 0.9)
        print(f"[probe] 冒烟：注入 {args.inject} 次，收到 {single} 个事件")
    summary = {
        "injected": args.inject,
        "rust": rust,
        "python": python_total,
        "delta": delta,
        "verdict": "pass" if verdict else "fail",
        "detail": results,
    }
    print(json.dumps({k: summary[k] for k in ("injected", "rust", "python", "delta", "verdict")}, ensure_ascii=False))
    if args.json:
        target = Path(args.json)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    return 0 if verdict else 1


if __name__ == "__main__":
    raise SystemExit(main())
