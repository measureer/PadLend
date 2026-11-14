r"""M4 真机回归：用 Rust sidecar 连接平板、接管、交还。

用法：
    .\.venv\Scripts\python.exe tools\device_regression.py --serial 10.144.144.5:43465
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
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def reader_thread(pipe, sink):
    for line in pipe:
        text = line.strip()
        if text:
            sink.put(text)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--exe", default="padlend-core/target/debug/padlend-core.exe")
    parser.add_argument("--serial", required=True)
    parser.add_argument("--json", default="build/gates/m4-device.json")
    parser.add_argument("--cycles", type=int, default=3, help="接管/交还循环次数")
    parser.add_argument("--no-auto-connect", action="store_true", help="关闭 auto_connect（对照旧竞态）")
    args = parser.parse_args(argv)

    home = Path(tempfile.mkdtemp(prefix="pl-device-"))
    # 默认走 auto_connect=true（真实用户路径）；--no-auto-connect 用于对照复现旧竞态。
    if args.no_auto_connect:
        (home / "config.yaml").write_text("device:\n  auto_connect: false\n", encoding="utf-8")
    env = dict(os.environ, PADLEND_HOME=str(home), PYTHONUNBUFFERED="1")
    env.pop("PADLEND_SIMULATE", None)
    proc = subprocess.Popen(
        [str((ROOT / args.exe).resolve())], cwd=str(ROOT / "padlend-core"), env=env,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, encoding="utf-8", errors="replace", bufsize=1,
    )
    q: queue.Queue[str | None] = queue.Queue()
    threading.Thread(target=reader_thread, args=(proc.stdout, q), daemon=True).start()

    def call(index, method, params=None, timeout=120.0):
        assert proc.stdin is not None
        proc.stdin.write(json.dumps({"id": index, "method": method, "params": params or {}}) + "\n")
        proc.stdin.flush()
        deadline = time.time() + timeout
        while time.time() < deadline:
            item = q.get(timeout=max(0.5, deadline - time.time()))
            if not item:
                raise RuntimeError("sidecar 提前退出")
            message = json.loads(item)
            if message.get("id") == index:
                return message
        raise RuntimeError(f"{method} 超时")

    steps = []
    try:
        while True:
            item = q.get(timeout=60)
            if item and json.loads(item).get("event") in ("ready", "service.ready"):
                break

        boot = call(1, "app.bootstrap")
        steps.append(("bootstrap", boot.get("ok"), (boot.get("result") or {}).get("state")))

        scan = call(2, "devices.scan")
        devices = (scan.get("result") or {}).get("devices") or []
        has_target = any(args.serial in json.dumps(item, ensure_ascii=False) for item in devices)
        steps.append(("devices.scan", scan.get("ok"), f"{len(devices)} 台, 目标在列表={has_target}"))

        t0 = time.time()
        connect = call(3, "devices.connect", {"serial": args.serial}, timeout=180)
        connect_result = connect.get("result") or {}
        steps.append(("devices.connect", bool(connect_result.get("ok")),
                      f"{connect_result} ({time.time()-t0:.1f}s)"))

        time.sleep(3)
        state = call(4, "engine.state")
        state_result = state.get("result") or {}
        connected = bool(state_result.get("connected"))
        steps.append(("engine.state", state.get("ok"), {"state": state_result.get("state"), "connected": connected}))

        if connected:
            index = 5
            for cycle in range(1, args.cycles + 1):
                toggle = call(index, "engine.toggle"); index += 1
                steps.append((f"cycle{cycle}.capture", toggle.get("ok"),
                              (toggle.get("result") or {}).get("state")))
                time.sleep(2)
                metrics = call(index, "engine.metrics"); index += 1
                payload = metrics.get("result") or {}
                steps.append((f"cycle{cycle}.metrics", metrics.get("ok"),
                              f"send_hz={payload.get('send_hz')} p95={payload.get('p95_ms')}"))
                toggle2 = call(index, "engine.toggle"); index += 1
                steps.append((f"cycle{cycle}.release", toggle2.get("ok"),
                              (toggle2.get("result") or {}).get("state")))
                time.sleep(1)
            final = call(index, "engine.state")
            steps.append(("engine.state(after)", final.get("ok"), (final.get("result") or {}).get("state")))
        call(99, "app.shutdown")
        proc.wait(timeout=20)
    except Exception as exc:  # noqa: BLE001
        steps.append(("error", False, repr(exc)))
        proc.kill()
    finally:
        if proc.poll() is None:
            proc.kill()

    payload = {
        "serial": args.serial,
        "ok": all(step[1] for step in steps if step[0] != "error") and not any(s[0] == "error" for s in steps),
        "steps": steps,
    }
    out = Path(args.json)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
    for name, ok, detail in steps:
        print(f"  {'PASS' if ok else 'FAIL'} {name:<28} {detail}")
    print(f"[device] {'PASS' if payload['ok'] else 'FAIL'} -> {out}")
    return 0 if payload["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())