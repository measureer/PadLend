r"""RPC golden trace 的录制与回放（25 个方法的跨实现门禁）。

录制：对 Python 版 sidecar 发一段固定会话，把每行请求/响应/事件写进 NDJSON。
回放：对任意实现（Rust sidecar 或 Python）重放同一批请求，按"行为等价"判定：
      ok/err 必须一致；ok 时比较 result 的**结构**（键集合 + 值类型），
      值差异单独列出供人工判断（端口、路径、时间戳天然不同）。

用法：

    .\.venv\Scripts\python.exe tools\rpc_trace.py record --out tests\fixtures\rpc_golden\simulate.ndjson
    .\.venv\Scripts\python.exe tools\rpc_trace.py replay --trace tests\fixtures\rpc_golden\simulate.ndjson --exe padlend-core\target\debug\padlend-core.exe
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

TIMEOUT_S = 30.0

# 覆盖全部 25 个方法的固定会话。写类方法故意只给空参数：既记录成功形状，
# 也记录参数校验的失败形状（同样是契约的一部分）。
SESSION: list[tuple[str, dict[str, Any]]] = [
    ("app.bootstrap", {}),
    ("system.paths", {}),
    ("config.get", {}),
    ("layout.desktop", {}),
    ("layout.initial", {}),
    ("devices.scan", {}),
    ("devices.locate", {"url": "adb-test-0000"}),
    ("devices.add", {"name": "bench", "address": "1.2.3.4:5555"}),
    ("devices.connect", {"serial": "no-such-serial"}),
    ("engine.state", {}),
    ("engine.metrics", {}),
    ("engine.toggle", {}),
    ("engine.reconnect", {}),
    ("engine.panic", {}),
    ("engine.inject_clipboard", {"text": "padlend"}),
    ("layout.save", {}),
    ("config.save", {}),
    ("devices.remove", {"id": "no-such-id"}),
    ("devices.disconnect", {}),
    ("diagnostics.static", {}),
    ("diagnostics.run", {}),
    ("diagnostics.export", {}),
    ("logs.recent", {"limit": 20}),
    ("wizard.check", {}),
    ("app.shutdown", {}),
]


class Reader:
    def __init__(self, pipe) -> None:
        self._q: queue.Queue[str | None] = queue.Queue()
        threading.Thread(target=self._run, args=(pipe,), daemon=True).start()

    def _run(self, pipe) -> None:
        try:
            for line in pipe:
                text = line.strip()
                if text:
                    self._q.put(text)
        except Exception:  # noqa: BLE001
            pass
        finally:
            self._q.put(None)

    def next(self, timeout: float = TIMEOUT_S) -> dict | None:
        item = self._q.get(timeout=timeout)
        if item is None:
            return None
        return json.loads(item)


def spawn(target: list[str], cwd: Path, home: Path) -> subprocess.Popen:
    env = dict(os.environ)
    env["PADLEND_HOME"] = str(home)
    env["PYTHONUNBUFFERED"] = "1"
    return subprocess.Popen(  # noqa: S603 - 被测对象是本地构建产物
        target,
        cwd=str(cwd),
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )


def wait_ready(reader: Reader) -> None:
    while True:
        message = reader.next()
        if message is None:
            raise RuntimeError("进程在 ready 之前退出")
        if message.get("event") in ("ready", "service.ready"):
            return


def exchange(proc: subprocess.Popen, reader: Reader, seq: int, method: str,
             params: dict[str, Any]) -> dict[str, Any]:
    assert proc.stdin is not None
    request = {"id": seq, "method": method, "params": params}
    proc.stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
    proc.stdin.flush()
    events: list[dict[str, Any]] = []
    deadline = time.time() + TIMEOUT_S
    while time.time() < deadline:
        message = reader.next(timeout=max(0.5, deadline - time.time()))
        if message is None:
            raise RuntimeError(f"{method}: 进程在响应前退出")
        if message.get("id") == seq:
            return {"seq": seq, "method": method, "params": params, "response": message, "events": events}
        if "event" in message:
            events.append(message)
    raise RuntimeError(f"{method}: 超时未收到响应")


def run_session(proc: subprocess.Popen, reader: Reader, session: list) -> list[dict]:
    wait_ready(reader)
    records = []
    for index, (method, params) in enumerate(session, start=1):
        try:
            records.append(exchange(proc, reader, index, method, params))
        except Exception as exc:  # noqa: BLE001 - 单个方法失败也要写进 trace
            records.append({"seq": index, "method": method, "params": params,
                            "response": {"id": index, "ok": False, "error": f"HARNESS: {exc}"},
                            "events": []})
        if method == "app.shutdown":
            break
    return records


def shutdown(proc: subprocess.Popen) -> None:
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def schema_diff(expected: Any, actual: Any, path: str = "result", out: list[str] | None = None) -> list[str]:
    """比较结构：键集合、容器类型、标量类型；数组只比较首元素的结构。"""
    diffs = out if out is not None else []
    if isinstance(expected, dict) and isinstance(actual, dict):
        missing = sorted(set(expected) - set(actual))
        extra = sorted(set(actual) - set(expected))
        if missing:
            diffs.append(f"{path}: 缺键 {missing}")
        if extra:
            diffs.append(f"{path}: 多键 {extra}")
        for key in sorted(set(expected) & set(actual)):
            schema_diff(expected[key], actual[key], f"{path}.{key}", diffs)
    elif isinstance(expected, list) and isinstance(actual, list):
        if expected and not actual:
            diffs.append(f"{path}: 期望非空数组，实际为空")
        elif expected and actual:
            schema_diff(expected[0], actual[0], f"{path}[0]", diffs)
    else:
        expected_type = type(expected).__name__
        actual_type = type(actual).__name__
        if expected_type != actual_type:
            diffs.append(f"{path}: 类型 {expected_type} -> {actual_type}")
    return diffs


def value_diff(expected: Any, actual: Any, path: str = "result", out: list[str] | None = None) -> list[str]:
    diffs = out if out is not None else []
    if isinstance(expected, dict) and isinstance(actual, dict):
        for key in sorted(set(expected) & set(actual)):
            value_diff(expected[key], actual[key], f"{path}.{key}", diffs)
    elif isinstance(expected, list) and isinstance(actual, list):
        if len(expected) == len(actual):
            for index, (left, right) in enumerate(zip(expected, actual)):
                value_diff(left, right, f"{path}[{index}]", diffs)
        elif expected or actual:
            diffs.append(f"{path}: 长度 {len(expected)} -> {len(actual)}")
    elif expected != actual:
        diffs.append(f"{path}: {expected!r} -> {actual!r}")
    return diffs


def cmd_record(args: argparse.Namespace, repo: Path) -> int:
    home = Path(tempfile.mkdtemp(prefix="padlend-trace-"))
    command = [sys.executable, "-m", "service.server", "--simulate", "--no-hooks"]
    proc = spawn(command, repo, home)
    reader = Reader(proc.stdout)
    try:
        records = run_session(proc, reader, SESSION)
    finally:
        shutdown(proc)
        shutil.rmtree(home, ignore_errors=True)

    target = Path(args.out)
    target.parent.mkdir(parents=True, exist_ok=True)
    with target.open("w", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record, ensure_ascii=False) + "\n")
    ok_count = sum(1 for item in records if item["response"].get("ok"))
    print(f"[ok] {len(records)} 条记录 -> {target}（ok={ok_count}, err={len(records) - ok_count}）")
    for item in records:
        state = "ok" if item["response"].get("ok") else "err"
        detail = "" if state == "ok" else str(item["response"].get("error"))[:60]
        print(f"  {item['method']:<26} {state:<3} {detail}")
    return 0


def cmd_replay(args: argparse.Namespace, repo: Path) -> int:
    trace = [json.loads(line) for line in Path(args.trace).read_text(encoding="utf-8").splitlines() if line.strip()]
    home = Path(tempfile.mkdtemp(prefix="padlend-replay-"))
    if args.exe:
        command = [str(Path(args.exe).resolve())]
    else:
        command = [sys.executable, "-m", "service.server", "--simulate", "--no-hooks"]
    proc = spawn(command, repo, home)
    reader = Reader(proc.stdout)
    rows = []
    try:
        wait_ready(reader)
        for record in trace:
            method = record["method"]
            params = record.get("params") or {}
            try:
                got = exchange(proc, reader, record["seq"], method, params)["response"]
            except Exception as exc:  # noqa: BLE001
                got = {"ok": False, "error": f"HARNESS: {exc}"}
            expected = record["response"]
            problems: list[str] = []
            got_error = str(got.get("error") or "")
            if got_error.startswith("not implemented:"):
                # 未实现不能靠"都报错"混过去（layout.save 这类期望报错的方法会假通过）。
                problems.append(f"未实现（{got_error[:60]}）")
            elif bool(expected.get("ok")) != bool(got.get("ok")):
                problems.append(f"ok/err 不一致（期望 {expected.get('ok')}，实际 {got.get('ok')}：{got_error[:70]}）")
            elif expected.get("ok"):
                problems.extend(schema_diff(expected.get("result"), got.get("result")))
                if args.strict_values:
                    problems.extend(value_diff(expected.get("result"), got.get("result")))
            else:
                expected_error = str(expected.get("error") or "")
                if expected_error != got_error:
                    problems.append(f"错误文本不同（期望 {expected_error[:50]!r}，实际 {got_error[:50]!r}）")
            rows.append((method, problems))
            if method == "app.shutdown":
                break
    finally:
        shutdown(proc)
        shutil.rmtree(home, ignore_errors=True)

    passed = [row for row in rows if not row[1]]
    print(f"[replay] {len(passed)}/{len(rows)} 方法通过")
    for method, problems in rows:
        if problems:
            print(f"  FAIL {method}")
            for problem in problems[:4]:
                print(f"       {problem}")
    if args.json:
        target = Path(args.json)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(
            {"command": command, "passed": len(passed), "total": len(rows),
             "failures": {method: problems for method, problems in rows if problems}},
            ensure_ascii=False, indent=2), encoding="utf-8")
    return 0 if len(passed) == len(rows) else 1


def main(argv: list[str] | None = None) -> int:
    repo = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(prog="rpc_trace", description="RPC golden trace 录制/回放")
    sub = parser.add_subparsers(dest="cmd", required=True)

    record = sub.add_parser("record", help="对 Python sidecar 录制")
    record.add_argument("--out", default=str(repo / "tests" / "fixtures" / "rpc_golden" / "simulate.ndjson"))

    replay = sub.add_parser("replay", help="对某个实现回放")
    replay.add_argument("--trace", default=str(repo / "tests" / "fixtures" / "rpc_golden" / "simulate.ndjson"))
    replay.add_argument("--exe", default=None, help="留空则回放给 Python 版（自检）")
    replay.add_argument("--strict-values", action="store_true", help="连值一起比（默认只比结构）")
    replay.add_argument("--json", default=None)

    args = parser.parse_args(argv)
    return cmd_record(args, repo) if args.cmd == "record" else cmd_replay(args, repo)


if __name__ == "__main__":
    raise SystemExit(main())
