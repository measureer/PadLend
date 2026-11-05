"""全局常量、超时、HID 描述符（设计文档 7.1 / 9.3 / 9.4 / 附录 A）。

本模块是依赖图的最底层：只允许 import 标准库，不得 import 本项目其它模块。
魔法数字统一收在这里，业务代码通过名字引用。
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# 应用标识
# ---------------------------------------------------------------------------
APP_NAME = "PadLend"
APP_ID = "PadLend"
VERSION = "0.3.18"
LICENSE = "Apache-2.0"

# 单实例互斥体（7.5）。Global\ 前缀保证跨用户会话唯一。
SINGLE_INSTANCE_MUTEX = r"Global\PadLend.SingleInstance"

# ---------------------------------------------------------------------------
# 路径
# ---------------------------------------------------------------------------
PROJECT_ROOT = Path(__file__).resolve().parent
# 源码运行时两者相同；PyInstaller onefile 下 __file__ 指向临时解包目录
# （_MEIPASS），而可执行文件所在目录才是用户可见、可写的应用目录。
if getattr(sys, "frozen", False):
    APP_DIR = Path(sys.executable).resolve().parent
    BUNDLE_DIR = Path(getattr(sys, "_MEIPASS", APP_DIR))
else:
    APP_DIR = PROJECT_ROOT
    BUNDLE_DIR = PROJECT_ROOT
VENDOR_DIR = BUNDLE_DIR / "vendor"
ASSETS_DIR = BUNDLE_DIR / "ui" / "theme"
ICONS_DIR = ASSETS_DIR / "icons"


def _writable_dir(base: Path, name: str = APP_NAME) -> Path | None:
    """返回可写的 ``base/name``；无法创建或写入时返回 ``None``。

    设计要求 ``%APPDATA%\\PadLend``，但受限环境（沙箱/商店版 Python/
    受控文件夹访问）可能拒绝写用户目录，因此用真实探针而不是 ``os.access``。
    """
    target = base / name
    try:
        target.mkdir(parents=True, exist_ok=True)
        probe = target / ".write-probe"
        probe.write_text("", encoding="utf-8")
        probe.unlink(missing_ok=True)
        return target
    except OSError:
        return None


def config_dir() -> Path:
    """%APPDATA%\\PadLend（6.9）。找不到时按 PADLEND_HOME/APPDATA/LOCALAPPDATA/项目目录逐级回退。"""
    override = os.environ.get("PADLEND_HOME")
    if override:
        target = Path(override).expanduser()
        target.mkdir(parents=True, exist_ok=True)
        return target

    candidates: list[Path] = []
    appdata = os.environ.get("APPDATA")
    if appdata:
        candidates.append(Path(appdata))
    local = os.environ.get("LOCALAPPDATA")
    if local:
        candidates.append(Path(local))
    candidates.append(APP_DIR)

    for base in candidates:
        name = ".padlend" if base == APP_DIR else APP_NAME
        found = _writable_dir(base, name)
        if found is not None:
            return found
    raise OSError(
        "找不到可写的配置目录（尝试过 PADLEND_HOME/APPDATA/LOCALAPPDATA/项目目录）"
    )


def config_path() -> Path:
    return config_dir() / "config.yaml"


def log_dir() -> Path:
    return config_dir() / "logs"


def clean_exit_marker() -> Path:
    """存在 = 上次正常退出；缺失 = 需要启动前强制清理（7.5 / 12.3）。"""
    return config_dir() / "exit.clean"


# ---------------------------------------------------------------------------
# scrcpy-server（6.1.2 / 9.5）
# ---------------------------------------------------------------------------
SCRCPY_VERSION = "5.0.1"
SCRCPY_SERVER_JAR_NAME = f"scrcpy-server-v{SCRCPY_VERSION}.jar"
SCRCPY_SERVER_JAR = VENDOR_DIR / SCRCPY_SERVER_JAR_NAME
# 远端文件名与设计一致，pkill -f 依赖它，改名会使 F7 清理失效。
SCRCPY_REMOTE_JAR = "/data/local/tmp/scrcpy-server-manual.jar"
SCRCPY_SERVER_CLASS = "com.genymobile.scrcpy.Server"

DEFAULT_SERVER_PORT = 1234
SERVER_PORT_RANGE = tuple(range(1234, 1245))  # E104 换端口 1234-1244


def scrcpy_server_args(port: int) -> list[str]:
    """服务端启动参数（6.1.2 实测可用，勿改参数名）。"""
    return [
        f"CLASSPATH={SCRCPY_REMOTE_JAR}",
        "app_process",
        "/",
        SCRCPY_SERVER_CLASS,
        SCRCPY_VERSION,
        "tunnel_forward=true",
        "video=false",
        "audio=false",
        "control=true",
        "cleanup=true",
        "raw_stream=true",
        "send_dummy_byte=true",
        "max_size=4096",
    ]


# 握手字节：scrcpy 5.0.1 在 tunnel_forward 且只开 control 通道时，
# DesktopConnection 只 accept 一次并写 **1 个** 0 字节
# （server/.../device/DesktopConnection.java: controlSocket.write(0)）。
HANDSHAKE_DUMMY = b"\x00"

# ---------------------------------------------------------------------------
# adb
# ---------------------------------------------------------------------------
ADB_EXE = "adb.exe"
ADB_SHELL_TIMEOUT = 5.0
ADB_PUSH_TIMEOUT = 120.0
ADB_FORWARD_TIMEOUT = 10.0
# 无线调试：connect/pair 要做 TCP 探测，比普通 adb 命令慢得多。
ADB_CONNECT_TIMEOUT = 20.0
ADB_PAIR_TIMEOUT = 30.0
ADB_SERVER_TIMEOUT = 8.0

# 无线调试端口提示：Android 11+ 的「无线调试」默认 5555，配对端口随机。
WIRELESS_DEFAULT_PORT = 5555
SOCKET_CONNECT_TIMEOUT = 5.0
SOCKET_HANDSHAKE_TIMEOUT = 3.0
# app_process 起来后到开始在 abstract socket 上监听有几百毫秒延迟；
# adb forward 会先接受本地连接再因远端未就绪而立即关闭。握手必须重试。
HANDSHAKE_RETRY_TIMEOUT_S = 8.0
HANDSHAKE_RETRY_INTERVAL_S = 0.1
SOCKET_SEND_TIMEOUT = 2.0

# 6.1.4 健康检查与重连
HEARTBEAT_INTERVAL_S = 2.0
HEALTHCHECK_INTERVAL_S = 2.0
RECONNECT_BACKOFF_S = (1.0, 2.0, 4.0, 8.0, 16.0)
CONSECUTIVE_WRITE_FAILURES = 3

# 6.1.1 首连重试：40s 内每 2s 一次，然后指数退避
INITIAL_RETRY_WINDOW_S = 40.0
INITIAL_RETRY_INTERVAL_S = 2.0

# 12.1 pointer_speed 复查
POINTER_SPEED_POLL_S = 10.0

# ---------------------------------------------------------------------------
# UHID / HID（9.3 / 9.4 / 附录 A）
# ---------------------------------------------------------------------------
UHID_MOUSE_ID = 0x0001
UHID_KEYBOARD_ID = 0x0002

HID_VENDOR_ID = 0x1D6B  # Linux Foundation
HID_MOUSE_PRODUCT_ID = 0x0001
HID_KEYBOARD_PRODUCT_ID = 0x0002

HID_MOUSE_NAME = b"PadLend Mouse"
HID_KEYBOARD_NAME = b"PadLend Keyboard"

# 5 键 + 相对 X/Y + 滚轮（附录 A.2）。长度以实际字节为准，不写死 0x34。
HID_MOUSE_DESCRIPTOR = bytes(
    [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00,
        0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01,
        0x95, 0x05, 0x75, 0x01, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x03, 0x81, 0x01,
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F,
        0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
        0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01,
        0x81, 0x06,
        0xC0, 0xC0,
    ]
)

# 6KRO 键盘（附录 A.3）
HID_KEYBOARD_DESCRIPTOR = bytes(
    [
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01,
        0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
        0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
        0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65,
        0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00,
        0xC0,
    ]
)

MOUSE_REPORT_SIZE = 4
KEYBOARD_REPORT_SIZE = 8

HID_MOUSE_REPORT_ID = 0  # 描述符未声明 Report ID
KEYBOARD_KRO = 6

# 描述符 Logical Max = 127，单报文位移上限
HID_CLAMP = 127

# Windows 原始滚轮一格是 120 单位（WHEEL_DELTA）；HID 滚轮一格是 1。
# Router 按 120 累计换算成 HID detent，余数留到下一次事件。
WHEEL_UNITS_PER_DETENT = 120

# 鼠标按键位图（9.3 报告字节 0）
MOUSE_BTN_LEFT = 0x01
MOUSE_BTN_RIGHT = 0x02
MOUSE_BTN_MIDDLE = 0x04
MOUSE_BTN_SIDE1 = 0x08
MOUSE_BTN_SIDE2 = 0x10

# Raw Input 按键标志（6.2.1 usButtonFlags）→ HID 位图
RAW_BUTTON_FLAGS = {
    0x0001: MOUSE_BTN_LEFT,   # RI_MOUSE_LEFT_BUTTON_DOWN
    0x0002: MOUSE_BTN_LEFT,   # RI_MOUSE_LEFT_BUTTON_UP
    0x0004: MOUSE_BTN_RIGHT,  # RI_MOUSE_RIGHT_BUTTON_DOWN
    0x0008: MOUSE_BTN_RIGHT,  # RI_MOUSE_RIGHT_BUTTON_UP
    0x0010: MOUSE_BTN_MIDDLE, # RI_MOUSE_MIDDLE_BUTTON_DOWN
    0x0020: MOUSE_BTN_MIDDLE, # RI_MOUSE_MIDDLE_BUTTON_UP
    0x0040: MOUSE_BTN_SIDE1,  # RI_MOUSE_BUTTON_4_DOWN
    0x0080: MOUSE_BTN_SIDE1,  # RI_MOUSE_BUTTON_4_UP
    0x0100: MOUSE_BTN_SIDE2,  # RI_MOUSE_BUTTON_5_DOWN
    0x0200: MOUSE_BTN_SIDE2,  # RI_MOUSE_BUTTON_5_UP
}
RAW_BUTTON_UP_FLAGS = frozenset({0x0002, 0x0008, 0x0020, 0x0080, 0x0200})

# 键盘报告修饰位（6.7.2）
MOD_LEFT_CTRL = 0x01
MOD_LEFT_SHIFT = 0x02
MOD_LEFT_ALT = 0x04
MOD_LEFT_GUI = 0x08
MOD_RIGHT_CTRL = 0x10
MOD_RIGHT_SHIFT = 0x20
MOD_RIGHT_ALT = 0x40
MOD_RIGHT_GUI = 0x80

# ---------------------------------------------------------------------------
# 输入 / 状态机默认阈值（6.5 / 6.9）
# ---------------------------------------------------------------------------
ARM_PX = 40
ARM_WINDOW_MS = 80
# 慢推兜底：没在 80ms 内攒够 40px，但已经明显向外推了这么多，
# 仍视为有意接管。轻推/慢推的用户不该被 500px/s 的速度门槛挡住。
ARM_SLOW_PX = 12
# 在平板边缘反向拖回多少像素才交还 PC。0.3.16 起默认 120（原来 24），
# 边缘点按晃动不再误退，代价是回 PC 要多拖一点点。
EXIT_OVERFLOW_PX = 120

EDGE_BAND_PX = 1
ENTRY_MARGIN_PX = 2
DEBOUNCE_MS = 250
BOUNDARY_INSET_PX = 0
CORNER_DIRECTION_RATIO = 0.15  # 6.3.6 角落裁决

DEFAULT_GAIN = 1.0
DEFAULT_REPORT_HZ = 1000
MIN_REPORT_HZ = 125
MAX_REPORT_HZ = 1000
# 0.3.16：上报频率只给四个固定档（设置页是下拉框，不再是滑条）。
REPORT_HZ_CHOICES = (125, 250, 500, 1000)
MAX_BATCH = 8  # 单帧最多 8 报文（D12）
SEND_QUEUE_MAXLEN = 64
RAW_QUEUE_MAXSIZE = 4096
KEY_QUEUE_MAXSIZE = 256

# 10.3 timeBeginPeriod(1)
TIMER_PERIOD_MS = 1

# 12.1 看门狗
CLIPCURSOR_WATCHDOG_S = 5.0
HOOK_WATCHDOG_S = 30.0

# ---------------------------------------------------------------------------
# Windows DPI / DWM（6.3.1 / 6.10.4）
# ---------------------------------------------------------------------------
DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = -4
DWM_USE_IMMERSIVE_DARK_MODE = 20
DWM_WINDOW_CORNER_PREFERENCE = 33
DWMWCP_ROUND = 2

# ---------------------------------------------------------------------------
# 事件词表（7.4，测试据此断言，勿改）
# ---------------------------------------------------------------------------
EVENT_CAP_START = "cap.start"
EVENT_CAP_HIT = "cap.hit"
EVENT_CAP_END = "cap.end"
EVENT_ARM_FAIL = "arm.fail"
EVENT_STATE_FAULT = "state.fault"
EVENT_TABLET_CONNECT = "tablet.connect"
EVENT_TABLET_RECONNECT = "tablet.reconnect"
EVENT_TABLET_CLEANUP_GHOST = "tablet.cleanup.ghost"
EVENT_KBD_SUPPRESS_ON = "kbd.suppress.on"
EVENT_CFG_RELOAD = "cfg.reload"

EVENT_NAMES = frozenset(
    {
        EVENT_CAP_START,
        EVENT_CAP_HIT,
        EVENT_CAP_END,
        EVENT_ARM_FAIL,
        EVENT_STATE_FAULT,
        EVENT_TABLET_CONNECT,
        EVENT_TABLET_RECONNECT,
        EVENT_TABLET_CLEANUP_GHOST,
        EVENT_KBD_SUPPRESS_ON,
        EVENT_CFG_RELOAD,
    }
)

# 安全键（6.2.3）：永不被屏蔽
SAFETY_HOTKEYS = ("Ctrl+Alt+S", "Ctrl+Alt+Q")
