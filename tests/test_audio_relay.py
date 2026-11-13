"""音频转发与 scrcpy 双通道握手的模拟测试（1.4.0）。"""

from __future__ import annotations

import threading

import pytest

import constants as C
from tablet import session as session_module
from tablet.audio import AudioRelay
from tablet.session import TabletSession


class FakeAudioSocket:
    def __init__(self, chunks: list[bytes]) -> None:
        self._chunks = list(chunks)
        self.closed = False
        self.timeouts: list[float | None] = []

    def recv(self, _size: int) -> bytes:
        return self._chunks.pop(0) if self._chunks else b""

    def settimeout(self, value: float | None) -> None:
        self.timeouts.append(value)

    def shutdown(self, _how: int) -> None:
        pass

    def close(self) -> None:
        self.closed = True


class FakeSink:
    def __init__(self, *, fail: bool = False) -> None:
        self.data = bytearray()
        self.fail = fail
        self.closed = False

    def sendall(self, data: bytes) -> None:
        if self.fail:
            raise OSError("sink gone")
        self.data.extend(data)

    def close(self) -> None:
        self.closed = True


def _run_relay(audio: FakeAudioSocket, sink: FakeSink | None) -> AudioRelay:
    relay = AudioRelay(
        audio,
        None if sink is None else 1,
        connect=(lambda _port: sink) if sink is not None else (lambda _port: None),  # type: ignore[arg-type,return-value]
    )
    relay.start()
    thread = relay._thread
    assert thread is not None
    thread.join(timeout=2.0)
    assert not thread.is_alive()
    return relay


def test_audio_relay_forwards_every_pcm_byte() -> None:
    audio = FakeAudioSocket([b"\x01\x00\x02\x00", b"\x03\x00\x04\x00", b""])
    sink = FakeSink()
    relay = _run_relay(audio, sink)
    assert bytes(sink.data) == b"\x01\x00\x02\x00\x03\x00\x04\x00"
    assert relay.bytes_forwarded == 8
    assert relay.degraded is False
    assert sink.closed is True


def test_audio_relay_drains_when_sink_write_fails() -> None:
    audio = FakeAudioSocket([b"\x01\x00\x02\x00", b"\x03\x00\x04\x00", b""])
    sink = FakeSink(fail=True)
    relay = _run_relay(audio, sink)
    assert relay.degraded is True
    assert relay.bytes_forwarded == 0
    assert sink.closed is True
    assert audio.closed is False  # 音频 socket 归 Session 所有


def test_audio_relay_keeps_draining_when_sink_is_unavailable() -> None:
    audio = FakeAudioSocket([b"\x00" * 8, b""])
    relay = AudioRelay(audio, 1, connect=lambda _port: (_ for _ in ()).throw(OSError("no sink")))
    relay.start()
    thread = relay._thread
    assert thread is not None
    thread.join(timeout=2.0)
    assert not thread.is_alive()
    assert relay.degraded is True


@pytest.mark.parametrize("audio", [False, True])
def test_scrcpy_server_args_audio_channel(audio: bool) -> None:
    args = C.scrcpy_server_args(1234, audio=audio)
    assert f"audio={'true' if audio else 'false'}" in args
    assert "video=false" in args
    assert "control=true" in args


class FakeHandshakeSocket:
    def __init__(self) -> None:
        self.timeouts: list[float | None] = []
        self.closed = False

    def recv(self, _size: int) -> bytes:
        return C.HANDSHAKE_DUMMY

    def settimeout(self, value: float | None) -> None:
        self.timeouts.append(value)

    def shutdown(self, _how: int) -> None:
        pass

    def close(self) -> None:
        self.closed = True


class FakeRelay:
    def __init__(self, audio: object, sink_port: int | None) -> None:
        self.audio = audio
        self.sink_port = sink_port
        self.degraded = False
        self.started = False

    def start(self) -> None:
        self.started = True


def _bare_session() -> TabletSession:
    session = object.__new__(TabletSession)
    session._port = 2345
    session._closing = threading.Event()
    session._audio_active = False
    session._audio_sink_port = None
    session._sock = None
    session._sock_broken = False
    session._send_lock = threading.Lock()
    session._audio_sock = None
    session._audio_relay = None
    return session


def test_connect_socket_uses_audio_then_control_order(monkeypatch: pytest.MonkeyPatch) -> None:
    audio_sock = FakeHandshakeSocket()
    control_sock = FakeHandshakeSocket()
    order: list[FakeHandshakeSocket] = [audio_sock, control_sock]
    monkeypatch.setattr(
        session_module.socket, "create_connection", lambda *_a, **_k: order.pop(0)
    )
    monkeypatch.setattr(session_module, "AudioRelay", FakeRelay)

    session = _bare_session()
    session._audio_active = True
    session._connect_socket()

    assert session._audio_sock is audio_sock
    assert session._sock is control_sock
    assert isinstance(session._audio_relay, FakeRelay)
    assert session._audio_relay.started is True
    assert order == []
    assert audio_sock.timeouts[-1] == session_module.AUDIO_SOCKET_POLL_S


def test_connect_socket_without_audio_keeps_single_channel(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    control_sock = FakeHandshakeSocket()
    monkeypatch.setattr(
        session_module.socket, "create_connection", lambda *_a, **_k: control_sock
    )
    monkeypatch.setattr(session_module, "AudioRelay", FakeRelay)

    session = _bare_session()
    session._connect_socket()

    assert session._sock is control_sock
    assert session._audio_sock is None
    assert session._audio_relay is None


class FakeServer:
    def __init__(self, _adb: object, *, port: int | None, jar: object, audio: bool) -> None:
        self.port = port
        self.audio = audio
        self.killed = False
        self.removed = False
        self.spawned = False

    def kill(self) -> None:
        self.killed = True

    def forward_remove(self) -> None:
        self.removed = True

    def spawn(self) -> None:
        self.spawned = True

    def forward(self) -> int:
        self.port = 3456
        return 3456


def test_restart_without_audio_discards_server_and_clears_flag(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    old = FakeServer(None, port=1234, jar=None, audio=True)
    monkeypatch.setattr(session_module, "ScrcpyServer", FakeServer)
    session = _bare_session()
    session._server = old
    session._dev_adb = object()
    session._jar = None
    session._audio_active = True
    session._close_socket = lambda: None  # type: ignore[method-assign]

    session._restart_without_audio()

    assert old.killed is True
    assert old.removed is True
    assert session._audio_active is False
    assert isinstance(session._server, FakeServer)
    assert session._server is not old
    assert session._server.audio is False
    assert session._server.spawned is True
    assert session._port == 3456


def test_close_socket_releases_audio_before_control(monkeypatch: pytest.MonkeyPatch) -> None:
    audio_sock = FakeHandshakeSocket()
    control_sock = FakeHandshakeSocket()
    relay = FakeRelay(audio_sock, None)
    relay.stop_called = False  # type: ignore[attr-defined]

    def stop() -> None:
        relay.stop_called = True  # type: ignore[attr-defined]

    relay.stop = stop  # type: ignore[method-assign]

    session = _bare_session()
    session._audio_sock = audio_sock
    session._audio_relay = relay
    session._sock = control_sock
    session._receiver_stop = None
    session._receiver_thread = None
    session._close_socket()

    assert audio_sock.closed is True
    assert relay.stop_called is True
    assert session._audio_sock is None
    assert session._audio_relay is None
    assert control_sock.closed is True
