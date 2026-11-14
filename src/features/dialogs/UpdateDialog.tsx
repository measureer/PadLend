/** 检查更新：从 GitHub Releases 读最新发布，与本机版本比较后给出下载入口。 */

import { useEffect, useRef } from 'react';
import { Icon } from '../../components/Icon';
import { Button, Modal } from '../../components/ui';
import { checkUpdates, openUrl } from '../../lib/actions';
import { humanSize } from '../../lib/updates';
import { useAppStore } from '../../store/appStore';

function formatDate(text: string): string {
  if (!text) return '';
  const date = new Date(text);
  return Number.isNaN(date.getTime()) ? text : date.toLocaleDateString('zh-CN');
}

export function UpdateDialog() {
  const close = () => useAppStore.getState().openDialog(null);
  const version = useAppStore((s) => s.version);
  const update = useAppStore((s) => s.update);
  const { checking, result, error, checkedAt } = update;
  // 打开对话框就查一次；已有结果时（重复打开）直接复用，不重复联网。
  const asked = useRef(false);
  useEffect(() => {
    if (asked.current || result || checking) return;
    asked.current = true;
    void checkUpdates();
  }, [result, checking]);

  const installer = result?.installer ?? null;
  const downloadUrl = installer?.downloadUrl || result?.release.pageUrl || '';

  return (
    <Modal
      title="检查更新"
      subtitle="从 GitHub 读取最新发布的安装包，与本机版本比较；不会自动下载或安装。"
      onClose={close}
      footer={
        <>
          <Button icon="refresh" loading={checking} onClick={() => void checkUpdates()}>重新检查</Button>
          <Button icon="link" disabled={!result} onClick={() => void openUrl(result?.release.pageUrl ?? '')}>Release 页面…</Button>
          {result?.newer && (
            <Button variant="primary" icon="download" disabled={!downloadUrl} onClick={() => void openUrl(downloadUrl)}>
              下载新版本…
            </Button>
          )}
          <Button variant="ghost" onClick={close}>关闭</Button>
        </>
      }
    >
      <div className="about-list">
        <div>当前版本：<strong>PadLend {version}</strong></div>
        {checking && (
          <div className="muted"><span className="spinner" /> 正在读取 GitHub Releases…</div>
        )}
        {!checking && error && (
          <div style={{ color: 'var(--danger)' }}>{error}</div>
        )}
        {!checking && result && (
          result.newer ? (
            <>
              <div>发现新版本：<strong>v{result.latest}</strong>{result.release.name ? `（${result.release.name}）` : ''}</div>
              <div className="muted">
                发布于 {formatDate(result.release.publishedAt) || '时间未知'}
                {installer ? ` · ${installer.name}${installer.size ? ` · ${humanSize(installer.size)}` : ''}` : ' · 该发布没有可直接下载的安装包'}
              </div>
            </>
          ) : (
            <div>已是最新版本{checkedAt ? `（${checkedAt} 检查）` : ''}。</div>
          )
        )}
      </div>

      {!checking && result?.newer && result.release.notes && (
        <div className="diagnostics-text mono" style={{ maxHeight: 220, marginTop: 'var(--sp-4)' }}>
          {result.release.notes.trim()}
        </div>
      )}

      <div className="help-block" style={{ marginTop: 'var(--sp-5)' }}>
        <Icon name="shield" size={14} /> 下载后运行安装包覆盖安装即可，config.yaml 与已配对设备都会保留；
        当前构建没有代码签名，SmartScreen 会提示「未知发布者」。
      </div>
    </Modal>
  );
}
