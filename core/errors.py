"""错误码与统一异常（设计文档 8.4）。

错误码是冻结契约：不要改名字或数值，日志/诊断包/用户提示都引用它们。
"""

from __future__ import annotations

from enum import StrEnum


class ErrorCode(StrEnum):
    """8.4 错误码表。``value`` 即对外展示的码。"""

    ADB_NOT_FOUND = "E100"
    DEVICE_OFFLINE = "E101"
    UHID_UNAVAILABLE = "E102"
    SERVER_START_FAILED = "E103"
    FORWARD_FAILED = "E104"
    HANDSHAKE_FAILED = "E105"
    SOCKET_BROKEN = "E106"

    HOOK_FAILED = "E200"
    RAWINPUT_FAILED = "E201"
    CLIPCURSOR_LEAK = "E202"

    CONFIG_INVALID = "E300"
    PLACEMENT_INVALID = "E301"

    UNEXPECTED = "E900"


MESSAGES: dict[ErrorCode, str] = {
    ErrorCode.ADB_NOT_FOUND: "找不到 adb，请在设置中配置 platform-tools 路径",
    ErrorCode.DEVICE_OFFLINE: "平板不在线或未授权 USB 调试",
    ErrorCode.UHID_UNAVAILABLE: "平板 /dev/uhid 不可写，只能使用降级模式",
    ErrorCode.SERVER_START_FAILED: "平板端 scrcpy-server 启动失败",
    ErrorCode.FORWARD_FAILED: "adb forward 失败（端口可能被占用）",
    ErrorCode.HANDSHAKE_FAILED: "与服务端握手失败（版本不匹配）",
    ErrorCode.SOCKET_BROKEN: "与平板的数据通道已断开",
    ErrorCode.HOOK_FAILED: "键盘钩子安装失败",
    ErrorCode.RAWINPUT_FAILED: "Raw Input 注册失败",
    ErrorCode.CLIPCURSOR_LEAK: "检测到光标锁定残留，已强制释放",
    ErrorCode.CONFIG_INVALID: "配置文件存在非法值，已回退默认值",
    ErrorCode.PLACEMENT_INVALID: "平板摆位失效，已重新锚定到主屏同侧",
    ErrorCode.UNEXPECTED: "发生未预期的错误",
}


class PadLendError(Exception):
    """带错误码的业务异常。所有对外失败的统一载体。"""

    def __init__(self, code: ErrorCode, detail: str = "", *, cause: BaseException | None = None):
        self.code = code
        self.detail = detail
        self.cause = cause
        super().__init__(f"{code.value} {code.name}: {MESSAGES.get(code, '')}"
                         + (f" — {detail}" if detail else ""))

    @property
    def user_message(self) -> str:
        base = MESSAGES.get(self.code, MESSAGES[ErrorCode.UNEXPECTED])
        return f"{base}（{self.code.value}）" + (f"：{self.detail}" if self.detail else "")


def wrap_unexpected(exc: BaseException, *, detail: str = "") -> PadLendError:
    """把任意异常转成 E900，保留 cause 便于日志追溯。"""
    return PadLendError(
        ErrorCode.UNEXPECTED,
        detail or f"{type(exc).__name__}: {exc}",
        cause=exc,
    )
