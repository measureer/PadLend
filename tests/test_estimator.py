"""CursorEstimator 单测（设计文档 6.4 / 8.2）。"""

from __future__ import annotations

import pytest

import constants as C
from core.geometry import Edge
from tablet.cursor import CursorEstimator


def test_initial_position_and_clamp():
    est = CursorEstimator(3200, 2136, 1.312)
    assert est.position == (0.0, 0.0)
    est.clamp()
    assert est.position == (0.0, 0.0)
    assert est.width == 3200 and est.height == 2136


def test_feed_scales_by_xscale():
    est = CursorEstimator(3200, 2136, 2.0)
    cx, cy = est.feed(10.0, 5.0, gain=1.0)
    assert (cx, cy) == (10, 5)
    assert est.position == (20.0, 10.0)


def test_gain_applies_before_integer_step():
    est = CursorEstimator(3200, 2136, 1.0)
    assert est.feed(10.0, 0.0, gain=0.5) == (5, 0)
    assert est.position == (5.0, 0.0)


def test_residual_preserves_subpixel_motion():
    est = CursorEstimator(3200, 2136, 1.0)
    reports = [est.feed(0.4, 0.0, gain=1.0) for _ in range(3)]
    # 0.4*3 = 1.2 -> 第三次才凑够 1 个计数，残差 0.2 保留
    assert reports == [(0, 0), (0, 0), (1, 0)]
    assert est.position == (1.0, 0.0)
    assert est.resid[0] == pytest.approx(0.2, abs=1e-9)


def test_hid_clamp():
    est = CursorEstimator(3200, 2136, 1.0)
    cx, cy = est.feed(10_000.0, -10_000.0, gain=1.0)
    assert cx == C.HID_CLAMP and cy == -C.HID_CLAMP
    # 位置随后被 clamp 到屏幕内（Android PointerController 同样会夹紧）
    assert est.position == (127.0, 0.0)


def test_position_is_always_clamped():
    est = CursorEstimator(100, 80, 2.0)
    for _ in range(10):
        est.feed(127.0, 127.0, gain=1.0)
    assert est.x <= 99.0 and est.y <= 79.0
    assert est.hits() == Edge.RIGHT  # 优先级 LEFT > RIGHT > TOP > BOTTOM

    for _ in range(20):
        est.feed(-127.0, -127.0, gain=1.0)
    assert est.x >= 0.0 and est.y >= 0.0
    assert est.hits() == Edge.LEFT


def test_hits_each_edge():
    est = CursorEstimator(100, 80, 1.0)

    est.x, est.y = 0.0, 40.0
    assert est.hits() is Edge.LEFT
    est.x, est.y = 99.0, 40.0
    assert est.hits() is Edge.RIGHT
    est.x, est.y = 50.0, 0.0
    assert est.hits() is Edge.TOP
    est.x, est.y = 50.0, 79.0
    assert est.hits() is Edge.BOTTOM
    est.x, est.y = 50.0, 40.0
    assert est.hits() is None
    # 设计文档 6.4.2 的 hit() 别名（类属性同一对象）
    assert CursorEstimator.hit is CursorEstimator.hits


def test_reset_default_centers_and_clears_residual():
    est = CursorEstimator(3200, 2136, 1.312)
    est.feed(50.0, 50.0, gain=1.0)
    est.reset()
    assert est.position == ((3200 - 1) / 2.0, (2136 - 1) / 2.0)
    assert est.resid == [0.0, 0.0]


def test_warp_to_moves_and_returns_reports():
    est = CursorEstimator(3200, 2136, 1.312)
    est.reset(1600.0, 1068.0)
    reports = est.warp_to(0.0, 0.0)
    assert reports, "远距离 warp 必须产生报文"
    assert len(reports) <= 64
    # 到达精度受整数计数 × k 限制
    assert abs(est.x - 0.0) <= est.k
    assert abs(est.y - 0.0) <= est.k
    assert 0.0 <= est.x <= 3199.0 and 0.0 <= est.y <= 2135.0
    assert all(-C.HID_CLAMP <= cx <= C.HID_CLAMP for cx, cy in reports)
    assert all(-C.HID_CLAMP <= cy <= C.HID_CLAMP for cx, cy in reports)


def test_warp_to_same_position_is_noop():
    est = CursorEstimator(1000, 1000, 1.0)
    est.reset(10.0, 10.0)
    assert est.warp_to(10.0, 10.0) == []
    assert est.position == (10.0, 10.0)


def test_warp_to_clamps_target():
    est = CursorEstimator(100, 100, 1.0)
    est.warp_to(500.0, -500.0)
    assert 0.0 <= est.x <= 99.0
    assert 0.0 <= est.y <= 99.0
