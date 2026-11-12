import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../App';
import { useAppStore } from '../store/appStore';

class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
vi.stubGlobal('ResizeObserver', ResizeObserverStub);
vi.stubGlobal('DOMPoint', class {
  constructor(public x: number, public y: number) {}
  matrixTransform() { return { x: this.x, y: this.y }; }
});

describe('应用外壳与核心交互（浏览器 mock 后端）', () => {
  beforeEach(() => {
    document.documentElement.dataset.theme = 'dark';
    useAppStore.setState({
      hydrated: false, page: 'devices', theme: 'dark', dialog: null,
      logs: [], devices: [], selectedSerial: '', toasts: [], configBanner: null,
    });
  });

  it('首屏渲染设备页，导航可切换且主题可切换', async () => {
    render(<App />);
    await waitFor(() => expect(screen.getByRole('heading', { level: 1, name: '设备' })).toBeTruthy());
    expect(screen.getByText('连接此设备')).toBeTruthy();
    expect(screen.getByText('设备详情')).toBeTruthy();

    fireEvent.click(screen.getByRole('button', { name: '设置' }));
    await waitFor(() => expect(screen.getAllByRole('button', { name: '保存设置' }).length).toBeGreaterThan(0));
    expect(screen.getByText('手感')).toBeTruthy();
    expect(screen.getByText('关闭窗口时最小化到托盘')).toBeTruthy();

    fireEvent.click(screen.getByRole('button', { name: '切换到白天模式' }));
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe('light'));

    fireEvent.click(screen.getByRole('navigation').querySelectorAll('button')[3]);
    await waitFor(() => expect(screen.getByText('事件日志')).toBeTruthy());
  });

  it('菜单与对话框按需加载', async () => {
    render(<App />);
    await waitFor(() => expect(screen.getByRole('heading', { level: 1, name: '设备' })).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: '菜单' }));
    await waitFor(() => expect(screen.getByText('运行诊断…')).toBeTruthy());
    fireEvent.click(screen.getByText('关于 PadLend…'));
    await waitFor(() => expect(screen.getByText(/第三方许可/)).toBeTruthy());
  });
});
