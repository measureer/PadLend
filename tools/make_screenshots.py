r"""Generate README screenshots from the real MainWindow (offscreen).

Usage:
    .venv\Scripts\python.exe tools\make_screenshots.py

The script never needs a tablet, adb or a desktop: it injects the same fake
Router/DesktopMap used by tests/test_ui_smoke.py.  Device facts mirror the
project's verified tablet (Honor MagicPad 3 / CGA-W00, 3200x2136, Android 17)
so the pages show realistic values; the device row itself is simulated and
the README captions say so ("界面示意").

Outputs (overwritten in place):
    docs/screenshots/ui-devices.png   1280x820  device page, dark
    docs/screenshots/ui-layout.png    1280x820  layout page, dark
    docs/screenshots/ui-settings.png  1280x820  settings page, dark
    docs/screenshots/ui-light.png    1280x820  device page, light
"""

from __future__ import annotations

import os
import sys
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from PySide6.QtWidgets import QApplication  # noqa: E402

from config import Config  # noqa: E402
from core.geometry import Edge, EdgeSegment, MonitorInfo, Placement, Rect  # noqa: E402
from core.state import State  # noqa: E402
from tablet.adb import DeviceEntry  # noqa: E402
from tablet.session import DeviceFacts  # noqa: E402
from ui import main_window as mw  # noqa: E402
from ui.style import apply_theme  # noqa: E402
from ui.theme import tokens  # noqa: E402

SIZE = tokens.WINDOW_DEFAULT_SIZE
DEVICE = DeviceFacts(
    serial="CGA-W00",
    model="HONOR CGA-W00",
    width=3200,
    height=2136,
    orientation=1,
    xscale=1.312,
    pointer_speed=-3,
)
ENTRY = DeviceEntry(
    serial="CGA-W00",
    state="device",
    product="CGA-W00",
    model="HONOR CGA-W00",
    device="CGA-W00",
    transport_id="1",
)


class FakeRouter:
    """Router public protocol subset (same shape as tests/test_ui_smoke.py)."""

    def __init__(self) -> None:
        self.on_state_changed: Any = None
        self.on_metrics: Any = None
        self.on_orientation_changed: Any = None

    def start(self) -> None:
        pass

    def stop(self) -> None:
        pass

    def on_raw(self, data: Any) -> None:
        pass

    def on_key(self, event: Any) -> None:
        pass

    def toggle_manual(self) -> None:
        pass

    def set_placement(self, placement: Placement) -> None:
        pass

    def resync_pointer(self) -> None:
        pass

    def inject_clipboard(self, text: str) -> bool:
        return True


class FakeDesktop:
    """DesktopMap subset: one 1920x1080 monitor, right/left exterior edges."""

    def __init__(self) -> None:
        self._monitors: tuple[MonitorInfo, ...] = (
            MonitorInfo(
                handle=1,
                device="\\\\.\\DISPLAY1",
                rect=Rect(0, 0, 1920, 1080),
                dpi=96,
                primary=True,
            ),
        )
        self._segments: tuple[EdgeSegment, ...] = (
            EdgeSegment(1, Edge.RIGHT, 0, 1080),
            EdgeSegment(1, Edge.LEFT, 0, 1080),
        )

    @property
    def monitors(self) -> tuple[MonitorInfo, ...]:
        return self._monitors

    @property
    def bounding_box(self) -> Rect:
        """Virtual desktop extent: one 1920x1080 monitor."""
        return Rect(0, 0, 1920, 1080)

    def exterior_segments(self) -> tuple[EdgeSegment, ...]:
        return self._segments


def _pump(app: QApplication) -> None:
    for _ in range(3):
        app.processEvents()


def _make_window(app: QApplication, config_path: Path) -> mw.MainWindow:
    window = mw.MainWindow(Config(), FakeRouter(), FakeDesktop(), config_path=config_path)
    window.resize(*SIZE)
    window.show()
    _pump(app)
    return window


def _simulate_connected(window: mw.MainWindow, app: QApplication) -> None:
    window.device_page.set_known([DEVICE.serial])
    window.device_page._show_entries([ENTRY], "")  # noqa: SLF001 - render only
    window.update_device(DEVICE)
    window._on_state(None, State.IDLE, "USB")  # noqa: SLF001 - status pill
    _pump(app)


def _grab(window: mw.MainWindow, path: Path, app: QApplication) -> None:
    _pump(app)
    pixmap = window.grab()
    if pixmap.isNull():
        raise RuntimeError(f"grab() returned a null pixmap for {path.name}")
    path.parent.mkdir(parents=True, exist_ok=True)
    if not pixmap.save(str(path), "PNG"):
        raise RuntimeError(f"failed to save {path}")
    print(f"{path.name}: {pixmap.width()}x{pixmap.height()}")


FONT_CANDIDATES = (
    Path(r"C:\Windows\Fonts\msyh.ttc"),
    Path(r"C:\Windows\Fonts\msyhbd.ttc"),
    Path(r"C:\Windows\Fonts\consola.ttf"),
    Path(r"C:\Windows\Fonts\consolab.ttf"),
    Path(r"C:\Windows\Fonts\seguiemj.ttf"),
    Path(r"C:\Windows\Fonts\seguisym.ttf"),
    Path(r"C:\Windows\Fonts\segoeui.ttf"),
)


def _install_fonts() -> None:
    """Offscreen Qt ships no fonts; register Windows fonts explicitly."""
    from PySide6.QtGui import QFontDatabase

    loaded = 0
    for path in FONT_CANDIDATES:
        if not path.exists():
            continue
        font_id = QFontDatabase.addApplicationFont(str(path))
        if font_id >= 0:
            loaded += 1
        else:
            print(f"warning: failed to register {path}", file=sys.stderr)
    families = QFontDatabase.families()
    if not families:
        raise RuntimeError("no fonts available; text would render as boxes")
    print(f"fonts: {loaded} files, {len(families)} families")


def main() -> int:
    out = ROOT / "docs" / "screenshots"
    app = QApplication.instance() or QApplication([])
    _install_fonts()
    apply_theme(app, "dark")

    with TemporaryDirectory(prefix="padlend-shots-") as tmp:
        dark_cfg = Path(tmp) / "dark.yaml"
        window = _make_window(app, dark_cfg)
        _simulate_connected(window, app)

        window._switch_page(0)  # noqa: SLF001
        _simulate_connected(window, app)  # refresh() cleared the fake rows
        _grab(window, out / "ui-devices.png", app)

        window._switch_page(1)  # noqa: SLF001
        _pump(app)
        window.canvas.fit_view()
        _pump(app)
        _grab(window, out / "ui-layout.png", app)

        window._switch_page(2)  # noqa: SLF001
        _pump(app)
        _grab(window, out / "ui-settings.png", app)

        window._force_quit = True  # noqa: SLF001 - bypass close-to-tray
        window.close()
        window.deleteLater()
        _pump(app)

        apply_theme(app, "light")
        light_cfg = Path(tmp) / "light.yaml"
        light = _make_window(app, light_cfg)
        _simulate_connected(light, app)
        light._switch_page(0)  # noqa: SLF001
        _simulate_connected(light, app)
        _grab(light, out / "ui-light.png", app)
        light._force_quit = True  # noqa: SLF001
        light.close()
        light.deleteLater()
        _pump(app)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
