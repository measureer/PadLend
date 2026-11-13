r"""录制 Router 行为场景（G2 门禁的跨语言素材）。

用 tests/fakes.py 的替身驱动真实 Router，把"输入事件序列 → 状态迁移 + 下发报文"
固化成 JSON；Rust 版 Router 落地后回放同一份脚本，必须逐条一致。

确定性：全部通过 router._sender_frame() 手动出帧，不依赖真实线程/时钟。

用法：.\.venv\Scripts\python.exe tools\router_scenarios.py --out tests\fixtures\router_scenarios.json
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tests"))

import constants as C  # noqa: E402
from core.state import State  # noqa: E402
from test_router import capture_manually, make_rig  # noqa: E402
from fakes import key, raw  # noqa: E402


def drain(rig, rounds: int = 1) -> None:
    for _ in range(rounds):
        rig.router._sender_frame()


def record(name: str, script, *, rounds_per_frame: int = 1) -> dict:
    """script: [(op, payload), ...]，op ∈ capture/release/raw/frame/key/start_frame。"""
    rig = make_rig()
    rig.router.start()
    try:
        for op, payload in script:
            if op == "capture":
                capture_manually(rig)
            elif op == "release":
                rig.router.toggle_manual()
            elif op == "raw":
                rig.router.on_raw(raw(**payload))
            elif op == "key":
                rig.router.on_key(key(**payload))
            elif op == "frame":
                for _ in range(int(payload.get("times", 1))):
                    rig.router._sender_frame()
            else:
                raise ValueError(f"未知操作 {op}")
        state = rig.router.state
        estimator = rig.router.estimator
        return {
            "name": name,
            "script": [{"op": op, "payload": payload} for op, payload in script],
            "expect": {
                "state": str(state),
                "transitions": [[str(a), str(b), reason] for a, b, reason in rig.transitions],
                "mouse_reports": [list(item) for item in rig.session.mouse_reports],
                "key_reports": [[mod, list(usages)] for mod, usages in rig.session.key_reports],
                "clip": None if not rig.mask.shown else [
                    rig.mask.shown[0].left, rig.mask.shown[0].top,
                    rig.mask.shown[0].right, rig.mask.shown[0].bottom,
                ],
                "estimator": None if estimator is None else {
                    "x": round(estimator.x, 3), "y": round(estimator.y, 3),
                    "width": estimator.width, "height": estimator.height,
                },
            },
        }
    finally:
        rig.router.stop()


SCENARIOS = [
    (
        "manual_entry_and_move",
        [
            ("capture", {}),
            ("frame", {"times": 1}),
            ("raw", {"dx": 12, "dy": -4}),
            ("frame", {"times": 1}),
            ("raw", {"dx": -30, "dy": 8}),
            ("frame", {"times": 1}),
        ],
    ),
    (
        "buttons_while_captured",
        [
            ("capture", {}),
            ("frame", {"times": 1}),
            ("raw", {"dx": 3, "dy": 0, "buttons": C.MOUSE_BTN_LEFT}),
            ("frame", {"times": 1}),
            ("raw", {"dx": 3, "dy": 0, "buttons": 0}),
            ("frame", {"times": 1}),
        ],
    ),
    (
        "wheel_accumulates_detents",
        [
            ("capture", {}),
            ("frame", {"times": 1}),
            ("raw", {"wheel": 60}),
            ("frame", {"times": 1}),
            ("raw", {"wheel": 60}),
            ("frame", {"times": 1}),
            ("raw", {"wheel": 120}),
            ("frame", {"times": 1}),
        ],
    ),
    (
        "keyboard_while_captured",
        [
            ("capture", {}),
            ("frame", {"times": 1}),
            ("key", {"vk": 0x41, "down": True}),
            ("key", {"vk": 0x42, "down": True}),
            ("key", {"vk": 0x41, "down": False}),
            ("frame", {"times": 1}),
        ],
    ),
    (
        "manual_release",
        [
            ("capture", {}),
            ("frame", {"times": 1}),
            ("raw", {"dx": 20, "dy": 0}),
            ("frame", {"times": 1}),
            ("release", {}),
            ("frame", {"times": 2}),
        ],
    ),
]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="router_scenarios", description="录制 Router 行为场景")
    parser.add_argument("--out", default=str(ROOT / "tests" / "fixtures" / "router_scenarios.json"))
    args = parser.parse_args(argv)

    records = [record(name, script) for name, script in SCENARIOS]
    target = Path(args.out)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(records, ensure_ascii=False, indent=1), encoding="utf-8")
    print(f"[ok] {len(records)} 个场景 -> {target}")
    for item in records:
        expect = item["expect"]
        print(
            f"  {item['name']:<26} state={expect['state']:<9} "
            f"transitions={len(expect['transitions'])} mouse={len(expect['mouse_reports'])} "
            f"keys={len(expect['key_reports'])}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
