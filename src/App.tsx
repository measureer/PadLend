/** 应用根：只负责外壳装配、一次性事件订阅与按需加载页面/对话框。 */

import { lazy, Suspense, useEffect } from 'react';
import { useHotkeys } from './hooks';
import { bootstrap, tickClipboardSync } from './lib/actions';
import { BottomBar, Header, Sidebar, TitleBar } from './features/shell/Shell';
import { ToastStack } from './components/ui';
import { connectEvents } from './store/events';
import { useAppStore } from './store/appStore';

// 路由级与对话框级代码分割：首屏只下载设备页与外壳。
const DevicesPage = lazy(() => import('./features/devices/DevicesPage').then((m) => ({ default: m.DevicesPage })));
const LayoutPage = lazy(() => import('./features/layout/LayoutPage').then((m) => ({ default: m.LayoutPage })));
const SettingsPage = lazy(() => import('./features/settings/SettingsPage').then((m) => ({ default: m.SettingsPage })));
const LogsPage = lazy(() => import('./features/logs/LogsPage').then((m) => ({ default: m.LogsPage })));
const AddDeviceDialog = lazy(() => import('./features/devices/AddDeviceDialog').then((m) => ({ default: m.AddDeviceDialog })));
const SetupWizard = lazy(() => import('./features/dialogs/SetupWizard').then((m) => ({ default: m.SetupWizard })));
const DiagnosticsDialog = lazy(() => import('./features/dialogs/DiagnosticsDialog').then((m) => ({ default: m.DiagnosticsDialog })));
const AboutDialog = lazy(() => import('./features/dialogs/AboutDialog').then((m) => ({ default: m.AboutDialog })));

function PageFallback() {
  return <div className="log-empty"><span className="spinner" /> 正在加载…</div>;
}

function CurrentPage() {
  const page = useAppStore((s) => s.page);
  switch (page) {
    case 'layout': return <LayoutPage />;
    case 'settings': return <SettingsPage />;
    case 'logs': return <LogsPage />;
    default: return <DevicesPage />;
  }
}

function DialogHost() {
  const dialog = useAppStore((s) => s.dialog);
  if (!dialog) return null;
  return (
    <Suspense fallback={<PageFallback />}>
      {dialog === 'addDevice' && <AddDeviceDialog />}
      {dialog === 'wizard' && <SetupWizard />}
      {dialog === 'diagnostics' && <DiagnosticsDialog />}
      {dialog === 'about' && <AboutDialog />}
    </Suspense>
  );
}

export default function App() {
  useHotkeys();

  useEffect(() => {
    const detach = connectEvents();
    void bootstrap();
    // 剪贴板同步只在接管期间真正读取；这里保持一个轻量定时器，
    // 而不是在每次状态变化时重新注册监听器。
    const timer = window.setInterval(() => void tickClipboardSync(), 500);
    return () => {
      window.clearInterval(timer);
      detach();
    };
  }, []);

  return (
    <div className="shell">
      <TitleBar />
      <Header />
      <div className="workspace">
        <Sidebar />
        <main className="content">
          <Suspense fallback={<PageFallback />}>
            <CurrentPage />
          </Suspense>
        </main>
      </div>
      <BottomBar />
      <DialogHost />
      <ToastStack />
    </div>
  );
}
