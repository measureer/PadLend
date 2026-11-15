/** 关于页：版本、检查更新、仓库地址与构建信息。
 *
 * 1.7.7 之前这些内容塞在「关于」对话框里；现在改成常驻页面，托盘 / 菜单的「关于」
 * 直接切到这里，检查更新也能就地看到结果，不用再叠一层对话框。
 */

import { Icon } from '../../components/Icon';
import { Button, Card } from '../../components/ui';
import { backend } from '../../api/client';
import { checkUpdates, openPath, openUrl } from '../../lib/actions';
import { humanSize, RELEASES_PAGE } from '../../lib/updates';
import { useAppStore } from '../../store/appStore';

const REPO_URL = 'https://github.com/measureer/PadLend';

/** 复制仓库地址：失败只提示，不阻塞页面。 */
async function copyRepo(): Promise<void> {
  const s = useAppStore.getState();
  try {
    await backend.clipboardWrite(REPO_URL);
    s.pushToast('仓库地址已复制');
  } catch (error) {
    s.pushToast(`无法复制：${(error as Error).message}`, 'err');
  }
}

function formatDate(text: string): string {
  if (!text) return '';
  const date = new Date(text);
  return Number.isNaN(date.getTime()) ? text : date.toLocaleDateString('zh-CN');
}

export function AboutPage() {
  const version = useAppStore((s) => s.version);
  const license = useAppStore((s) => s.paths.license);
  const configPath = useAppStore((s) => s.configPath);
  const simulated = useAppStore((s) => s.simulated);
  const update = useAppStore((s) => s.update);
  const { checking, result, error, checkedAt } = update;
  const installer = result?.installer ?? null;
  const downloadUrl = installer?.downloadUrl || result?.release.pageUrl || '';

  return (
    <div className="page">
      <div className="page-head">
        <div>
          <h1 className="page-title">关于</h1>
          <p className="page-subtitle">版本、更新与仓库地址；检查更新只读取 GitHub 的最新发布，不会自动下载或安装。</p>
        </div>
        <div className="page-actions">
          <Button icon="link" onClick={() => void openUrl(RELEASES_PAGE)}>Release 页面…</Button>
          <Button variant="primary" icon="refresh" loading={checking} onClick={() => void checkUpdates()}>
            检查更新
          </Button>
        </div>
      </div>

      <div className="about-grid">
        <Card className="about-card" title="PadLend" desc="跨设备共享键鼠无缝操作平板">
          <div className="about-hero">
            <div className="brand-mark" style={{ width: 56, height: 56, fontSize: 28 }}>P</div>
            <div>
              <div className="about-version">PadLend {version || '—'}</div>
              <div className="muted">
                {license || 'Apache-2.0'} · Tauri 2 + React/TS{simulated ? ' · 演示设备' : ''}
              </div>
            </div>
          </div>
          <div className="about-list">
            <div>界面：Tauri 2（Rust 外壳）+ React 19 + TypeScript；业务引擎是同包里独立的 Rust 进程 padlend-core。</div>
            <div>本程序按 Apache-2.0 发布，完整条款见随包 LICENSE。</div>
          </div>
        </Card>

        <Card
          className="about-card"
          title="版本与更新"
          desc="从 GitHub Releases 读取最新版本并与本机比较"
        >
          <div className="about-list">
            <div>当前版本：<strong>PadLend {version || '—'}</strong></div>
            {checking && (
              <div className="muted"><span className="spinner" /> 正在读取 GitHub Releases…</div>
            )}
            {!checking && error && <div style={{ color: 'var(--err)' }}>{error}</div>}
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
            {!checking && !result && !error && (
              <div className="muted">尚未检查；点右上角「检查更新」或下面的按钮。</div>
            )}
          </div>
          <div className="about-actions">
            {result?.newer && (
              <Button variant="primary" icon="download" disabled={!downloadUrl} onClick={() => void openUrl(downloadUrl)}>
                下载新版本…
              </Button>
            )}
            {result && (
              <Button icon="info" onClick={() => void openUrl(result.release.pageUrl)}>查看发布说明…</Button>
            )}
          </div>
          <div className="help-block" style={{ marginTop: 'var(--sp-4)' }}>
            <Icon name="shield" size={14} /> 下载后运行安装包覆盖安装即可，config.yaml 与已配对设备都会保留；
            当前构建没有代码签名，SmartScreen 会提示「未知发布者」。
          </div>
        </Card>

        <Card className="about-card" title="仓库地址" desc="源码、Issue 与版本历史都在 GitHub">
          <div className="about-repo mono">{REPO_URL}</div>
          <div className="about-actions">
            <Button icon="link" onClick={() => void openUrl(REPO_URL)}>在浏览器打开</Button>
            <Button icon="copy" onClick={() => void copyRepo()}>复制地址</Button>
          </div>
          <div className="about-list">
            <div>发布节奏：版本攒到一定程度才会发一个 GitHub Release；页面上的「最新版本」可能比你本地旧的已发布说明，属正常。</div>
          </div>
        </Card>

        <Card className="about-card" title="构建与支持" desc="第三方许可、配置与日志位置">
          <div className="about-list">
            <div>第三方：scrcpy-server 5.0.1、Android platform-tools（adb）、WebView2 与一批 MIT/Apache-2.0 的 Rust / npm 依赖；完整清单与许可原文见 THIRD_PARTY_NOTICES.md。</div>
            <div>配置文件：<span className="mono">{configPath || '%APPDATA%\\PadLend\\config.yaml'}</span></div>
          </div>
          <div className="about-actions">
            <Button icon="folder" onClick={() => void openPath('notices')}>第三方许可…</Button>
            <Button icon="folder" onClick={() => void openPath('config')}>打开配置…</Button>
            <Button icon="folder" onClick={() => void openPath('logs')}>打开日志目录…</Button>
          </div>
        </Card>
      </div>
    </div>
  );
}
