/** 关于对话框：版本、许可摘要与常用路径。 */

import { Icon } from '../../components/Icon';
import { Button, Modal } from '../../components/ui';
import { openPath } from '../../lib/actions';
import { useAppStore } from '../../store/appStore';

export function AboutDialog() {
  const close = () => useAppStore.getState().openDialog(null);
  const version = useAppStore((s) => s.version);
  const license = useAppStore((s) => s.paths.license);
  const simulated = useAppStore((s) => s.simulated);
  return (
    <Modal
      title="关于 PadLend"
      subtitle="跨设备共享键鼠无缝操作平板"
      onClose={close}
      footer={
        <>
          <Button icon="folder" onClick={() => void openPath('notices')}>第三方许可…</Button>
          <Button icon="folder" onClick={() => void openPath('logs')}>打开日志目录…</Button>
          <Button icon="refresh" disabled title="发行地址确定后启用">检查更新…</Button>
          <Button variant="primary" onClick={close}>关闭</Button>
        </>
      }
    >
      <div className="about-hero">
        <div className="brand-mark" style={{ width: 52, height: 52, fontSize: 26 }}>P</div>
        <div>
          <div style={{ fontSize: 'var(--fs-20)', fontWeight: 650 }}>PadLend {version}</div>
          <div className="muted">{license} · Tauri 2 + React/TS{simulated ? ' · 演示设备' : ''}</div>
        </div>
      </div>
      <div className="about-list">
        <div>界面：Tauri 2（Rust 外壳）+ React 19 + TypeScript；业务引擎仍由 Python 服务进程承载。</div>
        <div>第三方：scrcpy（Apache-2.0）、Android platform-tools、PyYAML、psutil；许可证原文见 THIRD_PARTY_NOTICES.md。</div>
        <div>许可证：本程序按 Apache-2.0 发布，完整条款见随包 LICENSE。</div>
      </div>
      <div className="help-block" style={{ marginTop: 'var(--sp-5)' }}>
        <Icon name="shield" size={14} /> 诊断导出默认脱敏，但仍请在分享前自行检查。
      </div>
    </Modal>
  );
}
