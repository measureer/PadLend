"""scrcpy-server 生命周期（设计文档 6.1.2 / 6.1.3 / 12.3 / 附录 C.4）。

职责边界：**只管平板上那个 jar**（校验、推送、启动、清理、幽灵校验），
socket 握手与 UHID 报文在 :mod:`tablet.session` / :mod:`tablet.protocol`。

F7（3.3）是这里最重要的约束：强杀 PC 端客户端时 ``cleanup=true`` 不生效，
平板会残留持有 ``/dev/uhid`` 的 ``app_process``，而 ``ps | grep scrcpy`` 抓不到它。
可靠手段是按 ``com.genymobile.scrcpy.Server`` / ``com.genymobile.scrcpy.CleanUp``
类名 ``pkill -9 -f``（Android 17 实测命令行不含 jar 路径），然后
``dumpsys input | grep -c scrcpy`` 必须为 0。
"""

from __future__ import annotations

import hashlib
import logging
import subprocess
import time
from pathlib import Path

import constants as C
from core.errors import ErrorCode, PadLendError

from .adb import Adb

LOG = logging.getLogger("tablet.server")

# 远端 abstract socket 名（9.1）
REMOTE_ABSTRACT = "localabstract:scrcpy"
# 清理用的进程匹配串。Android 17 实测：app_process 的命令行形如
# `app_process / com.genymobile.scrcpy.Server 5.0.1 ...`，**不含 jar 路径**，
# 因此仅按 jar 名 pkill 抓不到幽灵进程；Server / CleanUp 两个类名必须保留，
# jar 名保留用于旧版 scrcpy（其命令行带 jar 路径）。
KILL_PATTERNS = (
    "com.genymobile.scrcpy.Server",
    "com.genymobile.scrcpy.CleanUp",
    "scrcpy-server-manual.jar",
)
KILL_PATTERN = KILL_PATTERNS[0]  # 兼容旧引用与日志
# 幽灵校验命令（附录 C.4），期望输出 0。
VERIFY_CMD = "dumpsys input | grep -c scrcpy"


def file_md5(path: Path, *, chunk: int = 1 << 20) -> str:
    """本地文件 md5（分块读，jar 只有几 MB 但别整读）。"""
    digest = hashlib.md5(usedforsecurity=False)
    with path.open("rb") as fh:
        for block in iter(lambda: fh.read(chunk), b""):
            digest.update(block)
    return digest.hexdigest()


class ScrcpyServer:
    """平板端 scrcpy-server 的启停器（单实例，可重复调用）。"""

    def __init__(
        self,
        adb: Adb,
        *,
        port: int | None = None,
        jar: Path | None = None,
        remote_jar: str = C.SCRCPY_REMOTE_JAR,
        version: str = C.SCRCPY_VERSION,
    ) -> None:
        self._adb = adb
        self.port = port
        self.jar = Path(jar) if jar is not None else C.SCRCPY_SERVER_JAR
        self.remote_jar = remote_jar
        self.version = version
        self._proc: subprocess.Popen[bytes] | None = None
        self._pushed = False
        self._last_error = ""
        self._verify_detail = ""

    # -- 查询 --------------------------------------------------------------
    @property
    def proc_alive(self) -> bool:
        """本地那条 ``adb shell`` 是否还活着（不发新 adb 命令，GUI 线程可调）。"""
        return self._proc is not None and self._proc.poll() is None

    @property
    def last_error(self) -> str:
        return self._last_error

    @property
    def verify_detail(self) -> str:
        return self._verify_detail

    # -- 推送 --------------------------------------------------------------
    def push_if_needed(self) -> bool:
        """本地 md5 vs 远端 md5，不一致才推；返回是否真的推了。

        jar 缺失是硬错误（E103）：没有 jar 一定起不来，不要静默重试。
        """
        if not self.jar.is_file():
            raise PadLendError(
                ErrorCode.SERVER_START_FAILED,
                f"找不到 {self.jar}（从 scrcpy v{C.SCRCPY_VERSION} 发布包提取 "
                f"scrcpy-server 并放到该路径）",
            )
        local = file_md5(self.jar)
        remote = None
        try:
            remote = self._adb.md5(self.remote_jar)
        except PadLendError as exc:  # 设备暂时不可达时按"需要重推"处理
            LOG.debug("读取远端 md5 失败，将重推: %s", exc.detail)
        if remote == local:
            self._pushed = True
            LOG.info("服务端 jar 已是同一版本（md5=%s），跳过推送", local)
            return False
        LOG.info("推送服务端 jar（local=%s remote=%s）", local, remote)
        self._adb.push(self.jar, self.remote_jar)
        self._pushed = True
        return True

    # -- 启动 --------------------------------------------------------------
    def command(self) -> str:
        """6.1.2 的启动命令行；逐字符与文档一致（参数名不要改）。

        版本号取自实例（9.5：参数里的版本 MUST 与 jar 一致），
        其余参数由 ``constants.scrcpy_server_args`` 统一维护。
        """
        args = C.scrcpy_server_args(C.DEFAULT_SERVER_PORT)
        # 第 5 项是版本号字符串（CLASSPATH / app_process / / / class / version）
        args[4] = self.version
        return " ".join(args)

    def spawn(self) -> None:
        """后台起服务端（长驻，本地拿 Popen 句柄）。"""
        if self.proc_alive:
            LOG.debug("服务端已在运行，跳过 spawn")
            return
        command = self.command()
        LOG.info("启动服务端: %s", command)
        self._proc = self._adb.popen_shell(command)
        self._last_error = ""

    def running(self) -> bool:
        """服务端是否在跑（先看本地句柄，再问平板）。"""
        if self._proc is not None:
            return self._proc.poll() is None
        return any(self._adb.shell_ok("pgrep", "-f", pat) for pat in KILL_PATTERNS)

    # -- 端口转发 ----------------------------------------------------------
    def forward(self) -> int:
        """在 1234–1244 内找一个可用端口并转发（12.1 端口占用）。"""
        candidates = [self.port] if self.port else list(C.SERVER_PORT_RANGE)
        errors: list[str] = []
        for port in candidates:
            if port is None:
                continue
            self._adb.forward_remove(port)  # 清掉上次残留，幂等
            try:
                self._adb.forward(port, REMOTE_ABSTRACT)
            except PadLendError as exc:
                errors.append(f"{port}: {exc.detail}")
                LOG.debug("端口 %d 转发失败: %s", port, exc.detail)
                continue
            self.port = port
            LOG.info("adb forward tcp:%d → %s", port, REMOTE_ABSTRACT)
            return port
        detail = "；".join(errors[-3:]) if errors else "没有可用端口"
        raise PadLendError(
            ErrorCode.FORWARD_FAILED,
            f"{C.SERVER_PORT_RANGE[0]}–{C.SERVER_PORT_RANGE[-1]} 全部失败：{detail}",
        )

    def forward_remove(self) -> None:
        if self.port is not None:
            self._adb.forward_remove(self.port)

    def start(self) -> int:
        """推送 → 启动 → 转发，返回实际端口（6.1.2 的顺序）。"""
        self.push_if_needed()
        self.kill()  # 先清掉可能存在的旧实例，保证幂等
        self.spawn()
        return self.forward()

    # -- 清理 --------------------------------------------------------------
    def kill(self) -> None:
        """pkill 远端 + 收掉本地 adb shell（F7）。"""
        for pattern in KILL_PATTERNS:
            self._adb.shell_ok("pkill", "-9", "-f", pattern)
        self._reap_local()
        LOG.info("已执行 pkill -9 -f %s", " / ".join(KILL_PATTERNS))

    def _reap_local(self) -> None:
        proc = self._proc
        if proc is None:
            return
        self._proc = None
        if proc.poll() is not None:
            return
        try:
            proc.terminate()
            proc.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            try:
                proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:  # pragma: no cover - adb 卡死
                LOG.warning("本地 adb shell 进程未能退出 pid=%s", proc.pid)
        except OSError as exc:  # pragma: no cover - 进程已消失
            LOG.debug("收尾本地 adb shell 失败: %s", exc)

    def verify_clean(self) -> bool:
        """``dumpsys input | grep -c scrcpy`` 必须为 0（6.1.3）。"""
        try:
            rc, out = self._adb.shell_result(VERIFY_CMD)
        except PadLendError as exc:
            self._verify_detail = f"无法执行校验命令: {exc.detail}"
            return False
        text = out.strip()
        count = None
        for line in reversed(text.splitlines()):
            line = line.strip()
            if line.isdigit():
                count = int(line)
                break
        if count is None:
            self._verify_detail = f"grep 输出不可解析（rc={rc}）: {text!r}"
            return False
        if count != 0:
            self._verify_detail = f"仍有 {count} 处 scrcpy 残留"
            return False
        self._verify_detail = "clean"
        return True

    def stop(self) -> bool:
        """kill → 撤销转发 → 复检；返回是否干净（不抛异常）。"""
        try:
            self.kill()
        except PadLendError as exc:  # pkill 失败也要继续收转发
            LOG.warning("kill 失败: %s", exc.detail)
        self.forward_remove()
        if self.verify_clean():
            return True
        LOG.warning("清理校验失败：%s；重试一次 pkill", self._verify_detail)
        time.sleep(0.2)
        for pattern in KILL_PATTERNS:
            self._adb.shell_ok("pkill", "-9", "-f", pattern)
        return self.verify_clean()

    def __repr__(self) -> str:  # pragma: no cover - 调试输出
        return f"ScrcpyServer(port={self.port}, proc_alive={self.proc_alive})"
