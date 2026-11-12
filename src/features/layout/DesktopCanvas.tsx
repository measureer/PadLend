/** 桌面布局画布（SVG）。
 *
 * 性能要点：
 * - 拖动/平移/缩放只改 DOM transform，不触发 React 重渲染（旧版每次移动都重建 QGraphicsScene）；
 * - pointermove 用 rAF 合帧；主题色走 CSS 变量，不逐帧解析调色板；
 * - 只在 pointerup 提交一次 Placement，父组件才重渲染；
 * - 视野只在显示器变化 / 用户点「显示全部」时重算，拖动不会重置缩放。
 */

import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { Align, Desktop, DeviceFacts, Edge, Placement } from '../../api/types';
import { toggleManual } from '../../lib/actions';
import {
  isVertical,
  monitorForSegment,
  nearbySegments,
  padded,
  resolveCorner,
  segmentLine,
  segmentOffset,
  tabletRect,
  unionRect,
  type ScoredSegment,
} from '../../lib/geometry';

const SNAP_BAND = 64;
const MIN_ZOOM = 0.02;
const MAX_ZOOM = 8;
const DEFAULT_TABLET = { width: 3200, height: 2136 };

interface View { k: number; tx: number; ty: number }

interface Props {
  desktop: Desktop;
  placement: Placement | null;
  facts: DeviceFacts | null;
  fitNonce: number;
  onPlacementChange: (placement: Placement | null) => void;
}

function segKey(segment: { monitorHandle: number; edge: Edge; spanStart: number; spanEnd: number }): string {
  return `${segment.monitorHandle}:${segment.edge}:${segment.spanStart}:${segment.spanEnd}`;
}

export const DesktopCanvas = memo(function DesktopCanvas({ desktop, placement, facts, fitNonce, onPlacementChange }: Props) {
  const frameRef = useRef<HTMLDivElement | null>(null);
  const svgRef = useRef<SVGSVGElement | null>(null);
  const viewportRef = useRef<SVGGElement | null>(null);
  const tabletRef = useRef<SVGGElement | null>(null);
  const viewRef = useRef<View>({ k: 1, tx: 0, ty: 0 });
  const dragRef = useRef<{ originX: number; originY: number; placement: Placement; corner: Edge | null; shiftHandled: boolean } | null>(null);
  const panRef = useRef<{ x: number; y: number; tx: number; ty: number } | null>(null);
  const rafRef = useRef(0);
  const pendingRef = useRef<{ x: number; y: number } | null>(null);
  const spaceRef = useRef(false);
  const shiftRef = useRef(false);
  const tabletSizeRef = useRef({ ...DEFAULT_TABLET });
  const [sizeNonce, setSizeNonce] = useState(0);
  const [fit, setFit] = useState({ x: 0, y: 0, w: 4480, h: 1600 });
  const [ctx, setCtx] = useState<{ x: number; y: number } | null>(null);
  const [dragging, setDragging] = useState(false);

  const tablet = useMemo(() => {
    void sizeNonce;
    if (!placement) return null;
    const monitor = monitorForSegment(desktop.monitors, placement.monitorHandle);
    if (!monitor) return null;
    return tabletRect(placement, monitor, tabletSizeRef.current.width, tabletSizeRef.current.height);
  }, [placement, desktop.monitors, sizeNonce]);

  const applyView = useCallback(() => {
    const { k, tx, ty } = viewRef.current;
    viewportRef.current?.setAttribute('transform', `translate(${tx} ${ty}) scale(${k})`);
  }, []);

  const fitView = useCallback(() => {
    const monitorRects = desktop.monitors.map((m) => m.rect);
    const base = unionRect(monitorRects.length ? monitorRects : [{ left: 0, top: 0, right: 1920, bottom: 1080, width: 1920, height: 1080 }]);
    const box = padded(unionRect(tablet ? [base, tablet] : [base]));
    setFit({ x: box.left, y: box.top, w: box.width, h: box.height });
    viewRef.current = { k: 1, tx: 0, ty: 0 };
    applyView();
  }, [desktop.monitors, tablet, applyView]);

  const fitRef = useRef(fitView);
  fitRef.current = fitView;
  useEffect(() => { fitRef.current(); }, [fitNonce, desktop.monitors]);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === ' ') spaceRef.current = true;
      if (e.key === 'Shift') shiftRef.current = true;
      if (e.ctrlKey && e.key === '0') { e.preventDefault(); fitRef.current(); }
    };
    const onKeyUp = (e: KeyboardEvent) => {
      if (e.key === ' ') spaceRef.current = false;
      if (e.key === 'Shift') shiftRef.current = false;
    };
    const svg = svgRef.current;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const ctm = svg?.getScreenCTM();
      if (!ctm) return;
      const point = new DOMPoint(e.clientX, e.clientY).matrixTransform(ctm.inverse());
      const view = viewRef.current;
      const sceneX = (point.x - view.tx) / view.k;
      const sceneY = (point.y - view.ty) / view.k;
      const k = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, view.k * Math.pow(1.0015, -e.deltaY)));
      viewRef.current = { k, tx: point.x - k * sceneX, ty: point.y - k * sceneY };
      applyView();
    };
    window.addEventListener('keydown', onKeyDown);
    window.addEventListener('keyup', onKeyUp);
    svg?.addEventListener('wheel', onWheel, { passive: false });
    return () => {
      window.removeEventListener('keydown', onKeyDown);
      window.removeEventListener('keyup', onKeyUp);
      svg?.removeEventListener('wheel', onWheel);
      cancelAnimationFrame(rafRef.current);
    };
  }, [applyView]);

  /** 屏幕坐标 → viewBox 坐标 → 场景坐标（含缩放/平移）。 */
  const toScene = useCallback((clientX: number, clientY: number): { sceneX: number; sceneY: number; vx: number; vy: number } => {
    const ctm = svgRef.current?.getScreenCTM();
    if (!ctm) return { sceneX: 0, sceneY: 0, vx: 0, vy: 0 };
    const point = new DOMPoint(clientX, clientY).matrixTransform(ctm.inverse());
    const { k, tx, ty } = viewRef.current;
    return { sceneX: (point.x - tx) / k, sceneY: (point.y - ty) / k, vx: point.x, vy: point.y };
  }, []);

  const highlight = useCallback((chosen: Placement | null) => {
    const svg = svgRef.current;
    if (!svg) return;
    const wanted = chosen ? segKey(chosen) : '';
    svg.querySelectorAll<SVGLineElement>('.canvas-edge').forEach((line) => {
      line.setAttribute('class', `canvas-edge exterior${line.dataset.key === wanted ? ' attached' : ''}`);
    });
  }, []);

  const computeSnap = useCallback((sceneX: number, sceneY: number, current: Placement, corner: Edge | null, delta: [number, number]): Placement => {
    const scored = nearbySegments(desktop.segments, desktop.monitors, sceneX, sceneY, SNAP_BAND);
    if (!scored.length) return current;
    let chosen: ScoredSegment = scored[0];
    const other = scored.find((item) => item.segment.edge !== chosen.segment.edge && item.distance <= SNAP_BAND);
    if (other) {
      const segment = resolveCorner(chosen.segment, other.segment, delta, corner);
      chosen = scored.find((item) => item.segment === segment) ?? chosen;
    }
    const { segment, monitor } = chosen;
    return {
      monitorHandle: segment.monitorHandle,
      edge: segment.edge,
      spanStart: segment.spanStart,
      spanEnd: segment.spanEnd,
      offset: segmentOffset(segment, monitor, sceneX, sceneY),
      align: current.align,
      cornerPriority: corner,
    };
  }, [desktop.segments, desktop.monitors]);

  const preview = useCallback((next: Placement | null) => {
    if (!next) return;
    const monitor = monitorForSegment(desktop.monitors, next.monitorHandle);
    if (!monitor) return;
    const rect = tabletRect(next, monitor, tabletSizeRef.current.width, tabletSizeRef.current.height);
    tabletRef.current?.setAttribute('transform', `translate(${rect.left} ${rect.top})`);
    highlight(next);
  }, [desktop.monitors, highlight]);

  const onPointerMove = useCallback((event: React.PointerEvent<SVGSVGElement>) => {
    pendingRef.current = { x: event.clientX, y: event.clientY };
    if (rafRef.current) return;
    rafRef.current = requestAnimationFrame(() => {
      rafRef.current = 0;
      const point = pendingRef.current;
      if (!point) return;
      if (panRef.current) {
        const p = panRef.current;
        viewRef.current = { ...viewRef.current, tx: p.tx + (point.x - p.x), ty: p.ty + (point.y - p.y) };
        applyView();
        return;
      }
      const drag = dragRef.current;
      if (!drag) return;
      const { sceneX, sceneY } = toScene(point.x, point.y);
      if (shiftRef.current && !drag.shiftHandled) {
        const candidates = nearbySegments(desktop.segments, desktop.monitors, sceneX, sceneY, SNAP_BAND);
        const edges = Array.from(new Set(candidates.map((c) => c.segment.edge)));
        if (edges.length) {
          const index = drag.corner ? edges.indexOf(drag.corner) : -1;
          drag.corner = edges[(index + 1) % edges.length] ?? null;
        }
        drag.shiftHandled = true;
      }
      const next = computeSnap(sceneX, sceneY, drag.placement, drag.corner, [sceneX - drag.originX, sceneY - drag.originY]);
      dragRef.current = { ...drag, placement: next };
      preview(next);
    });
  }, [applyView, computeSnap, desktop.monitors, desktop.segments, preview, toScene]);

  const onPointerDown = useCallback((event: React.PointerEvent<SVGSVGElement>) => {
    if (event.button !== 0) return;
    setCtx(null);
    const { sceneX, sceneY } = toScene(event.clientX, event.clientY);
    if (spaceRef.current) {
      panRef.current = { x: event.clientX, y: event.clientY, tx: viewRef.current.tx, ty: viewRef.current.ty };
      event.currentTarget.setPointerCapture(event.pointerId);
      return;
    }
    if (!placement) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    dragRef.current = { originX: sceneX, originY: sceneY, placement, corner: placement.cornerPriority, shiftHandled: false };
    setDragging(true);
  }, [placement, toScene]);

  const onPointerUp = useCallback((event: React.PointerEvent<SVGSVGElement>) => {
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    panRef.current = null;
    const drag = dragRef.current;
    dragRef.current = null;
    setDragging(false);
    if (drag) onPlacementChange(drag.placement);
  }, [onPlacementChange]);

  const setAlign = (align: Align) => {
    if (!placement) return;
    onPlacementChange({ ...placement, align });
    setCtx(null);
  };

  const tabletLabel = facts ? `${facts.width} × ${facts.height}` : '未连接设备';
  const tabletMeta = facts ? `${facts.orientation === 'landscape' ? '横屏' : '竖屏'} · 拖动吸附` : '连接后显示实际尺寸';
  const size = tabletSizeRef.current;

  return (
    <div className="canvas-frame" ref={frameRef}>
      <svg
        ref={svgRef}
        className={`canvas-svg ${dragging ? 'dragging' : ''}`}
        viewBox={`${fit.x} ${fit.y} ${fit.w} ${fit.h}`}
        preserveAspectRatio="xMidYMid meet"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
        onContextMenu={(e) => {
          e.preventDefault();
          const rect = frameRef.current?.getBoundingClientRect();
          setCtx({ x: e.clientX - (rect?.left ?? 0), y: e.clientY - (rect?.top ?? 0) });
        }}
      >
        <g ref={viewportRef}>
          {desktop.monitors.map((monitor) => {
            const r = monitor.rect;
            return (
              <g key={monitor.handle}>
                <rect className="canvas-monitor" x={r.left} y={r.top} width={r.width} height={r.height} rx={28} />
                <text className="canvas-monitor-label" x={r.left + 110} y={r.top + 190}>
                  {monitor.name}{monitor.primary ? ' · 主屏' : ''}
                </text>
                <text className="canvas-monitor-meta" x={r.left + 110} y={r.top + 300}>
                  {r.width}×{r.height} · 缩放 {Math.round(monitor.scale * 100)}% · DPI {monitor.dpi}
                </text>
              </g>
            );
          })}

          {desktop.segments.map((segment) => {
            const monitor = monitorForSegment(desktop.monitors, segment.monitorHandle);
            if (!monitor) return null;
            const line = segmentLine(segment, monitor);
            const attached = placement && segKey(placement) === segKey(segment);
            return (
              <line
                key={segKey(segment)}
                data-key={segKey(segment)}
                className={`canvas-edge exterior${attached ? ' attached' : ''}`}
                x1={line.x1} y1={line.y1} x2={line.x2} y2={line.y2}
              />
            );
          })}

          {tablet && placement && (
            <g ref={tabletRef} transform={`translate(${tablet.left} ${tablet.top})`}>
              <rect
                className="canvas-tablet"
                x={0} y={0} width={size.width} height={size.height} rx={44}
                onDoubleClick={(e) => {
                  e.stopPropagation();
                  tabletSizeRef.current = { width: size.height, height: size.width };
                  setSizeNonce((n) => n + 1);
                }}
              />
              <text className="canvas-tablet-label" x={size.width / 2} y={size.height / 2 - 30}>{tabletLabel}</text>
              <text className="canvas-tablet-meta" x={size.width / 2} y={size.height / 2 + 140}>{tabletMeta}</text>
              <circle
                className="canvas-attachment"
                cx={isVertical(placement.edge) ? (placement.edge === 'left' ? size.width : 0) : size.width / 2}
                cy={isVertical(placement.edge) ? size.height / 2 : (placement.edge === 'top' ? size.height : 0)}
                r={60}
              />
            </g>
          )}
        </g>
      </svg>

      <div className="canvas-legend">
        <span><i className="legend-swatch" style={{ background: 'var(--border-strong)' }} />显示器</span>
        <span><i className="legend-swatch" style={{ background: 'var(--capturing)' }} />可吸附外轮廓</span>
        <span><i className="legend-swatch" style={{ background: 'var(--accent)' }} />平板</span>
      </div>

      {ctx && (
        <div className="ctx-menu" style={{ left: Math.min(ctx.x, 520), top: Math.min(ctx.y, 300) }}>
          <div className="sidebar-label" style={{ padding: '6px 10px' }}>对齐方式</div>
          {([['proportional', '等比'], ['center', '居中'], ['one_to_one', '1:1']] as [Align, string][]).map(([value, label]) => (
            <button key={value} className="menu-item" onClick={() => setAlign(value)}>
              {placement?.align === value ? '● ' : '○ '}{label}
            </button>
          ))}
          <div className="menu-sep" />
          <button className="menu-item" onClick={() => { onPlacementChange(null); setCtx(null); }}>移除</button>
          <button className="menu-item" onClick={() => { setCtx(null); void toggleManual(); }}>测试穿越</button>
        </div>
      )}
    </div>
  );
});
