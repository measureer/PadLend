import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import type { UpdateResult } from '../lib/updates';
import { useAppStore } from '../store/appStore';

vi.mock('../lib/actions', async () => {
  const actual = await vi.importActual<typeof import('../lib/actions')>('../lib/actions');
  return {
    ...actual,
    checkUpdates: vi.fn(async () => undefined),
    openUrl: vi.fn(async () => undefined),
  };
});

import { AboutPage } from '../features/about/AboutPage';

const release = {
  tag: 'v1.7.7',
  name: 'PadLend 1.7.7',
  notes: '- 关于页面',
  publishedAt: '2026-11-15T02:00:00Z',
  pageUrl: 'https://github.com/measureer/PadLend/releases/tag/v1.7.7',
  assets: [],
};

const newer: UpdateResult = {
  current: '1.7.6',
  latest: '1.7.7',
  newer: true,
  release,
  installer: {
    name: 'PadLend-1.7.7-setup.exe',
    size: 5_900_000,
    downloadUrl: 'https://github.com/measureer/PadLend/releases/download/v1.7.7/PadLend-1.7.7-setup.exe',
  },
  detailed: true,
};

describe('关于页面', () => {
  it('显示版本、仓库地址和检查更新入口', () => {
    useAppStore.setState({
      version: '1.7.7',
      configPath: 'C:\\Users\\me\\AppData\\Roaming\\PadLend\\config.yaml',
      update: { checking: false, result: null, error: '', checkedAt: '' },
    });
    render(<AboutPage />);
    expect(screen.getByRole('heading', { level: 1, name: '关于' })).toBeTruthy();
    // 版本号在页头和「当前版本」各出现一次。
    expect(screen.getAllByText('PadLend 1.7.7').length).toBeGreaterThan(0);
    expect(screen.getByText('https://github.com/measureer/PadLend')).toBeTruthy();
    expect(screen.getByRole('button', { name: '检查更新' })).toBeTruthy();
    expect(screen.getByText(/尚未检查/)).toBeTruthy();
  });

  it('发现新版本时给出下载入口', () => {
    useAppStore.setState({
      version: '1.7.6',
      update: { checking: false, result: newer, error: '', checkedAt: '2026/11/15 02:10:00' },
    });
    render(<AboutPage />);
    expect(screen.getByText(/发现新版本/)).toBeTruthy();
    const download = screen.getByRole('button', { name: /下载新版本/ });
    expect(download).toBeTruthy();
    fireEvent.click(download);
  });
});
