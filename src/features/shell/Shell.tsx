/** 应用外壳：自定义标题栏、顶栏、侧栏导航、底栏、溢出菜单。
 * 全部按 CSS 变量取色，主题切换不触发组件树重算。 */

import { memo } from 'react';
import { backend, isTauri } from '../../api/client';
import type { PageId } from '../../api/types';
import { Icon, type IconName } from '../../components/Icon';
import { IconButton, StatusPill } from '../../components/ui';
import * as actions from '../../lib/actions';
import { connectedHint, presentState, stateLabel } from '../../lib/format';
import { useAppStore } from '../../store/appStore';

const NAV: { id: PageId; label: string; icon: IconName }[] = [
  { id: 'devices', label: '设备', icon: 'devices' },
  { id: 'layout', label: '布局', icon: 'layout' },
  { id: 'settings', label: '设置', icon: 'settings' },
  { id: 'logs', label: '日志', icon: 'logs' },
];

export const TitleBar = memo(function TitleBar() {
  return (
    <div className="titlebar">
      <div className="titlebar-drag" data-tauri-drag-region>
        <span className="titlebar-dot" />
        <span>PadLend</span>
        <span style={{ opacity: 0.6 }}>跨设备共享键鼠无缝操作平板</span>
      </div>
      <div className="titlebar-actions">
        <button className="win-btn" aria-label="最小化" onClick={() => void backend.windowMinimize()} disabled={!isTauri}>
          <Icon name="minimize" size={14} />
        </button>
        <button className="win-btn" aria-label="最大化 / 还原" onClick={() => void backend.windowToggleMaximize()} disabled={!isTauri}>
          <Icon name="maximize" size={12} />
        </button>
        <button className="win-btn close" aria-label="关闭" onClick={() => void backend.windowClose()} disabled={!isTauri}>
          <Icon name="close" size={14} />
        </button>
      </div>
    </div>
  );
});

const MENU_ITEMS: { label: string; icon: IconName; action: () => void; danger?: boolean }[] = [
  { label: '手动接管 / 交还', icon: 'link', action: () => void actions.toggleManual() },
  { label: '重连', icon: 'refresh', action: () => void actions.reconnect() },
  { label: '打开配置…', icon: 'folder', action: () => void actions.openPath('config') },
  { label: '复制诊断信息', icon: 'copy', action: () => void actions.copyStaticDiagnostics() },
  { label: '打开日志目录…', icon: 'folder', action: () => void actions.openPath('logs') },
  { label: '关于 PadLend…', icon: 'info', action: () => useAppStore.getState().openDialog('about') },
  { label: '运行诊断…', icon: 'shield', action: () => { useAppStore.getState().openDialog('diagnostics'); void actions.runDiagnostics(); } },
  { label: '首次设置向导…', icon: 'sparkle', action: () => useAppStore.getState().openDialog('wizard') },
  { label: '退出 PadLend', icon: 'power', action: () => void actions.quitApp(), danger: true },
];

export const Header = memo(function Header() {
  const state = useAppStore((s) => s.state);
  const facts = useAppStore((s) => s.facts);
  const metrics = useAppStore((s) => s.metrics);
  const busy = useAppStore((s) => s.busy.connect);
  const theme = useAppStore((s) => s.theme);
  const menuOpen = useAppStore((s) => s.menuOpen);
  const setMenuOpen = useAppStore((s) => s.setMenuOpen);
  const setPage = useAppStore((s) => s.setPage);
  const visibleState = presentState(state, facts, Boolean(busy));
  const rtt = metrics.rtt_ms === undefined ? 'RTT —' : `RTT ${metrics.rtt_ms.toFixed(1)}ms`;

  return (
    <header className="header">
      <div className="brand">
        <div className="brand-mark">P</div>
        <div>
          <div className="brand-name">PadLend</div>
          <div className="brand-sub">Tablet bridge</div>
        </div>
      </div>
      <div className="header-divider" />
      <StatusPill state={visibleState} />
      <div className="header-info">
        <div className="header-info-item" style={{ maxWidth: 260 }}>
          <span className="header-info-label">当前设备</span>
          <span className="header-info-value" title={facts?.serial || ''}>
            {facts ? facts.model || facts.serial : '未连接设备'}
          </span>
        </div>
        <div className="header-info-item">
          <span className="header-info-label">显示</span>
          <span className="header-info-value mono">
            {facts ? `${facts.orientation === 'landscape' ? '横屏' : '竖屏'} · ${facts.width}×${facts.height}` : '—'}
          </span>
        </div>
        <div className="header-info-item">
          <span className="header-info-label">延迟</span>
          <span className="header-info-value mono">{rtt}</span>
        </div>
      </div>
      <div className="header-actions">
        <IconButton
          icon="dots"
          label="菜单"
          active={menuOpen}
          onClick={() => setMenuOpen(!menuOpen)}
        />
        <IconButton icon="gear" label="设置 (Ctrl+,)" onClick={() => setPage('settings')} />
        <IconButton
          icon={theme === 'dark' ? 'sun' : 'moon'}
          label={theme === 'dark' ? '切换到白天模式' : '切换到黑夜模式'}
          onClick={() => void actions.toggleTheme()}
        />
        {menuOpen && (
          <div className="menu-popover" role="menu">
            {MENU_ITEMS.map((item, index) => (
              <div key={item.label}>
                {(index === 2 || index === 6 || index === 8) && <div className="menu-sep" />}
                <button
                  type="button"
                  className={`menu-item ${item.danger ? 'danger' : ''}`}
                  role="menuitem"
                  onClick={() => { setMenuOpen(false); item.action(); }}
                >
                  <Icon name={item.icon} size={15} />
                  {item.label}
                </button>
              </div>
            ))}
          </div>
        )}
      </div>
    </header>
  );
});

export const Sidebar = memo(function Sidebar() {
  const page = useAppStore((s) => s.page);
  const setPage = useAppStore((s) => s.setPage);
  const version = useAppStore((s) => s.version);
  return (
    <nav className="sidebar" aria-label="主导航">
      <div className="sidebar-label">工作区</div>
      {NAV.map((item) => (
        <button
          key={item.id}
          type="button"
          className={`nav-item ${page === item.id ? 'active' : ''}`}
          aria-current={page === item.id ? 'page' : undefined}
          onClick={() => {
            setPage(item.id);
            if (item.id === 'devices') void actions.refreshDevices();
          }}
        >
          <span className="nav-icon"><Icon name={item.icon} size={19} /></span>
          <span className="nav-label">{item.label}</span>
        </button>
      ))}
      <div className="sidebar-foot">
        Ctrl+Alt+S 接管 / 交还<br />
        Ctrl+Alt+Q 急停<br />
        Ctrl+, 打开设置
      </div>
      <div className="sidebar-foot mono" style={{ borderTop: 0, paddingTop: 0 }}>v{version}</div>
    </nav>
  );
});

export const BottomBar = memo(function BottomBar() {
  const state = useAppStore((s) => s.state);
  const facts = useAppStore((s) => s.facts);
  const placement = useAppStore((s) => s.placement);
  const busy = useAppStore((s) => s.busy.connect);
  const version = useAppStore((s) => s.version);
  const visibleState = presentState(state, facts, Boolean(busy));
  return (
    <footer className="bottombar">
      <span className={`tone-${visibleState === 'captured' ? 'capturing' : visibleState === 'idle' ? 'ok' : 'muted'}`}>
        ● {stateLabel(visibleState)}
      </span>
      <span>{facts?.model || '未连接设备'}</span>
      <span className="grow" />
      <span>{connectedHint(facts, placement?.edge)}</span>
      <span className="mono subtle">v{version}</span>
    </footer>
  );
});
