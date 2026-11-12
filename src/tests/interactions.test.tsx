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

beforeEach(() => {
  document.documentElement.dataset.theme = 'dark';
  window.history.replaceState({}, '', '/');
  useAppStore.setState({
    hydrated: false, page: 'devices', theme: 'dark', dialog: null,
    logs: [], devices: [], selectedSerial: '', toasts: [], configBanner: null, layoutDirty: false,
  });
});

async function openDevices() {
  render(<App />);
  await waitFor(() => expect(screen.getByRole('heading', { level: 1, name: '设备' })).toBeTruthy());
}

function nav(label: string) {
  fireEvent.click(screen.getByRole('navigation').querySelectorAll('button')[['设备', '布局', '设置', '日志'].indexOf(label)]);
}

describe('核心功能与交互（mock 后端）', () => {
  it('连接 / 断开设备会更新事实并给出反馈', async () => {
    await openDevices();
    await waitFor(() => expect(useAppStore.getState().selectedSerial).not.toBe(''));
    fireEvent.click(screen.getByText('连接此设备'));
    await waitFor(() => expect(screen.getAllByText(/已连接/).length).toBeGreaterThan(0), { timeout: 4000 });
    await waitFor(() => expect(useAppStore.getState().facts?.width).toBe(3200));
    fireEvent.click(screen.getByText('断开连接'));
    await waitFor(() => expect(useAppStore.getState().facts).toBeNull());
  });

  it('添加无线设备的地址与配对码校验', async () => {
    await openDevices();
    fireEvent.click(screen.getByText('添加设备'));
    await waitFor(() => expect(screen.getByRole('dialog')).toBeTruthy());
    fireEvent.click(screen.getByText('连接', { selector: 'button' }));
    expect(screen.getByText('请填写平板上的 IP 地址与端口')).toBeTruthy();

    fireEvent.change(screen.getByLabelText('设备地址'), { target: { value: '192.168.1.23:99999' } });
    fireEvent.change(screen.getByLabelText('配对码'), { target: { value: '12345' } });
    fireEvent.click(screen.getByText('配对并连接', { selector: 'button' }));
    expect(screen.getByText(/配对码是平板上的 6 位数字|端口必须在 1–65535/)).toBeTruthy();
  });

  it('设置页改动只在保存时提交，并给出成功反馈', async () => {
    await openDevices();
    nav('设置');
    await waitFor(() => expect(screen.getByText('手感')).toBeTruthy());
    const toggle = screen.getByRole('switch', { name: '关闭窗口时最小化到托盘' });
    fireEvent.click(toggle);
    expect(toggle.getAttribute('aria-checked')).toBe('true');
    expect(useAppStore.getState().config.ui.close_to_tray).toBe(false); // 草稿未提交
    fireEvent.click(screen.getAllByRole('button', { name: '保存设置' })[0]);
    await waitFor(() => expect(useAppStore.getState().config.ui.close_to_tray).toBe(true));
    expect(screen.getAllByText(/已保存/).length).toBeGreaterThan(0);
  });

  it('日志过滤只影响显示，不丢弃事件', async () => {
    await openDevices();
    nav('日志');
    await waitFor(() => expect(screen.getByText('事件日志')).toBeTruthy());
    const total = useAppStore.getState().logs.length;
    fireEvent.change(screen.getByLabelText('过滤日志'), { target: { value: 'zzz-not-found' } });
    await waitFor(() => expect(screen.getByText('没有匹配的事件')).toBeTruthy());
    expect(useAppStore.getState().logs.length).toBeGreaterThanOrEqual(total);
  });

  it('布局页可保存预览摆位', async () => {
    await openDevices();
    nav('布局');
    await waitFor(() => expect(screen.getByText('桌面布局')).toBeTruthy());
    await waitFor(() => expect(screen.getByText('保存布局', { selector: 'button' })).toBeTruthy());
    fireEvent.click(screen.getByText('保存布局', { selector: 'button' }));
    await waitFor(() => expect(screen.getAllByText(/布局已保存/).length).toBeGreaterThan(0));
    expect(useAppStore.getState().layoutDirty).toBe(false);
  });
});
