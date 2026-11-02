"""Normalize scrcpy-server jar for Android 17.

Android 17's app_process/ART refuses to load a compressed ``classes.dex`` entry
from CLASSPATH (java.lang.ClassNotFoundException), so the official jar is
repacked with ``classes.dex`` stored uncompressed.  Code is unchanged.

Usage: python tools/repack_server_jar.py <input.jar> <output.jar>
"""

from __future__ import annotations

import sys
import zipfile
from pathlib import Path


def repack(src: Path, dst: Path) -> None:
    with zipfile.ZipFile(src) as zin, zipfile.ZipFile(dst, "w") as zout:
        for info in zin.infolist():
            data = zin.read(info.filename)
            entry = zipfile.ZipInfo(info.filename, date_time=info.date_time)
            entry.external_attr = info.external_attr
            entry.compress_type = (
                zipfile.ZIP_STORED
                if info.filename == "classes.dex"
                else zipfile.ZIP_DEFLATED
            )
            zout.writestr(entry, data)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    repack(Path(sys.argv[1]), Path(sys.argv[2]))
