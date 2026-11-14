/** 设备页：扫描、连接 / 断开、无线添加、记忆设备移除与设备事实展示。 */

import { memo, useCallback } from 'react';
import type { DeviceItem } from '../../api/types';
import { Icon } from '../../components/Icon';
import { Banner, Button, Card, EmptyState, StatusPill } from '../../components/ui';
import { LOCATED_LABELS, formatLastSeen, presentState } from '../../lib/format';
import { connectDevice, disconnectDevice, forgetDevice, refreshDevices } from '../../lib/actions';
import { useAppStore } from '../../store/appStore';

const DeviceRow = memo(function DeviceRow({ device, active, onSelect }: {
  device: DeviceItem;
  active: boolean;
  onSelect: (serial: string) => void;
}) {
  const serial = device.serial;
  const located = device.located ? (LOCATED_LABELS[device.located] ?? device.located) : '';
  const lastSeen = !device.online ? formatLastSeen(device.lastSeen) : '';
  return (
    <button
      type="button"
      className={`device-row ${active ? 'active' : ''}`}
      onClick={() => onSelect(serial)}
      onDoubleClick={() => void connectDevice(serial)}
      title={device.locatedDetail || serial}
    >
      <span className="device-glyph"><Icon name={device.transport === '无线' ? 'wifi' : 'tablet'} size={18} /></span>
      <span className="device-row-main">
        <span className="device-row-head">{device.model || serial}</span>
        <span className="device-row-meta mono">
          {serial} · {device.transport} · {device.online ? device.state : (located || '不在线')}
          {lastSeen ? ` · 最后在线 ${lastSeen}` : ''}
        </span>
      </span>
      <span className={`badge ${device.online ? 'ok' : device.located === 'moved' ? 'warn' : ''}`}>
        {device.online ? '在线' : device.remembered ? '已记住' : device.state}
      </span>
    </button>
  );
});

function DetailItem({ label, value }: { label: string; value: string }) {
  return (
    <div className="detail-item">
      <div className="detail-label">{label}</div>
      <div className="detail-value mono">{value || '—'}</div>
    </div>
  );
}

export function DevicesPage() {
  const devices = useAppStore((s) => s.devices);
  const selected = useAppStore((s) => s.selectedSerial);
  const setSelected = useAppStore((s) => s.setSelectedSerial);
  const error = useAppStore((s) => s.deviceError);
  const hint = useAppStore((s) => s.scanHint);
  const busy = useAppStore((s) => s.busy);
  const facts = useAppStore((s) => s.facts);
  const state = useAppStore((s) => s.state);
  const openDialog = useAppStore((s) => s.openDialog);
  const visibleState = presentState(state, facts, Boolean(busy.connect));
  const onSelect = useCallback((serial: string) => setSelected(serial), [setSelected]);
  const chosen = devices.find((d) => d.serial === selected);

  return (
    <div className="page">
      <div className="page-head">
        <div>
          <h1 className="page-title">设备</h1>
        </div>
        <div className="page-actions">
          <span className="badge accent">{hint || '准备扫描…'}</span>
          <Button icon="refresh" onClick={() => void refreshDevices()} loading={Boolean(busy.scan)}>刷新设备</Button>
          <Button icon="add" onClick={() => openDialog('addDevice')}>添加设备</Button>
        </div>
      </div>

      {error && <div className="error-strip"><Banner tone="err">{error}</Banner></div>}

      <div className="grid-2">
        <Card
          title="设备列表"
          desc="一台设备只占一行；双击可直接连接。「从列表移除」只删除记忆记录，不影响在线设备。"
          footer={
            <>
              <Button
                variant="primary"
                icon="link"
                loading={Boolean(busy.connect)}
                onClick={() => void connectDevice(selected)}
                disabled={!selected}
              >
                连接此设备
              </Button>
              <Button
                icon="trash"
                onClick={() => chosen && void forgetDevice(chosen.serial)}
                // 在线且已被记住的设备拿到的行是 scan 结果（remembered=false），
                // 但它带着注册表 id；只用 remembered 判断会让按钮永久灰掉。
                disabled={!chosen || (!chosen.remembered && !chosen.id)}
              >
                从列表移除
              </Button>
            </>
          }
        >
          {devices.length === 0 ? (
            <EmptyState
              title="未检测到设备"
              hint={'USB 连接：插上数据线并在平板上允许「USB 调试」。\n无线连接：点右上角「添加设备」，输入平板上的 IP 与端口。'}
            />
          ) : (
            <div className="device-list">
              {devices.map((device) => (
                <DeviceRow
                  key={device.id || device.serial}
                  device={device}
                  active={device.serial === selected}
                  onSelect={onSelect}
                />
              ))}
            </div>
          )}
        </Card>

        <Card title="设备详情" desc="连接后显示型号、序列号与真实屏幕参数。">
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)', marginBottom: 'var(--sp-5)' }}>
            <StatusPill state={visibleState} />
            <div style={{ minWidth: 0 }}>
              <div style={{ fontWeight: 600, fontSize: 'var(--fs-16)' }}>
                {facts ? facts.model || facts.serial : '尚未连接平板'}
              </div>
              <div className="muted" style={{ fontSize: 'var(--fs-12)' }}>
                {facts ? facts.serial : '连接后显示型号、序列号与真实屏幕参数'}
              </div>
            </div>
          </div>
          <div className="detail-grid">
            <DetailItem label="型号" value={facts?.model || ''} />
            <DetailItem label="序列号" value={facts?.serial || ''} />
            <DetailItem label="分辨率" value={facts ? `${facts.width} × ${facts.height}` : ''} />
            <DetailItem label="方向" value={facts ? (facts.orientation === 'landscape' ? '横屏' : '竖屏') : ''} />
            <DetailItem label="输入比例" value={facts ? facts.xscale.toFixed(3) : ''} />
            <DetailItem label="指针速度" value={facts ? String(facts.pointerSpeed) : ''} />
          </div>
          <div className="help-block">
            <strong>连接步骤</strong><br />
            1. 在平板上开启「开发者选项 → USB 调试」；<br />
            2. 首次连接时在平板上允许此电脑的调试授权；<br />
            3. 连接后自动读取真实分辨率与输入参数，不依赖型号。
          </div>
          <div style={{ marginTop: 'var(--sp-5)' }}>
            <Button variant="danger" icon="unlink" onClick={() => void disconnectDevice()} disabled={!facts}>
              断开连接
            </Button>
          </div>
        </Card>
      </div>
    </div>
  );
}
