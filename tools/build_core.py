r"""把 padlend-core 编译并放到 src-tauri/binaries，供 A/B 切换使用。

产物名带 target triple（Tauri externalBin 的约定）：
  src-tauri/binaries/padlend-core-x86_64-pc-windows-msvc.exe

用法：
    .\.venv\Scripts\python.exe tools\build_core.py            # 构建并复制
    .\.venv\Scripts\python.exe tools\build_core.py --check    # 只检查已有产物

注意：这个脚本**不**修改 tauri.conf.json；是否把 Rust sidecar 打进安装包
（externalBin/bundle.resources）属于发布决策，见 docs/B2_M5_REPORT.md。
"""

from __future__ import annotations

import argparse
import hashlib
import platform
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATE = ROOT / "padlend-core"
OUT_DIR = ROOT / "src-tauri" / "binaries"


def target_triple() -> str:
    machine = (platform.machine() or "x86_64").lower()
    arch = "aarch64" if machine in {"arm64", "aarch64"} else "x86_64"
    return f"{arch}-pc-windows-msvc"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="build_core", description="构建 padlend-core 并放到 binaries")
    parser.add_argument("--check", action="store_true", help="只检查产物是否存在")
    parser.add_argument("--debug", action="store_true", help="用 debug 构建（默认 release）")
    args = parser.parse_args(argv)

    profile = "debug" if args.debug else "release"
    built = CRATE / "target" / profile / "padlend-core.exe"
    target = OUT_DIR / f"padlend-core-{target_triple()}.exe"

    if not args.check:
        # 构建前先收尸：正在运行的 sidecar 会锁住 exe，cargo 链接会失败。
        subprocess.run(
            ["powershell", "-NoProfile", "-Command",
             "Get-Process padlend-core -ErrorAction SilentlyContinue | Stop-Process -Force"],
            check=False,
        )
        code = subprocess.run(["cargo", "build", *([] if args.debug else ["--release"])],
                              cwd=str(CRATE), check=False).returncode
        if code != 0:
            print(f"[error] cargo build 失败（exit {code}）", file=sys.stderr)
            return code
        OUT_DIR.mkdir(parents=True, exist_ok=True)
        shutil.copy2(built, target)
        print(f"[ok] {target}（{target.stat().st_size / 1024 / 1024:.2f} MB）")

    if not target.is_file():
        print(f"[error] 没有产物：{target}（先跑一次不带 --check）", file=sys.stderr)
        return 2
    digest = hashlib.sha256(target.read_bytes()).hexdigest()
    print(f"[check] {target.name}  {target.stat().st_size / 1024 / 1024:.2f} MB  sha256={digest[:16]}…")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
