"""Generate the Windows version resource (VSVersionInfo) for PadLend.

The single source of truth for the version is padlend.__version__
(padlend/__init__.py). This module is imported from PadLend.spec at
build time and writes build/version_info.txt for PyInstaller's version=
argument, so no version string is ever duplicated by hand.

Usage:
    .venv/Scripts/python.exe tools/version_info.py [output]

With no output argument it writes build/version_info.txt under the
repository root.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

COMPANY_NAME = "PadLend contributors"
COPYRIGHT = "Copyright (c) 2026 PadLend contributors. Licensed under Apache-2.0."
FILE_DESCRIPTION = "PadLend - Android tablet as a Windows second input screen"
PRODUCT_NAME = "PadLend"
INTERNAL_NAME = "PadLend"
ORIGINAL_FILENAME = "PadLend.exe"


def version_tuple(version: str) -> tuple[int, int, int, int]:
    """Return a 4-int Windows version tuple from a PEP 440-ish version."""
    parts = re.findall(r"\d+", version.split("+")[0].split("dev")[0])
    if not parts:
        raise ValueError(f"cannot parse version {version!r}")
    numbers = [int(p) for p in parts[:4]]
    numbers += [0] * (4 - len(numbers))
    return tuple(numbers)  # type: ignore[return-value]


def build_version_info(version: str):
    """Build a PyInstaller VSVersionInfo for the given version."""
    from PyInstaller.utils.win32.versioninfo import (
        FixedFileInfo,
        StringFileInfo,
        StringStruct,
        StringTable,
        VarFileInfo,
        VarStruct,
        VSVersionInfo,
    )

    numbers = version_tuple(version)
    file_version = ".".join(str(n) for n in numbers)
    product_version = version  # keep the public version readable in Explorer

    return VSVersionInfo(
        ffi=FixedFileInfo(
            filevers=numbers,
            prodvers=numbers,
            mask=0x3F,
            flags=0x0,
            OS=0x40004,
            fileType=0x1,
            subtype=0x0,
            date=(0, 0),
        ),
        kids=[
            StringFileInfo(
                [
                    StringTable(
                        "040904B0",
                        [
                            StringStruct("CompanyName", COMPANY_NAME),
                            StringStruct("FileDescription", FILE_DESCRIPTION),
                            StringStruct("FileVersion", file_version),
                            StringStruct("InternalName", INTERNAL_NAME),
                            StringStruct("LegalCopyright", COPYRIGHT),
                            StringStruct("OriginalFilename", ORIGINAL_FILENAME),
                            StringStruct("ProductName", PRODUCT_NAME),
                            StringStruct("ProductVersion", product_version),
                        ],
                    )
                ]
            ),
            VarFileInfo([VarStruct("Translation", [0x0409, 1200])]),
        ],
    )


def write_version_file(dest=None):
    """Write the version resource to dest (default build/version_info.txt)."""
    sys.path.insert(0, str(ROOT))
    from padlend import __version__  # imported late so the module works standalone

    target = Path(dest) if dest else ROOT / "build" / "version_info.txt"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(str(build_version_info(__version__)), encoding="utf-8")
    return target


def main() -> int:
    target = write_version_file(Path(sys.argv[1]) if len(sys.argv) > 1 else None)
    print(target)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
