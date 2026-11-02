"""剪贴板注入（6.8，SHOULD）：大小限制与开关。"""

from __future__ import annotations

from core.router import Router
from tests.fakes import FakeConfig, FakeDesktop, FakeMask, FakeSession


class _RecordingSession(FakeSession):
    def __init__(self) -> None:
        super().__init__()
        self.texts: list[str] = []

    def inject_text(self, text: str) -> None:
        self.texts.append(text)


def _router(session: FakeSession, *, enabled: bool = True) -> Router:
    cfg = FakeConfig()
    cfg.misc.clipboard_sync = enabled
    return Router(cfg, session, FakeDesktop(), FakeMask())


def test_inject_clipboard_truncates_to_4kb() -> None:
    session = _RecordingSession()
    router = _router(session)
    assert router.inject_clipboard("a" * 5000) is True
    assert len(session.texts[0].encode("utf-8")) == 4096


def test_inject_clipboard_respects_switch() -> None:
    session = _RecordingSession()
    router = _router(session, enabled=False)
    assert router.inject_clipboard("hello") is False
    assert session.texts == []


def test_inject_clipboard_ignores_empty() -> None:
    session = _RecordingSession()
    router = _router(session)
    assert router.inject_clipboard("") is False
    assert session.texts == []
