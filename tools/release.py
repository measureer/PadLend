"""PadLend 1.1+ 发布脚本：前端 / sidecar / Tauri 外壳的构建、打包与校验和。

用法：

    .\\.venv\\Scripts\\python.exe tools\\release.py                # 全部
    .\\.venv\\Scripts\\python.exe tools\\release.py --build --zip --sha256
    .\\.venv\\Scripts\\python.exe tools\\release.py --zip --sha256  # 只打包已有产物

与 1.0.x 的区别：不再使用 PyInstaller 打包 Qt 主程序，也不再用 Inno Setup。
Tauri 自己产出 NSIS 安装器；本脚本负责前端静态资源、Python 业务 sidecar、
免安装 zip 与 SHA256 校验和。没有 Rust 工具链时会跳过原生外壳并明确提示。
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import os
import shutil
import subprocess
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / "dist"
FRONTEND = ROOT / "frontend-dist"
BINARIES = ROOT / "src-tauri" / "binaries"
TAURI_TARGET = ROOT / "src-tauri" / "target" / "release"
APP_NAME = "PadLend"

sys.path.insert(0, str(ROOT))
from padlend import __version__ as VERSION  # noqa: E402


def _force_utf8() -> None:
    """CI 的 PowerShell 默认 cp1252；中文输出不能让发版失败。"""
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if callable(reconfigure):
            with contextlib.suppress(Exception):
                reconfigure(encoding="utf-8", errors="replace")


def log(message: str) -> None:
    print(f"[release] {message}", flush=True)


def fail(message: str) -> int:
    print(f"[release] ERROR: {message}", file=sys.stderr, flush=True)
    return 1


def run(command: list[str], *, cwd: Path = ROOT) -> int:
    log("执行：" + " ".join(command))
    # Windows 上 npm/npx 是 .cmd 垫片，CreateProcess 无法直接执行，
    # 必须经 cmd.exe；命令全部为本脚本硬编码，不含外部输入。
    result = subprocess.run(command, cwd=cwd, check=False, shell=(os.name == "nt"))
    if result.returncode != 0:
        log(f"失败（exit {result.returncode}）：{' '.join(command)}")
    return result.returncode


def has_cargo() -> bool:
    return shutil.which("cargo") is not None


def sidecar_name() -> str:
    import platform

    arch = "aarch64" if (platform.machine() or "").lower() in {"arm64", "aarch64"} else "x86_64"
    suffix = ".exe" if os.name == "nt" else ""
    return f"padlend-service-{arch}-pc-windows-msvc{suffix}"


def build() -> int:
    if not (ROOT / "node_modules").is_dir() and run(["npm", "ci"]) != 0:
        return 1
    if run(["npm", "run", "build"]) != 0:
        return 1
    if not (FRONTEND / "index.html").is_file():
        return fail("frontend-dist/index.html 不存在")
    log(f"前端产物：{FRONTEND}")

    if run([sys.executable, str(ROOT / "tools" / "build_sidecar.py")]) != 0:
        return 1
    sidecar = BINARIES / sidecar_name()
    if not sidecar.is_file():
        return fail(f"sidecar 未生成：{sidecar}")
    log(f"业务 sidecar：{sidecar.name}（{sidecar.stat().st_size // 1024 // 1024} MB）")

    # Rust 引擎（B2）：与 Python sidecar 一起进包，外壳默认优先用它；
    # PADLEND_ENGINE=python 可在运行时回退，无需重新打包。
    if run([sys.executable, str(ROOT / "tools" / "build_core.py")]) != 0:
        return fail("padlend-core 构建失败")
    core = BINARIES / "padlend-core-x86_64-pc-windows-msvc.exe"
    if not core.is_file():
        return fail(f"Rust sidecar 未生成：{core}")
    log(f"Rust sidecar：{core.name}（{core.stat().st_size / 1024 / 1024:.2f} MB）")

    if not has_cargo():
        log("本机没有 cargo，跳过 Tauri 原生外壳（CI 会构建并发布安装包）")
        return 0
    if run(["npx", "tauri", "build"]) != 0:
        return 1
    return 0


def collect_files() -> list[tuple[Path, str]]:
    """返回 (源文件, zip 内路径)；不存在的文件自动跳过。"""
    items: list[tuple[Path, str]] = []
    exe = TAURI_TARGET / "padlend-desktop.exe"
    if exe.is_file():
        items.append((exe, f"{APP_NAME}/{APP_NAME}.exe"))
    sidecar = BINARIES / sidecar_name()
    if sidecar.is_file():
        items.append((sidecar, f"{APP_NAME}/{sidecar.name}"))

    jar = ROOT / "vendor" / "scrcpy-server-v5.0.1.jar"
    if jar.is_file():
        items.append((jar, f"{APP_NAME}/vendor/{jar.name}"))
    adb_dir = ROOT / "vendor" / "adb" / "platform-tools"
    if adb_dir.is_dir():
        for path in sorted(adb_dir.rglob("*")):
            if path.is_file():
                items.append(
                    (
                        path,
                        f"{APP_NAME}/vendor/adb/platform-tools/{path.relative_to(adb_dir).as_posix()}",
                    )
                )

    for name in ("README.md", "LICENSE", "THIRD_PARTY_NOTICES.md"):
        path = ROOT / name
        if path.is_file():
            items.append((path, f"{APP_NAME}/{name}"))
    licenses = ROOT / "LICENSES"
    if licenses.is_dir():
        for path in sorted(licenses.rglob("*")):
            if path.is_file():
                items.append((path, f"{APP_NAME}/LICENSES/{path.relative_to(licenses).as_posix()}"))
    return items


def make_zip() -> int:
    DIST.mkdir(parents=True, exist_ok=True)
    items = collect_files()
    if not items:
        return fail("没有可打包的产物，请先运行 --build")
    native = any(target.endswith("PadLend.exe") for _, target in items)
    basename = f"{APP_NAME}-{VERSION}-win64" if native else f"{APP_NAME}-{VERSION}-bundle"
    target = DIST / f"{basename}.zip"
    with zipfile.ZipFile(target, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
        seen: set[str] = set()
        for source, arcname in items:
            if arcname in seen:
                continue
            seen.add(arcname)
            archive.write(source, arcname)
    log(f"打包：{target.name}（{target.stat().st_size // 1024 // 1024} MB，{len(items)} 个文件）")
    bundle = ROOT / "src-tauri" / "target" / "release" / "bundle" / "nsis"
    if bundle.is_dir():
        for installer in bundle.glob("*-setup.exe"):
            shutil.copy2(installer, DIST / f"{APP_NAME}-{VERSION}-setup.exe")
            log(f"安装器：{APP_NAME}-{VERSION}-setup.exe")
    return 0


def make_sha256() -> int:
    DIST.mkdir(parents=True, exist_ok=True)
    files = sorted(p for p in DIST.iterdir() if p.is_file() and p.name != "SHA256SUMS.txt")
    if not files:
        return fail("dist 里没有产物")
    lines = []
    for path in files:
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        lines.append(f"{digest}  {path.name}")
        log(f"sha256 {digest[:16]}…  {path.name}")
    (DIST / "SHA256SUMS.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return 0


def main(argv: list[str] | None = None) -> int:
    _force_utf8()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build", action="store_true", help="构建前端、sidecar 与 Tauri 外壳")
    parser.add_argument("--zip", action="store_true", help="生成免安装 zip")
    parser.add_argument("--sha256", action="store_true", help="生成 SHA256SUMS.txt")
    args = parser.parse_args(argv)
    if not any(vars(args).values()):
        args.build = args.zip = args.sha256 = True

    steps = []
    if args.build:
        steps.append(build)
    if args.zip:
        steps.append(make_zip)
    if args.sha256:
        steps.append(make_sha256)
    for step in steps:
        code = step()
        if code:
            return code
    log(f"完成：PadLend {VERSION}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
