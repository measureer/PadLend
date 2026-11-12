/** 状态文案、颜色与展示格式化（原 ui/theme/tokens.py 的单一来源在前端的镜像）。 */

import type { AppState, Edge, LogEntry, Metrics } from '../api/types';

export const STATE_LABELS: Record<string, string> = {
  '': '未连接',
  disconnected: '未连接',
  connecting: '连接中',
  idle: '已连接',
  arming: '外推中',
  returning: '回程中',
  captured: '接管中',
  fault: '故障',
};

export const STATE_TONES: Record<string, string> = {
  '': 'disconnected',
  disconnected: 'disconnected',
  connecting: 'accent',
  idle: 'ok',
  arming: 'warn',
  returning: 'warn',
  captured: 'capturing',
  fault: 'err',
};

/** 未拿到设备事实前不把 FAULT 显示成故障（与旧版 _present_state 一致）。 */
export function presentState(state: string, facts: unknown, connecting: boolean): AppState {
  if (!facts && (state === 'idle' || state === 'fault')) {
    return connecting ? 'connecting' : 'disconnected';
  }
  return (STATE_LABELS[state] ? state : 'disconnected') as AppState;
}

export function stateLabel(state: AppState): string {
  return STATE_LABELS[state] ?? '未连接';
}

export const EDGE_LABELS: Record<Edge, string> = {
  left: '左侧',
  right: '右侧',
  top: '上侧',
  bottom: '下侧',
};

export const LOCATED_LABELS: Record<string, string> = {
  online: '在线',
  moved: '已换地址',
  no_port: '端口未知',
  offline: '不在线',
};

export function logEventTone(event: string): string {
  const key = event.split('.', 1)[0].toLowerCase();
  if (key === 'arm' || key === 'cfg') return 'warn';
  if (key === 'cap') return 'capturing';
  if (key === 'hit') return 'ok';
  if (key === 'err' || key === 'fault') return 'err';
  if (key === 'back' || key === 'tablet' || key === 'kbd' || key === 'device') return 'accent';
  return 'muted';
}

export function metricLine(metrics: Metrics, key: 'rate' | 'rtt' | 'cpu'): string {
  const value = (n: number | undefined, digits = 1): string =>
    n === undefined ? '—' : n.toFixed(digits);
  if (key === 'rate') {
    return `上报 ${value(metrics.raw_hz, 0)}Hz  发送 ${value(metrics.send_hz, 0)}Hz  背压 ${metrics.backlog ?? '—'}`;
  }
  if (key === 'rtt') {
    return `RTT ${value(metrics.rtt_ms)}ms  p95 ${value(metrics.p95_ms)}ms  丢包 ${value(metrics.drop_pct, 2)}%`;
  }
  return `CPU ${value(metrics.cpu_pct)}%  内存 ${value(metrics.mem_mb)}MB`;
}

export function connectedHint(facts: { width: number; height: number; orientation: string } | null, edge: Edge | undefined): string {
  if (!facts) return '在设备页连接平板 · 命令行仅用于开发验证';
  const base = `${facts.orientation === 'landscape' ? '横屏' : '竖屏'} ${facts.width}×${facts.height}`;
  return edge
    ? `${base} · 从屏幕${EDGE_LABELS[edge]}边缘滑入平板`
    : `${base} · 在「布局」页把平板拖到屏幕边缘`;
}

export function parseLogLine(line: string): LogEntry {
  const match = /^(?<ts>\d{2}:\d{2}:\d{2}\.\d{3})\s+(?<event>[A-Za-z][\w.]*)\s+(?<message>.*)$/.exec(line.trim());
  if (!match || !match.groups) return { timestamp: '', event: '', message: line.trim() };
  return { timestamp: match.groups.ts, event: match.groups.event, message: match.groups.message };
}

export function formatGain(value: number): string {
  return `${value.toFixed(2)}×`;
}
