/** 运行日志 + 实时指标。过滤用防抖 + useMemo，列表虚拟滚动，最多只渲染可视区 ~30 行。 */

import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Icon } from '../../components/Icon';
import { Button, Card, TextInput } from '../../components/ui';
import { useDebouncedValue, useElementSize, useRafCallback } from '../../hooks';
import { logEventTone, metricLine } from '../../lib/format';
import { useAppStore } from '../../store/appStore';

const ROW_HEIGHT = 22;

const LogRow = memo(function LogRow({ timestamp, event, message }: { timestamp: string; event: string; message: string }) {
  return (
    <div className="log-row" style={{ height: ROW_HEIGHT }}>
      <span className="log-ts">{timestamp}</span>
      <span className={`log-event tone-${logEventTone(event)}`}>{event}</span>
      <span className="log-msg">{message}</span>
    </div>
  );
});

function MetricsPanel() {
  const metrics = useAppStore((s) => s.metrics);
  return (
    <Card title="指标" desc="来自 Router 的 2 秒健康快照">
      <div className="metric-row"><span className="metric-label">上报</span><span className="metric-value">{metricLine(metrics, 'rate')}</span></div>
      <div className="metric-row"><span className="metric-label">时延</span><span className="metric-value">{metricLine(metrics, 'rtt')}</span></div>
      <div className="metric-row"><span className="metric-label">资源</span><span className="metric-value">{metricLine(metrics, 'cpu')}</span></div>
      <p className="field-hint" style={{ marginTop: 'var(--sp-4)' }}>
        键名与 core.metrics.snapshot() 一致（raw_hz / send_hz / backlog / drop_pct / cpu_pct / mem_mb），不再做别名猜测。
      </p>
    </Card>
  );
}

export function LogsPage() {
  const logs = useAppStore((s) => s.logs);
  const filter = useAppStore((s) => s.logFilter);
  const setFilter = useAppStore((s) => s.setLogFilter);
  const debounced = useDebouncedValue(filter, 140);
  const listRef = useRef<HTMLDivElement | null>(null);
  const { height } = useElementSize(listRef);
  const [scrollTop, setScrollTop] = useState(0);
  const nearBottomRef = useRef(true);

  const filtered = useMemo(() => {
    const needle = debounced.trim().toLowerCase();
    if (!needle) return logs;
    return logs.filter((entry) => `${entry.timestamp} ${entry.event} ${entry.message}`.toLowerCase().includes(needle));
  }, [logs, debounced]);

  const onScroll = useRafCallback((value: number) => {
    setScrollTop(value);
    const element = listRef.current;
    if (element) nearBottomRef.current = element.scrollHeight - element.scrollTop - element.clientHeight < 40;
  });

  useEffect(() => {
    const element = listRef.current;
    if (element && nearBottomRef.current) element.scrollTop = element.scrollHeight;
  }, [filtered.length]);

  const visibleCount = Math.max(12, Math.ceil(height / ROW_HEIGHT) + 8);
  const start = Math.max(0, Math.min(Math.floor(scrollTop / ROW_HEIGHT) - 4, Math.max(0, filtered.length - visibleCount)));
  const rows = filtered.slice(start, start + visibleCount);

  const clearFilter = useCallback(() => setFilter(''), [setFilter]);

  return (
    <div className="page">
      <div className="page-head">
        <div>
          <h1 className="page-title">运行日志</h1>
          <p className="page-subtitle">固定事件名与实时指标；采集诊断时会包含最近日志。过滤只影响显示，不会丢弃已收到的事件。</p>
        </div>
        <div className="page-actions">
          <span className="badge accent">{logs.length} 条事件</span>
          <Button icon="refresh" onClick={() => void import('../../lib/actions').then((m) => m.loadRecentLogs())}>重新载入</Button>
        </div>
      </div>

      <div className="logs-layout">
        <Card
          title="事件日志"
          desc="HH:MM:SS.mmm EVENT 说明 · 最多保留 2000 行"
          actions={
            <div className="log-toolbar">
              <TextInput label="过滤日志" value={filter} onChange={setFilter} placeholder="过滤事件名或说明…" />
              <span title="按子串过滤"><Icon name="search" size={16} /></span>
              {filter && <Button compact variant="ghost" icon="close" onClick={clearFilter}>清除</Button>}
            </div>
          }
        >
          <div
            ref={listRef}
            className="log-list"
            onScroll={(e) => onScroll(e.currentTarget.scrollTop)}
          >
            {filtered.length === 0 ? (
              <div className="log-empty">{debounced ? '没有匹配的事件' : '等待事件…'}</div>
            ) : (
              <>
                <div style={{ height: start * ROW_HEIGHT }} />
                {rows.map((entry, index) => (
                  <LogRow key={`${start + index}-${entry.timestamp}-${entry.event}`} {...entry} />
                ))}
                <div style={{ height: Math.max(0, (filtered.length - start - rows.length) * ROW_HEIGHT) }} />
              </>
            )}
          </div>
        </Card>
        <MetricsPanel />
      </div>
    </div>
  );
}
