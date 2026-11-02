"""平板指针位置估计器（设计文档 6.4 / 8.2，冻结契约）。

平板是**相对设备**，且系统不暴露指针坐标（F5），所以位置只能由 PC 端镜像。
换算链（6.4.1）::

    PC 物理像素位移 Δp --×gain--> HID 相对计数 c（整数，clamp ±127）--×XScale--> 平板像素 Δx

关键不变量（I3）：每次 feed 之后立即 clamp，位置恒在 [0, W-1] × [0, H-1]。
这与 Android PointerController 的夹紧行为对齐，否则估计器会越走越偏。
"""

from __future__ import annotations

import constants as C
from core.geometry import Edge


class CursorEstimator:
    """平板指针位置的唯一真相源（只在 Router 线程访问，无需加锁）。"""

    def __init__(self, width: int, height: int, xscale: float):
        if width <= 0 or height <= 0:
            raise ValueError(f"invalid tablet size {width}x{height}")
        self.W = float(width)
        self.H = float(height)
        self.k = float(xscale) if xscale > 0 else 1.0
        # 6.4.2：初始在 (0,0)；真实入口位置由 Router 用 warp_to 摆放。
        self.x = 0.0
        self.y = 0.0
        # 小数残差，防慢速拖动丢步（D12：不丢老数据）
        self.resid = [0.0, 0.0]
        # 最近一次实际发出的 HID 计数，诊断用
        self.last_report = (0, 0)

    # -- 位置 --------------------------------------------------------------
    @property
    def position(self) -> tuple[float, float]:
        return (self.x, self.y)

    @property
    def width(self) -> int:
        return int(self.W)

    @property
    def height(self) -> int:
        return int(self.H)

    def reset(self, x: float | None = None, y: float | None = None) -> None:
        """重置位置。缺省归中（平板旋转/重启后指针位置未知，6.4.3）。"""
        if x is None:
            x = (self.W - 1.0) / 2.0
        if y is None:
            y = (self.H - 1.0) / 2.0
        self.x = float(x)
        self.y = float(y)
        self.resid[0] = 0.0
        self.resid[1] = 0.0
        self.clamp()

    def clamp(self) -> None:
        """与系统行为对齐：PointerController 也会夹紧，我们不夹就会越走越偏。"""
        self.x = min(max(self.x, 0.0), self.W - 1.0)
        self.y = min(max(self.y, 0.0), self.H - 1.0)

    # -- 换算 --------------------------------------------------------------
    def feed(
        self, dpx: float, dpy: float, gain: float, *, clamp: int = C.HID_CLAMP
    ) -> tuple[int, int]:
        """PC 物理像素位移 → 一帧 HID 相对计数（±clamp）。

        残差保留在 ``resid`` 中，慢速细碎移动不会丢步。
        """
        clamp = max(1, min(int(clamp), C.HID_CLAMP))
        self.resid[0] += dpx * gain
        self.resid[1] += dpy * gain

        cx = int(self.resid[0])
        cy = int(self.resid[1])
        self.resid[0] -= cx
        self.resid[1] -= cy

        cx = max(-clamp, min(clamp, cx))
        cy = max(-clamp, min(clamp, cy))

        self.x += cx * self.k
        self.y += cy * self.k
        self.clamp()
        self.last_report = (cx, cy)
        return cx, cy

    def hits(self) -> Edge | None:
        """估计指针是否已抵达平板边界；多个边界同时命中时按 6.4.2 的优先级。"""
        if self.x <= 0.0:
            return Edge.LEFT
        if self.x >= self.W - 1.0:
            return Edge.RIGHT
        if self.y <= 0.0:
            return Edge.TOP
        if self.y >= self.H - 1.0:
            return Edge.BOTTOM
        return None

    # 设计文档 6.5.3 用 hit()，契约 8.2 用 hits()；两个名字都保留。
    hit = hits

    def warp_to(self, tx: float, ty: float, *, max_reports: int = 64) -> list[tuple[int, int]]:
        """相对设备做"绝对定位"：穿屏时把平板指针搬到入口点（6.4.2）。

        返回需要一次性 flush 的 HID 报告列表（26 个报文 ≈ 3ms）。
        """
        out: list[tuple[int, int]] = []
        tx = min(max(float(tx), 0.0), self.W - 1.0)
        ty = min(max(float(ty), 0.0), self.H - 1.0)
        for _ in range(max_reports):
            dx, dy = tx - self.x, ty - self.y
            if abs(dx) < self.k and abs(dy) < self.k:
                break
            cx = max(-C.HID_CLAMP, min(C.HID_CLAMP, int(dx / self.k)))
            cy = max(-C.HID_CLAMP, min(C.HID_CLAMP, int(dy / self.k)))
            if cx == 0 and cy == 0:
                # k > 1 时残余小于 k 的整数步长无法表达；交给 clamp 收敛
                break
            self.x += cx * self.k
            self.y += cy * self.k
            out.append((cx, cy))
        self.clamp()
        if out:
            self.last_report = out[-1]
        return out

    # -- 诊断 --------------------------------------------------------------
    def __repr__(self) -> str:  # pragma: no cover - 调试输出
        return (
            f"CursorEstimator(W={self.W:g}, H={self.H:g}, k={self.k:g}, "
            f"pos=({self.x:.1f},{self.y:.1f}))"
        )
