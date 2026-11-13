"""平板音频转发：把 scrcpy 的裸 PCM 送到本机 Rust 播放器。

scrcpy-server 在 ``audio=true`` 且 ``raw_stream=true`` 时，音频 socket 上
是连续的 48 kHz / 双声道 / 16-bit 小端 PCM，没有容器或帧头。Python 只负责
搬运，解码与播放由 Tauri 的 ``audio.rs``（cpal）完成；这样 Windows 声音
设备、重采样和缓冲都在 Rust 侧，不依赖 Python 的音频库。

设计约束：

* 播放器是可选的外部 sink；sink 连不上或中途断开时继续把音频读掉，
  不能让控制通道因为音频缓冲区反压而卡住。
* 本模块不拥有 scrcpy 的音频 socket，只拥有自己创建的 sink；
  ``stop()`` 幂等，重复调用安全。
"""

from __future__ import annotations

import contextlib
import logging
import socket
import struct
import threading
from collections.abc import Callable

LOG = logging.getLogger(__name__)

AUDIO_SAMPLE_RATE = 48_000
AUDIO_CHANNELS = 2
AUDIO_BITS = 16
# 与 src-tauri/src/audio.rs 的 HEADER_MAGIC 保持一致。
AUDIO_HEADER = struct.pack("<4sIII", b"PLAU", AUDIO_SAMPLE_RATE, AUDIO_CHANNELS, AUDIO_BITS)
AUDIO_SINK_CONNECT_TIMEOUT_S = 2.0
AUDIO_SOCKET_POLL_S = 0.5
AUDIO_READ_SIZE = 65_536


def connect_sink(port: int, *, timeout: float = AUDIO_SINK_CONNECT_TIMEOUT_S) -> socket.socket:
    """连接本机 Rust 音频 sink，并发送 16 字节格式头。"""
    sock = socket.create_connection(("127.0.0.1", int(port)), timeout=timeout)
    try:
        sock.sendall(AUDIO_HEADER)
        sock.settimeout(None)
    except OSError:
        sock.close()
        raise
    return sock


class AudioRelay:
    """从 scrcpy 音频 socket 读 PCM，转发给可选 sink；sink 故障自动降级。"""

    def __init__(
        self,
        audio: socket.socket,
        sink_port: int | None,
        *,
        connect: Callable[[int], socket.socket] = connect_sink,
    ) -> None:
        self._audio = audio
        self._sink: socket.socket | None = None
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._lock = threading.Lock()
        self.degraded = False
        self.bytes_forwarded = 0
        if sink_port is not None:
            try:
                self._sink = connect(sink_port)
            except OSError as exc:
                self.degraded = True
                LOG.warning("音频播放器不可用，继续以静音方式读取音频: %s", exc)

    @property
    def running(self) -> bool:
        thread = self._thread
        return thread is not None and thread.is_alive()

    def start(self) -> None:
        with self._lock:
            if self._thread is not None and self._thread.is_alive():
                return
            self._stop.clear()
            thread = threading.Thread(
                target=self._run,
                name="padlend-audio-relay",
                daemon=True,
            )
            self._thread = thread
            thread.start()

    def _run(self) -> None:
        sink = self._sink
        try:
            while not self._stop.is_set():
                try:
                    chunk = self._audio.recv(AUDIO_READ_SIZE)
                except TimeoutError:
                    continue
                except OSError as exc:
                    if not self._stop.is_set():
                        LOG.debug("音频 socket 读取结束: %s", exc)
                    break
                if not chunk:
                    break
                if sink is None:
                    continue
                try:
                    sink.sendall(chunk)
                except OSError as exc:
                    self.degraded = True
                    LOG.warning("音频播放器写入失败，后续音频将丢弃: %s", exc)
                    self._close_sink(sink)
                    sink = None
                    self._sink = None
                    continue
                self.bytes_forwarded += len(chunk)
        finally:
            if sink is not None:
                self._close_sink(sink)
                self._sink = None
            LOG.debug("音频转发线程结束（已转发 %d 字节）", self.bytes_forwarded)

    def stop(self) -> None:
        """停止转发；可重复调用，不关闭 scrcpy 音频 socket。"""
        self._stop.set()
        with self._lock:
            thread = self._thread
            self._thread = None
        if thread is not None and thread is not threading.current_thread():
            thread.join(timeout=2.0)
        sink = self._sink
        self._sink = None
        if sink is not None:
            self._close_sink(sink)

    @staticmethod
    def _close_sink(sock: socket.socket) -> None:
        with contextlib.suppress(OSError):
            sock.close()
