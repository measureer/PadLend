"""1.1.0 发版表面测试：版本单源、无 Qt 残留、发布资源齐备。

旧版同名文件用 PySide6 检查主题令牌/托盘/关于对话框；界面迁移到 Tauri 后
改为纯文件契约检查，不再依赖任何 GUI 运行时。
"""

from __future__ import annotations

import json
import re
import tomllib
from pathlib import Path

import constants as C

ROOT = Path(__file__).resolve().parents[1]


def test_version_has_a_single_python_source() -> None:
    import padlend

    assert padlend.__version__ == C.VERSION
    project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))
    assert "version" not in project["project"], "版本只能来自 padlend.__version__"
    assert "version" in project["project"]["dynamic"]
    assert project["tool"]["setuptools"]["dynamic"]["version"]["attr"] == "padlend.__version__"


def test_all_manifests_share_the_same_version() -> None:
    import padlend

    version = padlend.__version__
    package = json.loads((ROOT / "package.json").read_text(encoding="utf-8"))
    assert package["version"] == version
    cargo = (ROOT / "src-tauri" / "Cargo.toml").read_text(encoding="utf-8")
    assert re.search(r'^version = "' + re.escape(version) + r'"', cargo, re.M)
    tauri = json.loads((ROOT / "src-tauri" / "tauri.conf.json").read_text(encoding="utf-8"))
    assert tauri["version"] == version


def test_no_qt_dependency_remains() -> None:
    for relative in ("pyproject.toml", "requirements.txt", "requirements-build.txt"):
        text = (ROOT / relative).read_text(encoding="utf-8")
        assert "PySide6" not in text
        assert "shiboken" not in text
    assert "PySide6" not in (ROOT / "main.py").read_text(encoding="utf-8")
    assert "PySide6" not in (ROOT / "core" / "crash.py").read_text(encoding="utf-8")
    assert not (ROOT / "ui").exists(), "旧 Qt 界面目录应已删除"


def test_release_assets_exist() -> None:
    icons = ROOT / "src-tauri" / "icons"
    for name in ("32x32.png", "128x128.png", "128x128@2x.png", "icon.png", "icon.ico"):
        assert (icons / name).is_file(), f"缺少 Tauri 图标 {name}"
    assert (ROOT / "src-tauri" / "tauri.conf.json").is_file()
    assert (ROOT / "tools" / "build_sidecar.py").is_file()
    assert (ROOT / "service" / "server.py").is_file()
    assert (ROOT / "vendor" / "scrcpy-server-v5.0.1.jar").is_file()
