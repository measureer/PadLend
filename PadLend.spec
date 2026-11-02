# -*- mode: python ; coding: utf-8 -*-
"""PyInstaller onefile spec for PadLend.

Build:
    .\\.venv\\Scripts\\python.exe -m PyInstaller --noconfirm PadLend.spec

Output: dist/PadLend.exe (single executable).

The onefile bootloader extracts next to the exe (runtime_tmpdir=".") instead
of %TEMP%; this keeps the portable app working in restricted Windows profiles
where the system temp directory is not writable.
"""

from pathlib import Path

block_cipher = None
ROOT = Path(SPECPATH)

datas = [
    (str(ROOT / "vendor"), "vendor"),
    (str(ROOT / "ui" / "theme"), "ui/theme"),
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
    a.binaries,
    a.datas,
    [],
    name="PadLend",
    icon="assets/padlend.ico",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=False,
    upx_exclude=[],
    runtime_tmpdir=".",
    console=False,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)
