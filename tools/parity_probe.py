r"""跨实现一致性探针：同一台机器上分别跑 Python 与 Rust sidecar（真实模式），
对确定性方法做**值级**对比（不是结构）。

用法：
    .\.venv\Scripts\python.exe tools\parity_probe.py --exe padlend-core\target\debug\padlend-core.exe

忽略的易变字段：metrics 的计数、logs、facts、时间戳、诊断路径。
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
METHODS = ["system.paths", "layout.desktop", "layout.initial", "config.get", "app.bootstrap"]
VOLATILE = {"metrics", "logs", "facts", "collectedAt", "uptime", "pid", "path"}


class Reader:
    def __init__(self, pipe):
        self.q: queue.Queue[str | None] = queue.Queue()
        threading.Thread(target=self._run, args=(pipe,), daemon=True).start()

    def _run(self, pipe):
        try:
            for line in pipe:
                text = line.strip()
                if text:
                    self.q.put(text)
        finally:
            self.q.put(None)

    def next(self, timeout=60.0):
        item = self.q.get(timeout=timeout)
        if item is None:
            raise RuntimeError("sidecar 提前退出")
        return json.loads(item)


def run_sidecar(command: list[str], cwd: Path, home: Path) -> dict:
    env = dict(os.environ, PADLEND_HOME=str(home), PYTHONUNBUFFERED="1")
    env.pop("PADLEND_SIMULATE", None)
    proc = subprocess.Popen(
        command, cwd=str(cwd), env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, text=True, encoding="utf-8", errors="replace", bufsize=1,
    )
    reader = Reader(proc.stdout)
    results: dict[str, dict] = {}
    try:
        while True:
            message = reader.next()
            if message.get("event") in ("ready", "service.ready"):
                break
        for index, method in enumerate(METHODS, start=1):
            assert proc.stdin is not None
            proc.stdin.write(json.dumps({"id": index, "method": method, "params": {}}) + "\n")
            proc.stdin.flush()
            while True:
                message = reader.next()
                if message.get("id") == index:
                    results[method] = message
                    break
        assert proc.stdin is not None
        proc.stdin.write(json.dumps({"id": 99, "method": "app.shutdown", "params": {}}) + "\n")
        proc.stdin.flush()
        try:
            proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()
    finally:
        if proc.poll() is None:
            proc.kill()
    return results


def normalize(value, home: Path):
    """把临时配置目录路径折叠成 <home>，让两边的 configPath/logDir 可比。"""
    home_text = str(home)
    if isinstance(value, dict):
        return {key: normalize(item, home) for key, item in value.items()}
    if isinstance(value, list):
        return [normalize(item, home) for item in value]
    if isinstance(value, str):
        return value.replace(home_text, "<home>")
    return value


def diff(expected, actual, path="result", out=None) -> list[str]:
    out = out if out is not None else []
    if isinstance(expected, dict) and isinstance(actual, dict):
        for key in sorted(set(expected) | set(actual)):
            if key in VOLATILE:
                continue
            if key not in expected:
                out.append(f"{path}.{key}: 仅 Rust 有")
            elif key not in actual:
                out.append(f"{path}.{key}: 仅 Python 有")
            else:
                diff(expected[key], actual[key], f"{path}.{key}", out)
    elif isinstance(expected, list) and isinstance(actual, list):
        if len(expected) != len(actual):
            out.append(f"{path}: 长度 {len(expected)} -> {len(actual)}")
        for index, (left, right) in enumerate(zip(expected, actual)):
            diff(left, right, f"{path}[{index}]", out)
    else:
        if expected != actual:
            out.append(f"{path}: {expected!r} -> {actual!r}")
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="parity_probe", description="Python/Rust sidecar 值级对比")
    parser.add_argument("--exe", default="padlend-core/target/debug/padlend-core.exe")
    parser.add_argument("--json", default=None, help="把差异写进该文件")
    args = parser.parse_args(argv)

    # 各自用全新的配置目录（否则 second 的 firstRun 必然为 false），
    # 比对前把临时目录路径归一化成 <home>。
    py_home = Path(tempfile.mkdtemp(prefix="pl-parity-py-"))
    rs_home = Path(tempfile.mkdtemp(prefix="pl-parity-rs-"))
    print("[parity] Python sidecar（真实模式）…")
    py = run_sidecar([sys.executable, "-m", "service.server", "--no-hooks"], ROOT, py_home)
    print("[parity] Rust sidecar（真实模式）…")
    rs = run_sidecar([str((ROOT / args.exe).resolve())], ROOT / "padlend-core", rs_home)
    py = normalize(py, py_home)
    rs = normalize(rs, rs_home)

    total = 0
    report: dict[str, list[str]] = {}
    for method in METHODS:
        expected = py.get(method, {})
        actual = rs.get(method, {})
        if expected.get("ok") != actual.get("ok"):
            problems = [f"ok 不一致：{expected.get('ok')} vs {actual.get('ok')}"]
        else:
            problems = diff(expected.get("result"), actual.get("result"))
        report[method] = problems
        total += len(problems)
        state = "一致" if not problems else f"{len(problems)} 处差异"
        print(f"  {method:<16} {state}")
        for item in problems[:6]:
            print(f"      {item}")

    if args.json:
        target = Path(args.json)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"[parity] 合计 {total} 处差异")
    return 0 if total == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
