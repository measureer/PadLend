"""右侧设置面板（设计文档 6.10.1）。

分组：平板位置 / 手感 / 键盘 / 快捷键 / 杂项。
- 摆位控件变化 → ``placementChanged(Placement)``（只做画布预览，不落盘）。
- 保存 → 写回 ``Config`` → ``Config.validate()`` → ``Config.save()``；
  有 issue 时标红并显示中文问题列表，不发 ``configChanged``。
- 吸附边下拉的数据源是 ``DesktopMap.exterior_segments()``（真实外轮廓边段）。
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import TYPE_CHECKING, Any

from PySide6.QtCore import Qt, Signal
from PySide6.QtGui import QKeySequence
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QFrame,
    QGroupBox,
    QHBoxLayout,
    QKeySequenceEdit,
    QLabel,
    QPushButton,
    QScrollArea,
    QSlider,
    QVBoxLayout,
    QWidget,
)

import constants as C
from core.geometry import Align, Edge, EdgeSegment, Placement
from ui.style import repolish, set_invalid

if TYPE_CHECKING:
    from config import Config

log = logging.getLogger("ui.settings_panel")

EDGE_LABELS: dict[Edge, str] = {
    Edge.LEFT: "左侧",
    Edge.RIGHT: "右侧",
    Edge.TOP: "上方",
    Edge.BOTTOM: "下方",
}

ALIGN_LABELS: list[tuple[str, str]] = [
    ("等比", Align.PROPORTIONAL.value),
    ("居中", Align.CENTER.value),
    ("1:1", Align.ONE_TO_ONE.value),
]

CORNER_LABELS: list[tuple[str, str]] = [
    ("自动", ""),
    ("左", Edge.LEFT.value),
    ("右", Edge.RIGHT.value),
    ("上", Edge.TOP.value),
    ("下", Edge.BOTTOM.value),
]

LAYOUTS: list[str] = ["us", "uk", "de", "fr", "es", "ru", "ja"]

_KEY_WIDTH = 76
_VALUE_WIDTH = 68


class _NoWheelComboBox(QComboBox):
    """不接受滚轮的组合框（0.3.13）。

    设置页本身要上下滚动；指针停在组合框上时滚轮会顺手改掉选项（实测把
    「布局」从 US 滚成 UK），非常容易误触。滚轮一律忽略，事件会冒泡给
    外层 ``QScrollArea`` 去滚动页面。
    """

    def wheelEvent(self, event: Any) -> None:  # noqa: N802 - Qt 命名
        event.ignore()


class _NoWheelSlider(QSlider):
    """不接受滚轮的滑条，理由同上。"""

    def wheelEvent(self, event: Any) -> None:  # noqa: N802 - Qt 命名
        event.ignore()


class SliderRow(QWidget):
    """一行滑杆：名称 + QSlider + 数值标签。"""

    def __init__(
        self,
        title: str,
        minimum: int,
        maximum: int,
        value: int,
        formatter: Any,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._formatter = formatter
        self.slider = _NoWheelSlider(Qt.Orientation.Horizontal, self)
        self.slider.setRange(minimum, maximum)
        self.slider.setValue(value)
        self.slider.setSingleStep(1)
        self.slider.setPageStep(max(1, (maximum - minimum) // 10))
        self.value_label = QLabel(self)
        self.value_label.setFixedWidth(_VALUE_WIDTH)
        self.value_label.setAlignment(
            Qt.AlignmentFlag.AlignRight | Qt.AlignmentFlag.AlignVCenter
        )
        self.value_label.setObjectName("mutedText")

        name = QLabel(title, self)
        name.setFixedWidth(_KEY_WIDTH)

        layout = QHBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(8)
        layout.addWidget(name)
        layout.addWidget(self.slider, 1)
        layout.addWidget(self.value_label)
        self.update_label()

    def update_label(self) -> None:
        self.value_label.setText(self._formatter(self.slider.value()))

    def value(self) -> int:
        return int(self.slider.value())

    def set_value(self, value: int) -> None:
        self.slider.setValue(int(value))
        self.update_label()


class SettingsPanel(QWidget):
    """主窗右栏：摆位/手感/键盘/快捷键/杂项 + 保存。"""

    placementChanged = Signal(object)
    configChanged = Signal(object)

    def __init__(
        self,
        config: Config,
        desktop: object,
        *,
        config_path: Path | None = None,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self.setObjectName("settingsPanel")
        self.setMinimumWidth(300)
        self._config = config
        self._desktop = desktop
        self._config_path = Path(config_path) if config_path is not None else C.config_path()
        self._segments: list[EdgeSegment] = []
        self._loading = False
        self._dirty = False
        self._field_widgets: dict[str, QWidget] = {}

        self._build()
        self._connect()
        self.refresh_segments()
        self.load_from_config()

    # ------------------------------------------------------------------ 构建
    def _build(self) -> None:
        self.title_label = QLabel("设置", self)
        self.title_label.setObjectName("sectionTitle")
        hint = QLabel("拖动画布可改摆位 · Ctrl+, 聚焦此处", self)
        hint.setObjectName("hintText")
        hint.setWordWrap(True)

        # 设置页很宽：左右两列等分铺开，避免所有控件挤在左侧、右侧大片留白。
        # 左：平板位置 / 键盘    右：手感 / 快捷键    底：杂项（跨两列）
        content = QWidget()
        content_layout = QVBoxLayout(content)
        content_layout.setContentsMargins(0, 0, 8, 0)
        content_layout.setSpacing(12)

        left = QVBoxLayout()
        left.setSpacing(12)
        left.addWidget(self._build_placement_group())
        left.addWidget(self._build_keyboard_group())
        left.addStretch(1)

        right = QVBoxLayout()
        right.setSpacing(12)
        right.addWidget(self._build_motion_group())
        right.addWidget(self._build_hotkey_group())
        right.addStretch(1)

        columns = QHBoxLayout()
        columns.setSpacing(14)
        columns.addLayout(left, 1)
        columns.addLayout(right, 1)
        content_layout.addLayout(columns)
        content_layout.addWidget(self._build_misc_group())
        content_layout.addStretch(1)

        self.scroll = QScrollArea(self)
        self.scroll.setWidgetResizable(True)
        self.scroll.setFrameShape(QFrame.Shape.NoFrame)
        self.scroll.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        self.scroll.setWidget(content)

        self.issues_label = QLabel(self)
        self.issues_label.setObjectName("banner")
        self.issues_label.setWordWrap(True)
        self.issues_label.setVisible(False)

        self.save_button = QPushButton("保存设置", self)
        self.save_button.setProperty("accent", True)
        self.save_button.setToolTip("校验并写入配置文件（非法值会标红）")

        footer = QHBoxLayout()
        footer.setContentsMargins(0, 0, 0, 0)
        footer.setSpacing(8)
        footer.addWidget(self.issues_label, 1)
        footer.addWidget(self.save_button)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(12, 12, 12, 12)
        layout.setSpacing(8)
        # 页面本身已经有“设置”标题，这里不再重复。
        self.title_label.setVisible(False)
        layout.addWidget(hint)
        layout.addWidget(self.scroll, 1)
        layout.addLayout(footer)

    def _make_group(self, title: str) -> tuple[QGroupBox, QVBoxLayout]:
        box = QGroupBox(title, self)
        layout = QVBoxLayout(box)
        layout.setContentsMargins(0, 8, 0, 4)
        layout.setSpacing(8)
        return box, layout

    def _labeled_row(
        self, title: str, widget: QWidget, hint: str | None = None
    ) -> QWidget:
        container = QWidget(self)
        box = QVBoxLayout(container)
        box.setContentsMargins(0, 0, 0, 0)
        box.setSpacing(2)

        row = QWidget(container)
        layout = QHBoxLayout(row)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(8)
        name = QLabel(title, row)
        name.setFixedWidth(_KEY_WIDTH)
        layout.addWidget(name)
        layout.addWidget(widget, 1)
        box.addWidget(row)
        if hint:
            box.addWidget(self._hint(hint))
        return container

    def _hint(self, text: str, indent: int = _KEY_WIDTH + 8) -> QWidget:
        """控件下方的一行说明：小字号、次要色、自动换行。"""
        wrapper = QWidget(self)
        layout = QHBoxLayout(wrapper)
        layout.setContentsMargins(indent, 0, 0, 0)
        layout.setSpacing(0)
        label = QLabel(text, wrapper)
        label.setObjectName("fieldHint")
        label.setWordWrap(True)
        layout.addWidget(label, 1)
        return wrapper

    def _check_row(self, check: QCheckBox, hint: str) -> QWidget:
        """复选框 + 说明，说明与复选框文字左对齐。"""
        container = QWidget(self)
        layout = QVBoxLayout(container)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(2)
        layout.addWidget(check)
        layout.addWidget(self._hint(hint, 24))
        return container

    def _build_placement_group(self) -> QGroupBox:
        box, layout = self._make_group("📍 平板位置")

        self.edge_combo = _NoWheelComboBox(box)
        self.edge_combo.setToolTip("可吸附的虚拟桌面外轮廓边段")
        layout.addWidget(
            self._labeled_row(
                "吸附边",
                self.edge_combo,
                "平板放在电脑屏幕的哪一侧：鼠标推到这条边就会穿进平板。"
                "平板在电脑下面就选「下方」；下拉里只列真正与桌面外轮廓相接的边段。",
            )
        )

        self.align_combo = _NoWheelComboBox(box)
        for text, value in ALIGN_LABELS:
            self.align_combo.addItem(text, value)
        layout.addWidget(
            self._labeled_row(
                "对齐",
                self.align_combo,
                "平板影幕和 PC 屏幕大小不一样时，入口/落点沿边怎么摊："
                "「等比」把 PC 这条边整体映射到平板整条边（两端都走得到），"
                "「居中」只映射中间一段，「1:1」按像素一一对应。一般用「等比」。",
            )
        )

        self.offset_row = SliderRow(
            "偏移", 0, 100, 50, lambda v: f"{v / 100:.2f}", box
        )
        self.offset_slider = self.offset_row.slider
        layout.addWidget(self.offset_row)
        layout.addWidget(
            self._hint(
                "平板影幕沿吸附边前后挪的位置：0.00 贴在边的起点（如屏幕最上端），"
                "1.00 贴在终点。影幕和这条边一样长时调它看不出变化。"
            )
        )

        self.corner_combo = _NoWheelComboBox(box)
        for text, value in CORNER_LABELS:
            self.corner_combo.addItem(text, value)
        self.corner_combo.setToolTip("拖到角落时优先吸附的边（Shift 临时切换）")
        layout.addWidget(
            self._labeled_row(
                "角优先",
                self.corner_combo,
                "鼠标推到两块屏幕交界的角落时，先算作哪条边（决定往左穿还是往下穿）。"
                "「自动」按鼠标更靠近的那条边算；被角卡住时按住 Shift 可临时切到另一边。",
            )
        )

        self._field_widgets["placement.edge"] = self.edge_combo
        self._field_widgets["placement.align"] = self.align_combo
        self._field_widgets["placement.offset"] = self.offset_slider
        self._field_widgets["placement.corner_priority"] = self.corner_combo
        return box

    def _build_motion_group(self) -> QGroupBox:
        box, layout = self._make_group("🖱 手感")

        self.gain_row = SliderRow("速度", 25, 300, 100, lambda v: f"{v / 100:.2f}×", box)
        self.gain_slider = self.gain_row.slider
        layout.addWidget(self.gain_row)
        layout.addWidget(
            self._hint(
                "平板指针的移动倍率：1.00× 表示鼠标在 PC 上走 100 像素、"
                "平板指针也走 100 像素。觉得指针太慢就调大，太飘就调小。"
            )
        )

        self.hz_row = SliderRow("上报频率", C.MIN_REPORT_HZ, C.MAX_REPORT_HZ, 1000,
                                lambda v: f"{v} Hz", box)
        self.hz_slider = self.hz_row.slider
        layout.addWidget(self.hz_row)
        layout.addWidget(self._hint("每秒向平板发多少次指针位置。1000 Hz 最跟手（多数设备的上限）；"
                "无线信号差、延迟大时降到 250–500 Hz 更稳。"))

        self.arm_row = SliderRow("触发阈值", 10, 200, C.ARM_PX, lambda v: f"{v} px", box)
        self.arm_slider = self.arm_row.slider
        layout.addWidget(self.arm_row)
        layout.addWidget(self._hint("鼠标从屏幕边缘继续往外推多少像素才正式接管平板。太小会一蹭就进，"
                "太大又觉得推不进去；40 px 是折中值。"))

        self.exit_row = SliderRow(
            "穿回阈值", 4, 120, C.EXIT_OVERFLOW_PX, lambda v: f"{v} px", box
        )
        self.exit_slider = self.exit_row.slider
        layout.addWidget(self.exit_row)
        layout.addWidget(self._hint("在平板边缘反向拖回多少像素才交还 PC。越大越不容易误退"
                "（在平板边缘点按钮时更稳），越小回 PC 越快。"))

        self._field_widgets["motion.gain"] = self.gain_slider
        self._field_widgets["motion.report_hz"] = self.hz_slider
        self._field_widgets["motion.arm_px"] = self.arm_slider
        self._field_widgets["motion.exit_overflow_px"] = self.exit_slider
        return box

    def _build_keyboard_group(self) -> QGroupBox:
        box, layout = self._make_group("⌨ 键盘")

        self.forward_check = QCheckBox("转发按键到平板", box)
        self.suppress_check = QCheckBox("屏蔽本机按键", box)
        self.layout_combo = _NoWheelComboBox(box)
        for name in LAYOUTS:
            self.layout_combo.addItem(name.upper(), name)

        layout.addWidget(
            self._check_row(
                self.forward_check,
                "接管期间按下的键盘按键会发到平板：可以打字、用平板上的快捷键。",
            )
        )
        layout.addWidget(
            self._check_row(
                self.suppress_check,
                "接管期间把键盘输入从 Windows 拦下来，只发给平板。"
                "关掉时同一个键会在两边各触发一次（例如 Ctrl+W 同时关掉 PC 和平板上的东西）。",
            )
        )
        layout.addWidget(
            self._labeled_row(
                "布局",
                self.layout_combo,
                "平板上当前输入法的键盘布局。要和 Android「设置 → 系统 → 语言和输入法」里"
                "选的一致，否则字符会对不上（例如 @ 和 \" 互换）。",
            )
        )

        self._field_widgets["keyboard.layout"] = self.layout_combo
        return box

    def _build_hotkey_group(self) -> QGroupBox:
        box, layout = self._make_group("⚡ 快捷键")

        self.toggle_edit = QKeySequenceEdit(box)
        self.panic_edit = QKeySequenceEdit(box)
        for edit in (self.toggle_edit, self.panic_edit):
            edit.setClearButtonEnabled(True)

        layout.addWidget(
            self._labeled_row(
                "接管/交还",
                self.toggle_edit,
                "手动进入/退出平板接管：按一次进入（鼠标可在平板上操作），再按一次交还 PC。"
                "也可以不按它，直接把鼠标从屏幕边缘推进平板。",
            )
        )
        layout.addWidget(
            self._labeled_row(
                "急停",
                self.panic_edit,
                "出问题时立即脱身：松开所有按住的键、释放遮罩、把光标还给 PC，不用先退出接管。",
            )
        )

        self._field_widgets["hotkeys.toggle"] = self.toggle_edit
        self._field_widgets["hotkeys.panic"] = self.panic_edit
        return box

    def _build_misc_group(self) -> QGroupBox:
        box, layout = self._make_group("⚙ 杂项")
        self.tray_check = QCheckBox("托盘气泡通知", box)
        self.ghost_check = QCheckBox("显示指针幽灵", box)
        self.close_tray_check = QCheckBox("关闭窗口时最小化到托盘", box)
        self.clipboard_check = QCheckBox("剪贴板同步", box)
        self.keepawake_check = QCheckBox("保持平板亮屏", box)
        left = QVBoxLayout()
        left.setSpacing(4)
        left.addWidget(
            self._check_row(
                self.tray_check,
                "连接成功、断开或出故障时，在右下角托盘弹一个气泡提醒。",
            )
        )
        left.addWidget(
            self._check_row(
                self.ghost_check,
                "在 PC 屏幕边缘画一个半透明小圆点，表示平板指针大概在哪，"
                "方便穿屏前找位置。",
            )
        )
        left.addWidget(
            self._check_row(
                self.close_tray_check,
                "点窗口 X 时：勾选 = 只收进托盘，接管服务继续在后台跑，"
                "要从托盘菜单「退出 PadLend」才真退；不勾选 = 直接退出。",
            )
        )
        right = QVBoxLayout()
        right.setSpacing(4)
        right.addWidget(
            self._check_row(
                self.clipboard_check,
                "勾选后，接管期间 PC 剪贴板会跟着平板走：进入接管时同步一次，"
                "之后每次复制也会自动同步（直接写平板剪贴板，不敲键盘）；"
                "取消勾选就完全不发。",
            )
        )
        right.addWidget(
            self._check_row(
                self.keepawake_check,
                "连接期间不让平板自动熄屏；断开后把平板原来的熄屏设置还回去。",
            )
        )
        columns = QHBoxLayout()
        columns.setSpacing(18)
        columns.addLayout(left, 1)
        columns.addLayout(right, 1)
        layout.addLayout(columns)
        return box

    def _connect(self) -> None:
        self.edge_combo.currentIndexChanged.connect(self._on_control_changed)
        self.align_combo.currentIndexChanged.connect(self._on_control_changed)
        self.corner_combo.currentIndexChanged.connect(self._on_control_changed)
        self.offset_slider.valueChanged.connect(self._on_control_changed)
        for row in (
            self.gain_row,
            self.hz_row,
            self.arm_row,
            self.exit_row,
        ):
            row.slider.valueChanged.connect(self._on_control_changed)
        self.layout_combo.currentIndexChanged.connect(self._on_control_changed)
        for check in (self.forward_check, self.suppress_check, self.tray_check,
                      self.ghost_check, self.close_tray_check, self.clipboard_check,
                      self.keepawake_check):
            check.toggled.connect(self._on_control_changed)
        for edit in (self.toggle_edit, self.panic_edit):
            edit.keySequenceChanged.connect(self._on_control_changed)
        self.save_button.clicked.connect(self.save)

    # ------------------------------------------------------------------ 数据
    def refresh_segments(self) -> None:
        """重新读取 ``desktop.exterior_segments()`` 并尽量保住当前选择。"""
        previous = self.edge_combo.currentData() if self.edge_combo.count() else None
        self._segments = list(self._query_segments())
        self._loading = True
        try:
            self.edge_combo.clear()
            if not self._segments:
                self.edge_combo.addItem("（未检测到可用外轮廓）", None)
                self.edge_combo.setEnabled(False)
            else:
                self.edge_combo.setEnabled(True)
                for segment in self._segments:
                    self.edge_combo.addItem(self.segment_label(segment), segment)
                index = self._index_of(previous)
                if index >= 0:
                    self.edge_combo.setCurrentIndex(index)
        finally:
            self._loading = False
        self._emit_placement()

    def segment_label(self, segment: EdgeSegment) -> str:
        edge = EDGE_LABELS.get(segment.edge, str(segment.edge))
        return f"{self._monitor_name(segment.monitor_handle)} · {edge} · {segment.length}px"

    def current_placement(self) -> Placement | None:
        """当前控件组合出的 ``Placement``（无可用边段时 None）。"""
        return self._build_placement()

    def load_from_config(self) -> None:
        """把 ``Config`` 的值灌进控件（不触发 ``placementChanged``）。"""
        self._loading = True
        try:
            self._fill_placement()
            self._fill_motion()
            self._fill_keyboard()
            self._fill_hotkeys()
            self._fill_misc()
        finally:
            self._loading = False
        self._refresh_value_labels()
        self._show_config_issues()

    # ------------------------------------------------------------------ 保存
    def save(self) -> bool:
        """校验并落盘；成功返回 True 并发 ``configChanged``。"""
        self._collect()
        problems = list(self._config.validate())
        self._mark_invalid(problems)
        if problems:
            self._show_banner("；".join(problems), "err")
            log.warning("config invalid: %s", problems)
            return False
        try:
            self._config.save(self._config_path)
        except OSError as exc:
            log.error("config save failed path=%s err=%r", self._config_path, exc)
            self._show_banner(f"保存失败：{exc}", "err")
            return False
        self._dirty = False
        self._show_banner(f"已保存到 {self._config_path}", "ok")
        log.info("config saved path=%s", self._config_path)
        self.configChanged.emit(self._config)
        return True

    def focus_first(self) -> None:
        self.save_button.setFocus(Qt.FocusReason.ShortcutFocusReason)

    # ------------------------------------------------------------------ 内部
    def _query_segments(self) -> tuple[EdgeSegment, ...]:
        getter = getattr(self._desktop, "exterior_segments", None)
        if not callable(getter):
            log.warning("desktop has no exterior_segments(); placement disabled")
            return ()
        try:
            return tuple(getter())
        except Exception as exc:  # 真实 DesktopMap 可能在枚举 Win32 时失败
            log.warning("desktop.exterior_segments failed: %r", exc)
            return ()

    def _index_of(self, segment: object) -> int:
        if not isinstance(segment, EdgeSegment):
            return -1
        try:
            return self._segments.index(segment)
        except ValueError:
            return -1

    def _monitor_name(self, handle: int) -> str:
        for monitor in getattr(self._desktop, "monitors", ()) or ():
            if getattr(monitor, "handle", None) != handle:
                continue
            name = getattr(monitor, "name", "") or getattr(monitor, "device", "")
            return str(name) or f"#{handle}"
        return f"#{handle}"

    def _monitor_device(self, handle: int) -> str:
        for monitor in getattr(self._desktop, "monitors", ()) or ():
            if getattr(monitor, "handle", None) == handle:
                return str(getattr(monitor, "device", ""))
        return ""

    def _build_placement(self) -> Placement | None:
        segment = self.edge_combo.currentData()
        if not isinstance(segment, EdgeSegment):
            return None
        align_text = str(self.align_combo.currentData() or Align.PROPORTIONAL.value)
        corner_text = str(self.corner_combo.currentData() or "")
        try:
            return Placement(
                segment=segment,
                offset=float(self.offset_slider.value()) / 100.0,
                align=Align(align_text),
                corner_priority=Edge(corner_text) if corner_text else None,
            )
        except ValueError as exc:
            log.warning("placement invalid from widgets: %r", exc)
            return None

    def _collect(self) -> None:
        cfg = self._config
        placement = self._build_placement()
        segment = self.edge_combo.currentData()
        if placement is not None and isinstance(segment, EdgeSegment):
            cfg.placement.monitor = self._monitor_device(segment.monitor_handle)
            cfg.placement.edge = segment.edge.value
            cfg.placement.span_start = segment.span_start
            cfg.placement.span_end = segment.span_end
            cfg.placement.offset = placement.offset
            cfg.placement.align = placement.align.value
            cfg.placement.corner_priority = (
                placement.corner_priority.value
                if placement.corner_priority is not None
                else ""
            )

        motion = cfg.motion
        motion.gain = round(self.gain_slider.value() / 100.0, 2)
        motion.report_hz = int(self.hz_slider.value())
        motion.arm_px = int(self.arm_slider.value())
        motion.exit_overflow_px = int(self.exit_slider.value())

        keyboard = cfg.keyboard
        keyboard.forward = self.forward_check.isChecked()
        keyboard.suppress_pc = self.suppress_check.isChecked()
        keyboard.layout = str(self.layout_combo.currentData() or "us")

        cfg.hotkeys.toggle = self._sequence_text(self.toggle_edit)
        cfg.hotkeys.panic = self._sequence_text(self.panic_edit)

        cfg.ui.tray_notifications = self.tray_check.isChecked()
        cfg.ui.show_pointer_ghost = self.ghost_check.isChecked()
        cfg.ui.close_to_tray = self.close_tray_check.isChecked()
        cfg.misc.clipboard_sync = self.clipboard_check.isChecked()
        cfg.misc.keep_screen_on = self.keepawake_check.isChecked()

    @staticmethod
    def _sequence_text(edit: QKeySequenceEdit) -> str:
        return edit.keySequence().toString(QKeySequence.SequenceFormat.PortableText)

    def _fill_placement(self) -> None:
        cfg = self._config.placement
        index = self._find_segment_index(cfg.monitor, cfg.edge)
        if index >= 0:
            self.edge_combo.setCurrentIndex(index)
        self._select_data(self.align_combo, cfg.align)
        self.offset_slider.setValue(int(round(cfg.offset * 100)))
        self._select_data(self.corner_combo, cfg.corner_priority or "")

    def _find_segment_index(self, monitor: str, edge: str) -> int:
        if not self._segments:
            return -1
        candidates = [
            idx
            for idx, segment in enumerate(self._segments)
            if segment.edge.value == edge
            and (not monitor or self._monitor_device(segment.monitor_handle) == monitor)
        ]
        if not candidates:
            candidates = [
                idx for idx, segment in enumerate(self._segments) if segment.edge.value == edge
            ]
        if not candidates:
            return -1
        # 错位显示器会把同一条边切成多段（例如右侧只剩顶部 132px 的外轮廓）；
        # 默认摆位应选最长段，而不是枚举顺序里的第一段。
        return max(candidates, key=lambda idx: self._segments[idx].length)

    def _fill_motion(self) -> None:
        cfg = self._config.motion
        self.gain_slider.setValue(int(round(cfg.gain * 100)))
        self.hz_slider.setValue(int(cfg.report_hz))
        self.arm_slider.setValue(int(cfg.arm_px))
        self.exit_slider.setValue(int(cfg.exit_overflow_px))

    def _fill_keyboard(self) -> None:
        cfg = self._config.keyboard
        self.forward_check.setChecked(bool(cfg.forward))
        self.suppress_check.setChecked(bool(cfg.suppress_pc))
        self._select_data(self.layout_combo, cfg.layout)

    def _fill_hotkeys(self) -> None:
        cfg = self._config.hotkeys
        self.toggle_edit.setKeySequence(QKeySequence(cfg.toggle))
        self.panic_edit.setKeySequence(QKeySequence(cfg.panic))

    def _fill_misc(self) -> None:
        cfg = self._config
        self.tray_check.setChecked(bool(cfg.ui.tray_notifications))
        self.ghost_check.setChecked(bool(cfg.ui.show_pointer_ghost))
        self.close_tray_check.setChecked(bool(cfg.ui.close_to_tray))
        self.clipboard_check.setChecked(bool(cfg.misc.clipboard_sync))
        self.keepawake_check.setChecked(bool(cfg.misc.keep_screen_on))

    @staticmethod
    def _select_data(combo: QComboBox, value: object) -> None:
        index = combo.findData(value)
        if index >= 0:
            combo.setCurrentIndex(index)

    def _refresh_value_labels(self) -> None:
        for row in (self.offset_row, self.gain_row, self.hz_row, self.arm_row, self.exit_row):
            row.update_label()

    def _on_control_changed(self, *_args: object) -> None:
        self._refresh_value_labels()
        if self._loading:
            return
        self._dirty = True
        self._emit_placement()

    def _emit_placement(self) -> None:
        if self._loading:
            return
        placement = self._build_placement()
        if placement is not None:
            self.placementChanged.emit(placement)

    def _mark_invalid(self, problems: list[str]) -> None:
        for widget in self._field_widgets.values():
            set_invalid(widget, False)
        for problem in problems:
            key = problem.split(" ", 1)[0].strip()
            widget = self._field_widgets.get(key)
            if widget is not None:
                set_invalid(widget, True)

    def _show_config_issues(self) -> None:
        issues = list(getattr(self._config, "issues", ()) or ())
        if issues:
            self._show_banner("；".join(issues), "warn")

    def _show_banner(self, text: str, severity: str) -> None:
        self.issues_label.setText(text)
        self.issues_label.setProperty("severity", severity)
        repolish(self.issues_label)
        self.issues_label.setVisible(bool(text))
