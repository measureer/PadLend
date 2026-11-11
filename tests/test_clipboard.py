"""剪贴板同步（6.8，SHOULD）：大小限制、开关与协议选择。"""

from __future__ import annotations

import socket
import threading

from core.router import Router
from tablet.session import TabletSession
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


def test_tablet_clipboard_is_forwarded_only_when_enabled() -> None:
    session = _RecordingSession()
    router = _router(session)
    received: list[str] = []
    router.on_clipboard_received = received.append

    session.publish_clipboard("平板文本")
    assert received == ["平板文本"]

    router._cfg.misc.clipboard_sync = False
    session.publish_clipboard("不应同步")
    assert received == ["平板文本"]


def test_receiver_reads_fragmented_messages_and_skips_non_clipboard() -> None:
    session = TabletSession()
    local, remote = socket.socketpair()
    received: list[str] = []
    arrived = threading.Event()

    def on_clipboard(text: str) -> None:
        received.append(text)
        arrived.set()

    payload = "来自平板".encode()
    packet = (
        b"\x01" + (9).to_bytes(8, "big")
        + b"\x02\x00\x02\x00\x02\xab\xcd"
        + b"\x00" + len(payload).to_bytes(4, "big") + payload
    )
    session._sock = local
    session.set_clipboard_callback(on_clipboard)
    session._start_receiver()
    try:
        # 逐字节发送，覆盖 TCP 任意拆包，而不是假设一次 recv 就是一条消息。
        for value in packet:
            remote.sendall(bytes([value]))
        assert arrived.wait(2.0)
    finally:
        session._close_socket()
        remote.close()

    assert received == ["来自平板"]
    assert session._receiver_thread is None
