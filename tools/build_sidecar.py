"""把 ``service/`` 打成 Tauri 可随包分发的 sidecar exe。

用法（在装了 PyInstaller 的 venv 里）：

    .\\.venv\\Scripts\\python.exe -m pip install -r requirements-build.txt
    .\\.venv\\Scripts\\python.exe tools\\build_sidecar.py

产物：``src-tauri/binaries/padlend-service-x86_64-pc-windows-msvc.exe``。
Rust 侧 ``bridge::resolve_service`` 会优先在 exe 同目录 / resources 里找它；
找不到时退回 ``python -m service.server``（开发模式）。
"""

from __future__ import annotations

import contextlib
import platform
import subprocess
import sys
from pathlib import Path


def _force_utf8() -> None:
    """CI 的 PowerShell 默认 cp1252；中文输出不能让构建失败。"""
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if callable(reconfigure):
            with contextlib.suppress(Exception):
                reconfigure(encoding="utf-8", errors="replace")


ROOT = Path(__file__).resolve().parent.parent
OUT_DIR = ROOT / "src-tauri" / "binaries"
WORK_DIR = ROOT / "build" / "sidecar"

HIDDEN_IMPORTS = [
    "config",
    "constants",
    "main",
    "core.crash",
    "core.diagnostics",
    "core.errors",
    "core.events",
    "core.geometry",
    "core.logsetup",
    "core.metrics",
    "core.router",
    "core.state",
    "core",
    "win.cursor_watch",
    "win.desktop",
    "win.dpi",
    "win.hotkeys",
    "win.keyboard_hook",
    "win.mask",
    "win.rawinput",
    "win.single_instance",
    "win.trayprobe",
    "win",
    "tablet.adb",
    "tablet.cursor",
    "tablet.keymap",
    "tablet.locator",
    "tablet.mdns",
    "tablet.protocol",
    "tablet.server",
    "tablet.session",
    "tablet.simulated",
    "tablet.uhid",
    "tablet",
    "service.api",
    "service.serializers",
    "service.server",
    "service",
]


def target_triple() -> str:
    machine = (platform.machine() or "x86_64").lower()
    arch = "aarch64" if machine in {"arm64", "aarch64"} else "x86_64"
    return f"{arch}-pc-windows-msvc"


def main() -> int:
    _force_utf8()
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    WORK_DIR.mkdir(parents=True, exist_ok=True)
    name = f"padlend-service-{target_triple()}"
    cmd = [
        sys.executable,
        "-m",
        "PyInstaller",
        "--noconfirm",
        "--clean",
        "--onefile",
        "--console",  # 保留 stdio 管道；Rust 侧用 CREATE_NO_WINDOW 隐藏窗口
        "--name",
        name,
        "--distpath",
        str(OUT_DIR),
        "--workpath",
        str(WORK_DIR),
        "--specpath",
        str(WORK_DIR),
        "--paths",
        str(ROOT),
        "--collect-submodules",
        "tablet",
        "--collect-submodules",
        "core",
        "--collect-submodules",
        "win",
        "--collect-submodules",
        "service",
    ]
    for module in HIDDEN_IMPORTS:
        cmd.extend(["--hidden-import", module])
    jar = ROOT / "vendor" / "scrcpy-server-v5.0.1.jar"
    if jar.is_file():
        cmd.extend(["--add-data", f"{jar}{';' if sys.platform == 'win32' else ':'}vendor"])
    adb_dir = ROOT / "vendor" / "adb" / "platform-tools"
    if adb_dir.is_dir():
        cmd.extend(
            [
                "--add-data",
                f"{adb_dir}{';' if sys.platform == 'win32' else ':'}vendor/adb/platform-tools",
            ]
        )
    cmd.append(str(ROOT / "service" / "server.py"))
    print("构建 sidecar：", " ".join(cmd[:6]), "...")
    result = subprocess.run(cmd, cwd=ROOT, check=False)
    if result.returncode != 0:
        return result.returncode
    built = OUT_DIR / f"{name}.exe" if sys.platform == "win32" else OUT_DIR / name
    print(
        f"完成：{built}（{built.stat().st_size // 1024 // 1024} MB）"
        if built.is_file()
        else "未找到产物"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
