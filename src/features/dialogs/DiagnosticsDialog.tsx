/** 诊断信息：后台采集、脱敏文本、导出 zip。 */

import { Button, Modal } from '../../components/ui';
import { exportDiagnostics, runDiagnostics } from '../../lib/actions';
import { useAppStore } from '../../store/appStore';

export function DiagnosticsDialog() {
  const close = () => useAppStore.getState().openDialog(null);
  const info = useAppStore((s) => s.diagnostics);
  return (
    <Modal
      wide
      title="诊断信息"
      subtitle="采集 adb devices / dumpsys input / pointer_speed / UHID 状态；采集在后台执行，不会卡住界面。导出的 zip 会附带最近 512 KB 日志，并自动隐藏序列号、IP/MAC、用户名与用户目录路径。"
      onClose={close}
      footer={
        <>
          <span className="muted" style={{ flex: 1, fontSize: 'var(--fs-12)' }}>
            {info.progress || info.status}
          </span>
          <Button icon="refresh" loading={info.collecting} onClick={() => void runDiagnostics()}>重新采集</Button>
          <Button variant="primary" icon="download" disabled={!info.text || info.collecting} onClick={() => void exportDiagnostics()}>导出 zip…</Button>
          <Button variant="ghost" onClick={close}>关闭</Button>
        </>
      }
    >
      <div className="diagnostics-text mono">
        {info.text || '点击「重新采集」获取诊断信息'}
      </div>
    </Modal>
  );
}
