/** 添加无线设备：地址补全、端口校验、6 位配对码校验，与旧版文案一致。 */

import { useState } from 'react';
import { Button, Modal, TextInput } from '../../components/ui';
import { addDevice } from '../../lib/actions';
import { useAppStore } from '../../store/appStore';

function normalizeAddress(raw: string): string {
  const value = raw.trim();
  if (!value) return '';
  return value.includes(':') ? value : `${value}:5555`;
}

function validate(address: string, code: string): string {
  const normalized = normalizeAddress(address);
  if (!normalized) return '请填写平板上的 IP 地址与端口';
  const match = /^(\d{1,3}(?:\.\d{1,3}){3}|[a-zA-Z0-9.-]+):(\d{1,5})$/.exec(normalized);
  if (!match) return '地址格式应为 IP:端口，例如 192.168.1.23:5555';
  const port = Number(match[2]);
  if (port < 1 || port > 65535) return '端口必须在 1–65535 之间';
  if (code && !/^\d{6}$/.test(code.trim())) return '配对码是平板上的 6 位数字';
  return '';
}

export function AddDeviceDialog() {
  const close = () => useAppStore.getState().openDialog(null);
  const busy = useAppStore((s) => s.busy.add);
  const [address, setAddress] = useState('');
  const [code, setCode] = useState('');
  const [error, setError] = useState('');

  const submit = () => {
    const problem = validate(address, code);
    if (problem) {
      setError(problem);
      return;
    }
    setError('');
    void addDevice(normalizeAddress(address), code.trim());
  };

  return (
    <Modal
      title="添加无线设备"
      subtitle="用无线调试把平板接入；首次配对需要平板上的 6 位配对码。"
      onClose={close}
      footer={
        <>
          <Button variant="ghost" onClick={close}>取消</Button>
          <Button variant="primary" icon="link" loading={Boolean(busy)} onClick={submit}>
            {code.trim() ? '配对并连接' : '连接'}
          </Button>
        </>
      }
    >
      <div style={{ display: 'grid', gap: 'var(--sp-5)' }}>
        <div className="help-block" style={{ marginTop: 0 }}>
          在平板的「开发者选项 → 无线调试」里查看 IP 地址和端口；首次使用点「使用配对码配对设备」，
          把 6 位配对码填到下面。平板与电脑需要在同一 Wi-Fi，路由器不能开启客户端隔离。
        </div>
        <div className="field">
          <label className="field-label" htmlFor="addr">设备地址</label>
          <TextInput
            label="设备地址"
            value={address}
            onChange={(value) => { setAddress(value); setError(''); }}
            placeholder="平板 IP:端口，例如 192.168.1.23:5555"
            invalid={Boolean(error) && !code}
            onKeyDown={(e) => { if (e.key === 'Enter') submit(); }}
          />
        </div>
        <div className="field">
          <label className="field-label" htmlFor="code">配对码</label>
          <TextInput
            label="配对码"
            value={code}
            onChange={(value) => { setCode(value.replace(/\D/g, '').slice(0, 6)); setError(''); }}
            placeholder="配对码（首次配对时填 6 位数字，可留空）"
            maxLength={6}
            onKeyDown={(e) => { if (e.key === 'Enter') submit(); }}
          />
        </div>
        {error && <div className="banner err">{error}</div>}
      </div>
    </Modal>
  );
}
