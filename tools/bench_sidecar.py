r"""测量业务 sidecar 的冷启动与常驻内存（onefile / onedir 对照）。

用法（在装了 requirements-build.txt 的 venv 里）：

    .\.venv\Scripts\python.exe tools\bench_sidecar.py --exe src-tauri/binaries/padlend-service-x86_64-pc-windows-msvc.exe --label onefile

指标口径（与前端体验对齐）：

* ready_ms：进程 spawn -> 收到 {"event":"ready"}（Tauri 侧 bridge.rs 等待的就是它）；
* bootstrap_ms：发出 app.bootstrap -> 收到同 id 响应；
* rss_mb / private_mb：就绪后 200ms 采样，含 PyInstaller onefile 的引导父进程子树；
* procs：进程树里的进程数（onefile 恒为 2：引导父 + 真身）。

配置目录用 PADLEND_HOME 指向临时目录，不碰用户的 %APPDATA%\PadLend。
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

TIMEOUT_S = 30.0


class Reader:
    """把 stdout 逐行推入队列，避免读管道时无法超时。"""

    def __init__(self, pipe) -> None:
        self._q: queue.Queue[str | None] = queue.Queue()
        threading.Thread(target=self._run, args=(pipe,), daemon=True).start()

    def _run(self, pipe) -> None:
        try:
            for line in pipe:
                text = line.strip()
                if text:
                    self._q.put(text)
        except Exception:  # noqa: BLE001 - 进程被 kill 时读线程直接结束
            pass
        finally:
            self._q.put(None)

    def next(self, timeout: float = TIMEOUT_S) -> dict:
        item = self._q.get(timeout=timeout)
        if item is None:
            raise RuntimeError("sidecar 已关闭 stdout")
        return json.loads(item)


def dir_size_mb(path: Path) -> float:
    if path.is_file():
        return path.stat().st_size / (1024 * 1024)
    total = 0
    for item in path.rglob("*"):
        if item.is_file():
            total += item.stat().st_size
    return total / (1024 * 1024)


def one_run(exe: Path, home: Path, *, simulate: bool = True) -> dict:
    import psutil

    env = dict(os.environ)
    env["PADLEND_HOME"] = str(home)
    env["PYTHONUNBUFFERED"] = "1"
    argv = [str(exe)]
    if simulate:
        argv += ["--simulate", "--no-hooks"]

    started = time.perf_counter()
    proc = subprocess.Popen(  # noqa: S603 - 被测对象是我们自己构建的产物
        argv,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        cwd=str(exe.parent),
        env=env,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )
    reader = Reader(proc.stdout)
    result: dict = {}

    while True:
        message = reader.next()
        if message.get("event") == "ready":
            result["ready_ms"] = (time.perf_counter() - started) * 1000.0
            break

    boot_started = time.perf_counter()
    assert proc.stdin is not None
    proc.stdin.write(json.dumps({"id": 1, "method": "app.bootstrap", "params": {}}) + "\n")
    proc.stdin.flush()
    while True:
        message = reader.next()
        if message.get("id") == 1:
            if not message.get("ok"):
                raise RuntimeError(f"bootstrap 失败：{message.get('error')}")
            result["bootstrap_ms"] = (time.perf_counter() - boot_started) * 1000.0
            break

    time.sleep(0.2)
    root = psutil.Process(proc.pid)
    tree = [root, *root.children(recursive=True)]
    rss = 0
    private = 0
    for item in tree:
        try:
            info = item.memory_info()
        except psutil.Error:
            continue
        rss += info.rss
        private += getattr(info, "private", info.rss)
    result["rss_mb"] = rss / (1024 * 1024)
    result["private_mb"] = private / (1024 * 1024)
    result["procs"] = len(tree)

    proc.stdin.write(json.dumps({"id": 2, "method": "app.shutdown", "params": {}}) + "\n")
    proc.stdin.flush()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)
    result["exit_code"] = proc.returncode
    return result


def summarize(samples: list[dict]) -> dict:
    def pick(key: str) -> dict:
        values = [s[key] for s in samples if key in s]
        if not values:
            return {}
        values.sort()
        return {
            "median": round(statistics.median(values), 1),
            "p95": round(values[max(0, int(len(values) * 0.95) - 1)], 1),
            "min": round(min(values), 1),
            "max": round(max(values), 1),
        }

    return {
        "ready_ms": pick("ready_ms"),
        "bootstrap_ms": pick("bootstrap_ms"),
        "rss_mb": pick("rss_mb"),
        "private_mb": pick("private_mb"),
        "procs": samples[0].get("procs") if samples else None,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="bench_sidecar", description="sidecar 启动/内存测量")
    parser.add_argument("--exe", required=True, help="sidecar 可执行文件（onedir 传目录里的 exe）")
    parser.add_argument("--label", default="", help="结果标签，例如 onefile / onedir")
    parser.add_argument("--runs", type=int, default=7, help="重复次数（默认 7）")
    parser.add_argument("--json", default=None, help="把结果追加写入该 JSON 文件")
    parser.add_argument("--no-simulate", action="store_true", help="不用 --simulate（会尝试访问真机）")
    args = parser.parse_args(argv)

    exe = Path(args.exe).resolve()
    if not exe.is_file():
        print(f"[error] 找不到 {exe}", file=sys.stderr)
        return 2

    samples: list[dict] = []
    home = Path(tempfile.mkdtemp(prefix="padlend-bench-"))
    try:
        for index in range(args.runs):
            try:
                sample = one_run(exe, home, simulate=not args.no_simulate)
            except Exception as exc:  # noqa: BLE001 - 单次失败要能报出原因
                print(f"[error] 第 {index + 1} 次失败：{exc!r}", file=sys.stderr)
                return 1
            samples.append(sample)
            print(
                f"  run {index + 1}: ready={sample['ready_ms']:.0f}ms "
                f"bootstrap={sample['bootstrap_ms']:.0f}ms "
                f"rss={sample['rss_mb']:.1f}MB procs={sample['procs']} exit={sample['exit_code']}"
            )
    finally:
        shutil.rmtree(home, ignore_errors=True)

    # onedir 形态真正的体积在 exe 同级的 _internal 目录里。
    disk_target = exe.parent if (exe.parent / "_internal").is_dir() else exe
    payload = {
        "label": args.label or exe.name,
        "exe": str(exe),
        "disk_mb": round(dir_size_mb(disk_target), 2),
        "runs": len(samples),
        "summary": summarize(samples),
        "samples": samples,
    }
    print(json.dumps(payload["summary"], ensure_ascii=False, indent=2))
    if args.json:
        target = Path(args.json)
        target.parent.mkdir(parents=True, exist_ok=True)
        existing = []
        if target.is_file():
            try:
                existing = json.loads(target.read_text(encoding="utf-8"))
            except json.JSONDecodeError:
                existing = []
        existing = [item for item in existing if item.get("label") != payload["label"]]
        existing.append(payload)
        target.write_text(json.dumps(existing, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"[ok] 写入 {target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
