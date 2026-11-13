r"""B2 门禁总入口：一条命令跑完当前阶段所有可自动化的门禁，产出 JSON 证据。

用法：

    .\.venv\Scripts\python.exe tools\run_gates.py --stage M0 --json build\gates\M0.json

门禁定义见 docs/B2_MIGRATION_PLAN.md。活体 Raw Input 门禁在桌面锁定时自动标记 blocked。
"""

from __future__ import annotations

import argparse
import ctypes
import json
import platform
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATE = ROOT / "padlend-core"


def run(cmd: list[str], cwd: Path, timeout: int = 900) -> tuple[int, str]:
    started = time.time()
    proc = subprocess.run(  # noqa: S603 - 固定命令
        cmd, cwd=str(cwd), capture_output=True, text=True,
        encoding="utf-8", errors="replace", timeout=timeout,
    )
    elapsed = time.time() - started
    output = (proc.stdout or "") + (proc.stderr or "")
    print(f"    ({elapsed:.1f}s, exit={proc.returncode})")
    return proc.returncode, output


def gate_cargo_test() -> dict:
    """Rust 单测（含 Raw Input 解码向量、各模块移植测试）。"""
    print("  [G-rust] cargo test")
    code, output = run(["cargo", "test", "--quiet"], CRATE)
    passed = 0
    failed = 0
    for match in re.finditer(r"test result: (\w+)\. (\d+) passed; (\d+) failed", output):
        passed += int(match.group(2))
        failed += int(match.group(3))
    failures = re.findall(r"^---- (\S+) stdout ----", output, re.MULTILINE)
    return {
        "gate": "rust-tests",
        "ok": code == 0 and failed == 0,
        "passed": passed,
        "failed": failed,
        "failing_tests": failures[:20],
        "tail": output[-1500:] if code != 0 else "",
    }


def gate_router_replay() -> dict:
    """G2：Raw Input 回放 → 状态迁移逐条一致（fixture 驱动）。"""
    print("  [G2] router scenario replay")
    code, output = run(
        ["cargo", "test", "--quiet", "--lib", "fixture_scenarios_replay_identically"], CRATE
    )
    passed = "1 passed" in output and code == 0
    return {
        "gate": "router-replay",
        "ok": passed,
        "tail": "" if passed else output[-1200:],
    }


def gate_python_suite() -> dict:
    """Python 侧回归（确认移植没有误伤原来的实现）。"""
    print("  [G-python] pytest -q")
    code, output = run([sys.executable, "-m", "pytest", "tests", "-q", "--no-header"], ROOT)
    # 本仓库的 pytest 配置不打印 "N passed" 汇总行，改用 collect-only 的节点清单计数。
    # 不加 -q：addopts 已带 -q，再叠加会把节点清单也省掉。
    _, collected_out = run([sys.executable, "-m", "pytest", "tests", "--collect-only"], ROOT)
    collected_count = len([line for line in collected_out.splitlines() if "::" in line])
    collected = re.search(r"(\d+)\s+tests? collected", collected_out)
    failed_match = re.search(r"(\d+) failed", output)
    return {
        "gate": "python-suite",
        "ok": code == 0,
        "passed": int(collected.group(1)) if collected else collected_count,
        "failed": int(failed_match.group(1)) if failed_match else 0,
        "tail": output[-1200:] if code != 0 else "",
    }


def gate_rpc_golden(rust_exe: Path, minimum: int = 0) -> dict:
    """25 个 RPC 方法的 golden trace 回放。"""
    print("  [G-rpc] rpc_trace replay")
    report = ROOT / "build" / "gates" / "rpc-replay.json"
    report.parent.mkdir(parents=True, exist_ok=True)
    code, output = run([
        sys.executable, "tools/rpc_trace.py", "replay",
        "--exe", str(rust_exe), "--json", str(report),
    ], ROOT)
    data = {}
    if report.is_file():
        data = json.loads(report.read_text(encoding="utf-8"))
    passed = data.get("passed", 0)
    return {
        "gate": "rpc-golden",
        "ok": code == 0 if minimum >= 25 else passed >= minimum,
        "min_required": minimum,
        "passed": passed,
        "total": data.get("total", 0),
        "failures": data.get("failures", {}),
        "tail": output[-800:] if code != 0 else "",
    }


def desktop_locked() -> bool:
    user32 = ctypes.windll.user32
    point = ctypes.wintypes.POINT() if hasattr(ctypes, "wintypes") else None
    if point is None:
        from ctypes import wintypes

        point = wintypes.POINT()
    user32.GetCursorPos(ctypes.byref(point))
    ok = user32.SetCursorPos(point.x, point.y)
    return not bool(ok)


def gate_rawinput_live(rust_exe: Path) -> dict:
    """Raw Input 活体计数对拍（需要解锁的桌面）。"""
    print("  [G1-live] raw_input_probe")
    if desktop_locked():
        print("    桌面已锁，标记 blocked（需人工解锁后重跑）")
        return {"gate": "rawinput-live", "ok": None, "status": "blocked",
                "reason": "desktop locked (SendInput/SetCursorPos 返回 ACCESS_DENIED)"}
    report = ROOT / "build" / "gates" / "rawinput-live.json"
    report.parent.mkdir(parents=True, exist_ok=True)
    code, output = run([
        sys.executable, "tools/raw_input_probe.py",
        "--rust", str(rust_exe.relative_to(ROOT)).replace("\\", "/"),
        "--python-exe", sys.executable,
        "--json", str(report),
    ], ROOT)
    data = {}
    if report.is_file():
        data = json.loads(report.read_text(encoding="utf-8"))
    return {
        "gate": "rawinput-live",
        "ok": code == 0,
        "rust": data.get("rust"),
        "python": data.get("python"),
        "delta": data.get("delta"),
        "tail": output[-600:] if code != 0 else "",
    }


# 每个阶段要求哪些门禁；rpc-golden 在 M5 之前只作为进度指标（min=0），M5 才要求 25/25。
STAGES = {
    "stage0": ["rust-tests", "python-suite", "rpc-golden"],
    "M0": ["rust-tests", "python-suite", "rawinput-live"],
    "M1": ["rust-tests", "python-suite", "rpc-golden"],
    "M2": ["rust-tests", "python-suite", "rawinput-live", "rpc-golden"],
    "M3": ["rust-tests", "python-suite", "router-replay", "rpc-golden"],
    "M4": ["rust-tests", "python-suite", "router-replay", "rpc-golden"],
    "M5": ["rust-tests", "python-suite", "router-replay", "rpc-golden"],
}
RPC_MIN = {"M5": 25}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="run_gates", description="B2 阶段门禁")
    parser.add_argument("--stage", default="M1", choices=sorted(STAGES))
    parser.add_argument("--json", default=None, help="证据写入路径")
    parser.add_argument("--profile", default="debug", choices=("debug", "release"))
    args = parser.parse_args(argv)

    rust_exe = CRATE / "target" / args.profile / (
        "padlend-core.exe" if platform.system() == "Windows" else "padlend-core"
    )
    if not rust_exe.is_file():
        print(f"[warn] 找不到 {rust_exe}，先 cargo build")
        subprocess.run(["cargo", "build"], cwd=str(CRATE), check=False)
    if not rust_exe.is_file():
        print("[error] 构建产物缺失，门禁无法继续")
        return 2

    print(f"[gates] stage={args.stage} profile={args.profile}")
    handlers = {
        "rust-tests": gate_cargo_test,
        "python-suite": gate_python_suite,
        "rpc-golden": lambda: gate_rpc_golden(rust_exe, RPC_MIN.get(args.stage, 0)),
        "rawinput-live": lambda: gate_rawinput_live(rust_exe),
        "router-replay": gate_router_replay,
    }
    results = [handlers[name]() for name in STAGES[args.stage]]

    print("\n[gates] 结果")
    for item in results:
        state = "PASS" if item["ok"] else ("BLOCKED" if item.get("ok") is None else "FAIL")
        print(f"  {state:<8} {item['gate']}")
        if item["gate"] == "rpc-golden":
            print(f"           {item['passed']}/{item['total']} 方法")
        if item["gate"] in ("rust-tests", "python-suite"):
            print(f"           {item['passed']} passed / {item['failed']} failed")

    failed = [item for item in results if item["ok"] is False]
    payload = {
        "stage": args.stage,
        "profile": args.profile,
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
        "verdict": "pass" if not failed else "fail",
        "results": results,
    }
    if args.json:
        target = Path(args.json)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"[gates] 证据 -> {target}")
    return 0 if not failed else 1


if __name__ == "__main__":
    raise SystemExit(main())
