"""矢量线性图标（QPainter 绘制，不依赖图片资源）。

所有图标都在 24×24 的逻辑网格上绘制，按 2× 像素密度输出，因此在
150%/200% 缩放下仍然锐利。颜色由调用方传入，默认使用主题主色。
"""

from __future__ import annotations

import math

from PySide6.QtCore import QPointF, QRectF, Qt
from PySide6.QtGui import (
    QColor,
    QIcon,
    QLinearGradient,
    QPainter,
    QPainterPath,
    QPen,
    QPixmap,
)

from ui.theme import tokens

_GRID = 24.0


def _pen(color: str, width: float = 1.7) -> QPen:
    pen = QPen(QColor(color))
    pen.setWidthF(width)
    pen.setCapStyle(Qt.PenCapStyle.RoundCap)
    pen.setJoinStyle(Qt.PenJoinStyle.RoundJoin)
    return pen


def _draw(name: str, painter: QPainter, color: str) -> None:
    painter.setPen(_pen(color))
    painter.setBrush(Qt.BrushStyle.NoBrush)

    if name == "devices":
        painter.drawRoundedRect(QRectF(2.5, 4.0, 14.0, 10.0), 2.2, 2.2)
        painter.drawLine(QPointF(7.0, 18.0), QPointF(12.0, 18.0))
        painter.drawLine(QPointF(9.5, 14.0), QPointF(9.5, 18.0))
        path = QPainterPath()
        path.addRoundedRect(QRectF(15.2, 9.5, 6.3, 10.5), 1.6, 1.6)
        painter.drawPath(path)
        painter.drawLine(QPointF(17.2, 17.6), QPointF(19.5, 17.6))
    elif name == "canvas":
        painter.drawRoundedRect(QRectF(2.5, 4.0, 12.5, 9.0), 2.0, 2.0)
        painter.drawRoundedRect(QRectF(13.0, 9.5, 8.5, 11.0), 1.8, 1.8)
        pen = _pen(color, 1.3)
        pen.setStyle(Qt.PenStyle.DashLine)
        painter.setPen(pen)
        painter.drawLine(QPointF(12.0, 3.0), QPointF(12.0, 21.0))
    elif name == "settings":
        for y, knob in ((6.5, 15.0), (12.0, 8.0), (17.5, 17.0)):
            painter.drawLine(QPointF(3.0, y), QPointF(21.0, y))
            painter.setBrush(QColor(color))
            painter.drawEllipse(QPointF(knob, y), 2.2, 2.2)
            painter.setBrush(Qt.BrushStyle.NoBrush)
    elif name == "logs":
        for index, width in enumerate((13.0, 16.0, 10.0, 14.0)):
            y = 5.5 + index * 4.4
            painter.setBrush(QColor(color))
            painter.drawEllipse(QPointF(4.2, y), 1.1, 1.1)
            painter.setBrush(Qt.BrushStyle.NoBrush)
            painter.drawLine(QPointF(7.5, y), QPointF(7.5 + width, y))
    elif name == "refresh":
        painter.drawArc(QRectF(4.0, 4.0, 16.0, 16.0), 35 * 16, 285 * 16)
        path = QPainterPath()
        path.moveTo(18.4, 3.4)
        path.lineTo(20.4, 7.6)
        path.lineTo(15.9, 7.9)
        painter.drawPath(path)
    elif name == "plus":
        painter.drawLine(QPointF(12.0, 5.6), QPointF(12.0, 18.4))
        painter.drawLine(QPointF(5.6, 12.0), QPointF(18.4, 12.0))
    elif name == "wifi":
        painter.setBrush(Qt.BrushStyle.NoBrush)
        for radius in (8.6, 5.6, 2.6):
            rect = QRectF(12.0 - radius, 17.8 - radius, radius * 2, radius * 2)
            painter.drawArc(rect, 35 * 16, 110 * 16)
        painter.setBrush(QColor(color))
        painter.drawEllipse(QPointF(12.0, 17.8), 1.4, 1.4)
    elif name == "link":
        painter.drawArc(QRectF(2.8, 8.2, 9.6, 7.6), 90 * 16, 180 * 16)
        painter.drawArc(QRectF(11.6, 8.2, 9.6, 7.6), -90 * 16, 180 * 16)
        painter.drawLine(QPointF(7.8, 12.0), QPointF(16.2, 12.0))
    elif name == "unlink":
        painter.drawArc(QRectF(2.8, 8.2, 9.6, 7.6), 90 * 16, 180 * 16)
        painter.drawArc(QRectF(11.6, 8.2, 9.6, 7.6), -90 * 16, 180 * 16)
        painter.drawLine(QPointF(5.0, 5.0), QPointF(8.0, 8.0))
        painter.drawLine(QPointF(16.0, 16.0), QPointF(19.0, 19.0))
    elif name == "dots":
        painter.setBrush(QColor(color))
        for x in (6.0, 12.0, 18.0):
            painter.drawEllipse(QPointF(x, 12.0), 1.5, 1.5)
    elif name == "minimize":
        painter.drawLine(QPointF(5.0, 16.5), QPointF(19.0, 16.5))
    elif name == "close":
        painter.drawLine(QPointF(6.0, 6.0), QPointF(18.0, 18.0))
        painter.drawLine(QPointF(18.0, 6.0), QPointF(6.0, 18.0))
    elif name == "sun":
        painter.drawEllipse(QPointF(12.0, 12.0), 4.1, 4.1)
        for index in range(8):
            angle = math.radians(index * 45.0)
            inner = 6.9
            outer = 9.6
            painter.drawLine(
                QPointF(12.0 + inner * math.cos(angle), 12.0 + inner * math.sin(angle)),
                QPointF(12.0 + outer * math.cos(angle), 12.0 + outer * math.sin(angle)),
            )
    elif name == "moon":
        path = QPainterPath()
        path.moveTo(15.9, 3.4)
        path.cubicTo(10.6, 4.6, 7.9, 8.0, 7.9, 12.0)
        path.cubicTo(7.9, 16.0, 10.6, 19.4, 15.9, 20.6)
        path.cubicTo(12.1, 18.2, 11.5, 15.2, 11.5, 12.0)
        path.cubicTo(11.5, 8.8, 12.1, 5.8, 15.9, 3.4)
        painter.drawPath(path)
    elif name == "spark":
        path = QPainterPath()
        path.moveTo(12.0, 2.5)
        path.lineTo(14.2, 9.3)
        path.lineTo(21.0, 12.0)
        path.lineTo(14.2, 14.7)
        path.lineTo(12.0, 21.5)
        path.lineTo(9.8, 14.7)
        path.lineTo(3.0, 12.0)
        path.lineTo(9.8, 9.3)
        path.closeSubpath()
        painter.drawPath(path)


def icon_pixmap(name: str, color: str | None = None, size: int = 20) -> QPixmap:
    """返回指定图标的 QPixmap（2× 超采样，带 devicePixelRatio）。"""
    tint = color or tokens.COLORS["text_secondary"]
    scale = 2
    pixmap = QPixmap(size * scale, size * scale)
    pixmap.fill(Qt.GlobalColor.transparent)
    painter = QPainter(pixmap)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
    painter.scale(size * scale / _GRID, size * scale / _GRID)
    _draw(name, painter, tint)
    painter.end()
    pixmap.setDevicePixelRatio(scale)
    return pixmap


def make_icon(name: str, color: str | None = None, size: int = 20) -> QIcon:
    """返回 QIcon；同一图标可被多个按钮复用。"""
    return QIcon(icon_pixmap(name, color, size))


def brand_pixmap(size: int = 34) -> QPixmap:
    """品牌标记：深色圆角底 + 渐变折线，用于顶栏/向导/任务栏图标。"""
    scale = 2
    pixmap = QPixmap(size * scale, size * scale)
    pixmap.fill(Qt.GlobalColor.transparent)
    painter = QPainter(pixmap)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
    painter.scale(size * scale / _GRID, size * scale / _GRID)

    painter.setPen(Qt.PenStyle.NoPen)
    gradient = QLinearGradient(0.0, 0.0, 24.0, 24.0)
    gradient.setColorAt(0.0, QColor("#5B8CFF"))
    gradient.setColorAt(1.0, QColor("#8B6CFF"))
    painter.setBrush(gradient)
    painter.drawRoundedRect(QRectF(0.8, 0.8, 22.4, 22.4), 7.0, 7.0)

    pen = _pen("#FFFFFF", 2.5)
    painter.setPen(pen)
    path = QPainterPath()
    path.moveTo(8.2, 19.2)
    path.lineTo(8.2, 6.2)
    path.lineTo(13.2, 6.2)
    path.quadTo(17.2, 6.2, 17.2, 10.2)
    path.quadTo(17.2, 14.2, 13.2, 14.2)
    path.lineTo(8.2, 14.2)
    painter.drawPath(path)

    painter.setPen(Qt.PenStyle.NoPen)
    painter.setBrush(QColor(255, 255, 255, 220))
    painter.drawEllipse(QPointF(16.4, 18.2), 1.5, 1.5)
    painter.end()
    pixmap.setDevicePixelRatio(scale)
    return pixmap


def brand_icon() -> QIcon:
    """窗口/任务栏图标：多尺寸品牌标（标题栏不要再用状态色圆点）。"""
    icon = QIcon()
    for size in (16, 20, 24, 32, 48, 64, 128, 256):
        icon.addPixmap(brand_pixmap(size))
    return icon
