import { describe, expect, it } from 'vitest';
import { connectedHint, formatLastSeen, logEventTone, metricLine, parseLogLine, presentState, stateLabel } from '../lib/format';

describe('状态与格式化', () => {
  it('没有设备事实时不把 fault 显示成故障', () => {
    expect(presentState('fault', null, false)).toBe('disconnected');
    expect(presentState('fault', null, true)).toBe('connecting');
    expect(presentState('captured', null, false)).toBe('captured');
    expect(stateLabel('arming')).toBe('外推中');
  });

  it('指标键名与 core.metrics.snapshot() 对齐', () => {
    const text = metricLine({ raw_hz: 126, send_hz: 125, backlog: 2, rtt_ms: 4.2, p95_ms: 8, drop_pct: 0.5, cpu_pct: 2, mem_mb: 90 }, 'rtt');
    expect(text).toContain('RTT 4.2ms');
    expect(text).toContain('丢包 0.50%');
    expect(metricLine({}, 'rate')).toContain('—');
  });

  it('日志行解析支持无时间戳的原文', () => {
    expect(parseLogLine('12:00:00.123 STATE idle → captured')).toEqual({
      timestamp: '12:00:00.123', event: 'STATE', message: 'idle → captured',
    });
    expect(parseLogLine('裸文本')).toEqual({ timestamp: '', event: '', message: '裸文本' });
  });

  it('最后在线只取 UTC 日期，坏值不显示', () => {
    expect(formatLastSeen('2026-11-12T23:30:00+00:00')).toBe('2026-11-12');
    expect(formatLastSeen('')).toBe('');
    expect(formatLastSeen('not-a-date')).toBe('');
  });

  it('日志事件着色与底栏提示', () => {
    expect(logEventTone('cap.start')).toBe('capturing');
    expect(logEventTone('cfg.reload')).toBe('warn');
    expect(connectedHint({ width: 3200, height: 2136, orientation: 'landscape' }, 'right')).toContain('右侧边缘');
    expect(connectedHint(null, undefined)).toContain('在设备页连接平板');
  });
});
