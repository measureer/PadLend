/** 画布几何：从 core/geometry.py 与 ui/canvas_items.py 移植的纯函数。
 * 前后端共用同一套公式，预览位置与实际穿越范围不会出现偏差。 */

import type { Edge, Monitor, Placement, Rect, Segment } from '../api/types';

export interface Line {
  x1: number;
  y1: number;
  x2: number;
  y2: number;
}

export const clamp01 = (value: number): number => Math.min(1, Math.max(0, value));

export const OUTWARD: Record<Edge, [number, number]> = {
  left: [-1, 0],
  right: [1, 0],
  top: [0, -1],
  bottom: [0, 1],
};

export const isVertical = (edge: Edge): boolean => edge === 'left' || edge === 'right';

export function segmentLine(segment: Segment, monitor: Monitor): Line {
  const r = monitor.rect;
  switch (segment.edge) {
    case 'left':
      return { x1: r.left, y1: segment.spanStart, x2: r.left, y2: segment.spanEnd };
    case 'right':
      return { x1: r.right, y1: segment.spanStart, x2: r.right, y2: segment.spanEnd };
    case 'top':
      return { x1: segment.spanStart, y1: r.top, x2: segment.spanEnd, y2: r.top };
    case 'bottom':
      return { x1: segment.spanStart, y1: r.bottom, x2: segment.spanEnd, y2: r.bottom };
  }
}

export function pointSegmentDistance(segment: Segment, monitor: Monitor, x: number, y: number): number {
  const line = segmentLine(segment, monitor);
  if (isVertical(segment.edge)) {
    const cy = Math.min(Math.max(y, line.y1), line.y2);
    return Math.hypot(x - line.x1, y - cy);
  }
  const cx = Math.min(Math.max(x, line.x1), line.x2);
  return Math.hypot(x - cx, y - line.y1);
}

/** 光标在段上的投影点 → 0..1 相对落点。 */
export function segmentOffset(segment: Segment, _monitor: Monitor, x: number, y: number): number {
  const along = isVertical(segment.edge) ? y : x;
  return clamp01((along - segment.spanStart) / Math.max(segment.length, 1));
}

/** 平板矩形（虚拟桌面物理像素）。贴边一侧长度 = 平板高/宽。 */
export function tabletRect(
  placement: Placement,
  monitor: Monitor,
  tabletW: number,
  tabletH: number,
): Rect {
  const r = monitor.rect;
  const along = placement.spanStart + clamp01(placement.offset) * Math.max(placement.spanEnd - placement.spanStart, 1);
  if (isVertical(placement.edge)) {
    const left = placement.edge === 'left' ? r.left - tabletW : r.right;
    const top = along - tabletH / 2;
    return { left, top, right: left + tabletW, bottom: top + tabletH, width: tabletW, height: tabletH };
  }
  const top = placement.edge === 'top' ? r.top - tabletH : r.bottom;
  const left = along - tabletW / 2;
  return { left, top, right: left + tabletW, bottom: top + tabletH, width: tabletW, height: tabletH };
}

export function monitorForSegment(monitors: Monitor[], handle: number): Monitor | undefined {
  return monitors.find((m) => m.handle === handle);
}

export interface ScoredSegment {
  segment: Segment;
  monitor: Monitor;
  distance: number;
}

/** 拖动时只对附近的边段打分，避免每帧遍历并重建整个场景。 */
export function nearbySegments(
  segments: Segment[],
  monitors: Monitor[],
  x: number,
  y: number,
  band = 64,
): ScoredSegment[] {
  const scored: ScoredSegment[] = [];
  for (const segment of segments) {
    const monitor = monitorForSegment(monitors, segment.monitorHandle);
    if (!monitor) continue;
    scored.push({ segment, monitor, distance: pointSegmentDistance(segment, monitor, x, y) });
  }
  scored.sort((a, b) => a.distance - b.distance);
  const within = scored.filter((s) => s.distance <= band);
  return within.length ? within : scored.slice(0, 1);
}

/** 角落裁决：按外推方向与优先边选择该贴哪条边。 */
export function resolveCorner(
  first: Segment,
  second: Segment,
  delta: [number, number],
  priority: Edge | null,
): Segment {
  const score = (segment: Segment): number => {
    const [nx, ny] = OUTWARD[segment.edge];
    return delta[0] * nx + delta[1] * ny;
  };
  const sorted = [first, second].sort((a, b) => score(b) - score(a));
  if (priority && (sorted[0].edge === priority || sorted[1].edge === priority)) {
    return sorted.find((s) => s.edge === priority) ?? sorted[0];
  }
  const [bestScore, nextScore] = [score(sorted[0]), score(sorted[1])];
  if (bestScore > 0 && (bestScore - nextScore) / Math.max(bestScore, 1) > 0.15) return sorted[0];
  return sorted[0];
}

export function unionRect(rects: Rect[]): Rect {
  if (!rects.length) return { left: 0, top: 0, right: 1920, bottom: 1080, width: 1920, height: 1080 };
  const left = Math.min(...rects.map((r) => r.left));
  const top = Math.min(...rects.map((r) => r.top));
  const right = Math.max(...rects.map((r) => r.right));
  const bottom = Math.max(...rects.map((r) => r.bottom));
  return { left, top, right, bottom, width: right - left, height: bottom - top };
}

export function padded(rect: Rect, ratio = 0.06, min = 24): Rect {
  const margin = Math.max(min, ratio * Math.max(rect.width, rect.height));
  return {
    left: rect.left - margin,
    top: rect.top - margin,
    right: rect.right + margin,
    bottom: rect.bottom + margin,
    width: rect.width + margin * 2,
    height: rect.height + margin * 2,
  };
}
