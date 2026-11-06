"""PadLend release helper: build, zip, checksums, signing, installer.

Standard library only. Run from anywhere:

    .venv/Scripts/python.exe tools/release.py --build --zip --sha256
    .venv/Scripts/python.exe tools/release.py --zip --sha256
    .venv/Scripts/python.exe tools/release.py --sign
    .venv/Scripts/python.exe tools/release.py --installer

Every flag is optional and composable; steps run in the order build, zip,
installer, sign, sha256. Exit code is non-zero when a requested step fails.

Environment variables used by --sign:
    PADLEND_SIGN_CERT      path to a .pfx/.p12 code-signing certificate
    PADLEND_SIGN_PASSWORD  its password
If PADLEND_SIGN_CERT is unset the step is skipped (release notes should say
the artifact is unsigned).
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / "dist"
APP_NAME = "PadLend"
TIMESTAMP_URL = "http://timestamp.digicert.com"

sys.path.insert(0, str(ROOT))
from padlend import __version__ as VERSION  # noqa: E402

RELEASE_BASENAME = f"{APP_NAME}-{VERSION}-win64"


def log(message: str) -> None:
    print(f"[release] {message}", flush=True)


def fail(message: str) -> int:
    print(f"[release] ERROR: {message}", file=sys.stderr, flush=True)
    return 1


def app_dir() -> Path:
    return DIST / APP_NAME


def ensure_legal_files() -> None:
    """Make sure notices/LICENSES also sit next to PadLend.exe (not only _internal)."""
    target = app_dir()
    if not target.is_dir():
        return
    shutil.copy2(ROOT / "THIRD_PARTY_NOTICES.md", target / "THIRD_PARTY_NOTICES.md")
    shutil.copytree(ROOT / "LICENSES", target / "LICENSES", dirs_exist_ok=True)


def require_built() -> Path | None:
    exe = app_dir() / f"{APP_NAME}.exe"
    if not exe.is_file():
        fail(f"missing {exe}; run --build first")
        return None
    return exe


def build() -> int:
    log(f"building {APP_NAME} {VERSION} (onedir) with PyInstaller")
    command = [sys.executable, "-m", "PyInstaller", "--noconfirm", "--clean", "PadLend.spec"]
    result = subprocess.run(command, cwd=ROOT)
    if result.returncode != 0:
        return fail(f"PyInstaller failed with exit code {result.returncode}")
    exe = require_built()
    if exe is None:
        return 1
    ensure_legal_files()
    size = sum(p.stat().st_size for p in app_dir().rglob("*") if p.is_file())
    log(f"built {exe} ({size / 1024 / 1024:.1f} MiB total)")
    return 0


def _zip_add(archive: zipfile.ZipFile, source: Path, arcname: Path, seen: set[str]) -> None:
    name = arcname.as_posix()
    if name in seen:
        return
    seen.add(name)
    archive.write(source, name)


def _zip_add_tree(
    archive: zipfile.ZipFile, source: Path, prefix: str, seen: set[str]
) -> None:
    for path in sorted(source.rglob("*")):
        if path.is_file():
            _zip_add(archive, path, Path(prefix) / path.relative_to(source), seen)


def make_zip() -> int:
    if require_built() is None:
        return 1
    ensure_legal_files()
    target = DIST / f"{RELEASE_BASENAME}.zip"
    notices = ROOT / "THIRD_PARTY_NOTICES.md"
    licenses = ROOT / "LICENSES"
    readme = ROOT / "README.md"
    for required in (notices, licenses, readme):
        if not required.exists():
            return fail(f"missing {required}")

    if target.exists():
        target.unlink()
    log(f"creating {target.name}")
    seen: set[str] = set()
    with zipfile.ZipFile(target, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        _zip_add_tree(archive, app_dir(), "", seen)
        _zip_add(archive, notices, Path("THIRD_PARTY_NOTICES.md"), seen)
        _zip_add(archive, readme, Path("README.md"), seen)
        _zip_add_tree(archive, licenses, "LICENSES", seen)
    log(f"zip: {target} ({target.stat().st_size / 1024 / 1024:.1f} MiB)")
    return 0


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def make_sha256() -> int:
    files = [DIST / f"{RELEASE_BASENAME}.zip"]
    setup = DIST / f"{APP_NAME}-{VERSION}-setup.exe"
    if setup.exists():
        files.append(setup)
    missing = [p for p in files if not p.exists()]
    if missing:
        return fail("nothing to hash: " + ", ".join(str(p) for p in missing))
    sums = DIST / "SHA256SUMS.txt"
    lines = []
    for path in files:
        digest = sha256_file(path)
        lines.append(f"{digest}  {path.name}")
        log(f"sha256 {digest}  {path.name}")
    sums.write_text("\n".join(lines) + "\n", encoding="ascii")
    log(f"wrote {sums}")
    return 0


def find_signtool() -> str | None:
    which = shutil.which("signtool")
    if which:
        return which
    program_files_x86 = os.environ.get("PROGRAMFILES(X86)") or r"C:\Program Files (x86)"
    kits = Path(program_files_x86) / "Windows Kits" / "10" / "bin"
    if kits.is_dir():
        candidates = sorted(kits.glob("*/x64/signtool.exe"), reverse=True)
        if candidates:
            return str(candidates[0])
    return None


def sign(include_exe: bool = True, include_setup: bool = True) -> int:
    cert = os.environ.get("PADLEND_SIGN_CERT")
    if not cert:
        log("PADLEND_SIGN_CERT not set - skipping signing (artifact stays unsigned)")
        return 0
    certificate = Path(cert)
    if not certificate.is_file():
        return fail(f"PADLEND_SIGN_CERT does not exist: {certificate}")
    signtool = find_signtool()
    if not signtool:
        return fail("signtool.exe not found (install Windows SDK)")
    targets = []
    if include_exe:
        targets.append(app_dir() / f"{APP_NAME}.exe")
    setup = DIST / f"{APP_NAME}-{VERSION}-setup.exe"
    if include_setup and setup.exists():
        targets.append(setup)
    if not targets:
        log("no signing target present - skipping")
        return 0
    for target in targets:
        command = [
            signtool, "sign", "/fd", "SHA256",
            "/tr", TIMESTAMP_URL, "/td", "SHA256",
            "/f", str(certificate),
        ]
        password = os.environ.get("PADLEND_SIGN_PASSWORD")
        if password:
            command += ["/p", password]
        command.append(str(target))
        log(f"signing {target.name}")
        if subprocess.run(command, cwd=ROOT).returncode != 0:
            return fail(f"signtool failed for {target.name}")
    return 0


def find_iscc() -> str | None:
    which = shutil.which("iscc")
    if which:
        return which
    # winget installs Inno Setup per-user by default when --scope user is used.
    local = os.environ.get("LOCALAPPDATA")
    if local:
        candidate = Path(local) / "Programs" / "Inno Setup 6" / "ISCC.exe"
        if candidate.is_file():
            return str(candidate)
    for base in (os.environ.get("PROGRAMFILES(X86)"), os.environ.get("PROGRAMFILES")):
        if not base:
            continue
        candidate = Path(base) / "Inno Setup 6" / "ISCC.exe"
        if candidate.is_file():
            return str(candidate)
    return None


def make_installer() -> int:
    iscc = find_iscc()
    if not iscc:
        log("Inno Setup (ISCC.exe) not found - skipping installer")
        return 0
    script = ROOT / "installer" / "padlend.iss"
    if not script.is_file():
        return fail(f"missing {script}")
    setup = DIST / f"{APP_NAME}-{VERSION}-setup.exe"
    if setup.exists():
        setup.unlink()
    command = [
        iscc, str(script),
        f"/DAppVersion={VERSION}",
        f"/O{DIST}",
        f"/F{APP_NAME}-{VERSION}-setup",
    ]
    log(f"building installer {setup.name}")
    if subprocess.run(command, cwd=ROOT).returncode != 0:
        return fail("ISCC failed")
    if not setup.is_file():
        return fail(f"installer not produced: {setup}")
    log(f"installer: {setup} ({setup.stat().st_size / 1024 / 1024:.1f} MiB)")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--build", action="store_true", help="run PyInstaller --noconfirm PadLend.spec"
    )
    parser.add_argument("--zip", action="store_true", help="create dist/PadLend-<ver>-win64.zip")
    parser.add_argument("--sha256", action="store_true", help="write dist/SHA256SUMS.txt")
    parser.add_argument(
        "--sign", action="store_true", help="sign exe/installer with PADLEND_SIGN_CERT"
    )
    parser.add_argument("--installer", action="store_true", help="build Inno Setup installer")
    args = parser.parse_args(argv)
    if not any(vars(args).values()):
        parser.print_help()
        return 0

    steps = []
    if args.build:
        steps.append(build)
    # Sign the app before packaging so the zip and installer carry a signed exe;
    # the installer itself is signed afterwards.
    if args.sign:
        steps.append(lambda: sign(include_exe=True, include_setup=False))
    if args.zip:
        steps.append(make_zip)
    if args.installer:
        steps.append(make_installer)
    if args.sign and args.installer:
        steps.append(lambda: sign(include_exe=False, include_setup=True))
    if args.sha256:
        steps.append(make_sha256)
    for step in steps:
        code = step()
        if code:
            return code
    log("done")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
