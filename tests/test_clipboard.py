"""剪贴板同步（6.8，SHOULD）：大小限制、开关与协议选择。"""

from __future__ import annotations

from core.router import Router
from tests.fakes import FakeConfig, FakeDesktop, FakeMask, FakeSession


class _RecordingSession(FakeSession):
    def __init__(self) -> None:
        super().__init__()
        self.clipboards: list[str] = []
        self.texts: list[str] = []

    def set_clipboard(self, text: str, *, sequence: int = 0) -> None:
        self.clipboards.append(text)

    def inject_text(self, text: str) -> None:  # 0.3.14 起不应再走这条路
        self.texts.append(text)


def _router(session: FakeSession, *, enabled: bool = True) -> Router:
    cfg = FakeConfig()
    cfg.misc.clipboard_sync = enabled
    return Router(cfg, session, FakeDesktop(), FakeMask())


def test_inject_clipboard_truncates_to_4kb() -> None:
    session = _RecordingSession()
    router = _router(session)
    assert router.inject_clipboard("a" * 5000) is True
    assert len(session.clipboards[0].encode("utf-8")) == 4096
    assert session.texts == []


def test_inject_clipboard_respects_switch() -> None:
    session = _RecordingSession()
    router = _router(session, enabled=False)
    assert router.inject_clipboard("hello") is False
    assert session.clipboards == []


def test_inject_clipboard_ignores_empty() -> None:
    session = _RecordingSession()
    router = _router(session)
    assert router.inject_clipboard("") is False
    assert session.clipboards == []


def test_inject_clipboard_writes_without_typing() -> None:
    session = _RecordingSession()
    router = _router(session)
    assert router.inject_clipboard("hello") is True
    assert session.clipboards == ["hello"]
    assert session.texts == []
