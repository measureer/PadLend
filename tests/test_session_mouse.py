"""鼠标报告日志：按下/抬起必须在日志里留下痕迹（便于定位平板误触）。"""

from __future__ import annotations

import logging

from tablet.session import TabletSession
from tablet.uhid import mouse_report


def _bare_session() -> tuple[TabletSession, list[bytes]]:
    session = object.__new__(TabletSession)
    session._last_mouse_buttons = None
    sent: list[bytes] = []
    session._ensure_connected = lambda: None  # type: ignore[method-assign]
    session._send = lambda payload: sent.append(payload)  # type: ignore[method-assign]
    return session, sent


def test_send_mouse_logs_only_on_button_change(caplog) -> None:
    session, sent = _bare_session()
    with caplog.at_level(logging.INFO, logger="tablet.session"):
        session.send_mouse(0, 3, 4, 0)
        session.send_mouse(0, 1, 1, 0)
        session.send_mouse(1, 0, 0, 0)
        session.send_mouse(0, 0, 0, 0)

    lines = [r.getMessage() for r in caplog.records if "tablet.mouse.btn" in r.getMessage()]
    assert lines == [
        "tablet.mouse.btn 0x00 dx=3 dy=4",
        "tablet.mouse.btn 0x01 dx=0 dy=0",
        "tablet.mouse.btn 0x00 dx=0 dy=0",
    ]
    assert len(sent) == 4


def test_send_mouse_masks_buttons_to_five_bits() -> None:
    # Raw Input 的滚轮/按键标志位不能原样落进 HID 报告，否则平板会看到幽灵按下。
    assert mouse_report(0x0400, 0, 0, 0)[0] == 0
    assert mouse_report(0x01, 0, 0, 0)[0] == 0x01
