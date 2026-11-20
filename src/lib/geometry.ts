/** 画布几何：与 padlend-core/src/logic/geometry.rs 共用同一套公式的纯函数
 * （历史：从 Python 时代的 core/geometry.py 与 ui/canvas_items.py 移植）。
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
export function segmentOffset(segment: Segment, x: number, y: number): number {
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

/** 全量打分后按距离排序，取吸附带内的边段（没有则退回最近一条）。
 * 边段数量小（每屏只有几条），每帧全量打分没有性能问题。 */
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

/** 方向足够明确的阈值：最高分要领先次高分 15% 以上（与引擎 CORNER_DIRECTION_RATIO 一致）。 */
export const CORNER_DIRECTION_RATIO = 0.15;

/**
 * 角落裁决：规则以引擎 `DesktopMap::resolve_corner` 为准，两边算出的吸附边必须一致。
 * 1. 拖动方向足够明确（最高分 > 0 且领先次高分 15% 以上）→ 按方向；
 * 2. 否则用优先边（Shift 切换或保存的 corner_priority）；
 * 3. 再否则取最高分（同分保持输入顺序）。
 * 注意顺序：方向明确时优先边不生效，旧实现先看 priority，和引擎正好相反。
 */
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
  const scored = [first, second]
    .map((segment) => ({ segment, score: score(segment) }))
    .sort((a, b) => b.score - a.score);
  const best = scored[0];
  const nextScore = scored[1]?.score ?? 0;
  if (best.score > 0 && (best.score - nextScore) / Math.max(best.score, 1) > CORNER_DIRECTION_RATIO) {
    return best.segment;
  }
  if (priority) {
    const preferred = scored.find((item) => item.segment.edge === priority);
    if (preferred) return preferred.segment;
  }
  return best.segment;
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
