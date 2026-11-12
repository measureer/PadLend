import { describe, expect, it } from 'vitest';
import type { Monitor, Placement, Segment } from '../api/types';
import {
  clamp01,
  monitorForSegment,
  nearbySegments,
  padded,
  pointSegmentDistance,
  resolveCorner,
  segmentLine,
  segmentOffset,
  tabletRect,
  unionRect,
} from '../lib/geometry';

const monitor: Monitor = {
  handle: 1,
  device: 'DISPLAY1',
  name: 'DISPLAY1',
  rect: { left: -1920, top: 0, right: 0, bottom: 1080, width: 1920, height: 1080 },
  dpi: 96,
  scale: 1,
  primary: true,
};

const right: Segment = { monitorHandle: 1, edge: 'right', spanStart: 0, spanEnd: 1080, length: 1080 };
const top: Segment = { monitorHandle: 1, edge: 'top', spanStart: -1920, spanEnd: 0, length: 1920 };
const placement: Placement = {
  monitorHandle: 1, edge: 'right', spanStart: 0, spanEnd: 1080,
  offset: 0.5, align: 'proportional', cornerPriority: null,
};

describe('画布几何（与 core/geometry.py 保持一致）', () => {
  it('负坐标显示器也能算出正确的边段', () => {
    expect(segmentLine(right, monitor)).toEqual({ x1: 0, y1: 0, x2: 0, y2: 1080 });
    expect(segmentLine(top, monitor)).toEqual({ x1: -1920, y1: 0, x2: 0, y2: 0 });
  });

  it('点到线段的距离按线段（不是无限直线）计算', () => {
    expect(pointSegmentDistance(right, monitor, 10, 540)).toBeCloseTo(10);
    expect(pointSegmentDistance(right, monitor, 10, 2000)).toBeCloseTo(Math.hypot(10, 920));
  });

  it('offset 落在 0..1 且中点为 0.5', () => {
    expect(clamp01(1.4)).toBe(1);
    expect(segmentOffset(right, monitor, 0, 540)).toBeCloseTo(0.5);
    expect(segmentOffset(right, monitor, 0, 5000)).toBe(1);
  });

  it('平板贴在右边缘外侧，段上投影对准平板中心', () => {
    const rect = tabletRect(placement, monitor, 3200, 2136);
    expect(rect.left).toBe(0);
    expect(rect.top).toBe(540 - 2136 / 2);
    expect(rect.width).toBe(3200);
  });

  it('只返回吸附带内的边段，找不到时退回最近一条', () => {
    const found = nearbySegments([right, top], [monitor], 10, 500);
    expect(found[0].segment).toBe(right);
    const fallback = nearbySegments([right], [monitor], 5000, 500);
    expect(fallback).toHaveLength(1);
  });

  it('角落按外推方向裁决', () => {
    expect(resolveCorner(right, top, [80, 0], null).edge).toBe('right');
    expect(resolveCorner(right, top, [80, 0], 'top').edge).toBe('top');
  });

  it('包围盒与留白覆盖所有显示器和平板', () => {
    const box = padded(unionRect([monitor.rect, tabletRect(placement, monitor, 3200, 2136)]));
    expect(box.left).toBeLessThan(monitor.rect.left);
    expect(box.right).toBeGreaterThan(3200);
  });

  it('找不到显示器时返回 undefined 而不是抛异常', () => {
    expect(monitorForSegment([monitor], 999)).toBeUndefined();
  });
});
