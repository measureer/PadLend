# -*- mode: python ; coding: utf-8 -*-
"""PyInstaller onedir spec for PadLend.

Build:
    .venv/Scripts/python.exe -m PyInstaller --noconfirm PadLend.spec

Output:
    dist/PadLend/PadLend.exe       (windowed bootloader)
    dist/PadLend/_internal/        (Qt/PySide6, vendor, licenses, notices)

onedir keeps the Qt DLLs as ordinary files on disk, which is what the
LGPLv3 "replaceable library" obligation requires (see THIRD_PARTY_NOTICES.md).
There is no runtime_tmpdir: nothing is extracted at launch, so the app works
from Program Files, shortcuts, scheduled tasks and read-only folders.

Version resource: generated from padlend.__version__ by tools/version_info.py
at build time - never type a version string in this file.
The shell rejects tray icons for low-integrity executables; see the note in
docs/RELEASE_READINESS.md if the build directory carries a Low mandatory
integrity label.
"""

import importlib.util
import shutil
import sys
from pathlib import Path

from PyInstaller.building.api import COLLECT
from PyInstaller.config import CONF

block_cipher = None
ROOT = Path(SPECPATH).resolve()
sys.path.insert(0, str(ROOT))


def _load_version_info():
    path = ROOT / "tools" / "version_info.py"
    spec = importlib.util.spec_from_file_location("padlend_build_version_info", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


version_info_module = _load_version_info()
VERSION_FILE = version_info_module.write_version_file(ROOT / "build" / "version_info.txt")

datas = [
    (str(ROOT / "vendor"), "vendor"),
    (str(ROOT / "ui" / "theme"), "ui/theme"),
    (str(ROOT / "THIRD_PARTY_NOTICES.md"), "."),
    (str(ROOT / "LICENSES"), "LICENSES"),
]

a = Analysis(
    ["main.py"],
    pathex=[str(ROOT)],
    binaries=[],
    datas=datas,
    hiddenimports=[],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=["tkinter", "unittest", "pydoc_data"],
    noarchive=False,
)

pyz = PYZ(a.pure, a.zipped_data, cipher=block_cipher)

exe = EXE(
    pyz,
    a.scripts,
    [],
    exclude_binaries=True,
    name="PadLend",
    icon=str(ROOT / "assets" / "padlend.ico"),
    version=str(VERSION_FILE),
    manifest=str(ROOT / "assets" / "padlend.manifest"),
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=False,
    upx_exclude=[],
    console=False,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)

class _CollectWithLegal(COLLECT):
    """COLLECT that also drops notices/LICENSES next to PadLend.exe.

    PyInstaller 6 puts all datas under _internal/.  The About dialog reads
    THIRD_PARTY_NOTICES.md via BUNDLE_DIR (_MEIPASS == _internal), but users
    (and zip/installer listings) expect the legal files at the app root, so
    they are copied there after COLLECT finishes.
    """

    def assemble(self):
        toc = super().assemble()
        target = Path(CONF["distpath"]) / "PadLend"
        shutil.copy2(ROOT / "THIRD_PARTY_NOTICES.md", target / "THIRD_PARTY_NOTICES.md")
        shutil.copytree(ROOT / "LICENSES", target / "LICENSES", dirs_exist_ok=True)
        return toc


coll = _CollectWithLegal(
    exe,
    a.binaries,
    a.datas,
    strip=False,
    upx=False,
    upx_exclude=[],
    name="PadLend",
)
