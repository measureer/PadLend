import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { UpdateDialog } from '../features/dialogs/UpdateDialog';
import { resultFromTag } from '../lib/updates';
import { useAppStore } from '../store/appStore';

const release = {
  tag_name: 'v1.7.6',
  name: 'PadLend 1.7.6',
  published_at: '2026-11-14T10:00:00Z',
  body: '- 修了某处',
  html_url: 'https://github.com/measureer/PadLend/releases/tag/v1.7.6',
  assets: [
    {
      name: 'PadLend-1.7.6-setup.exe',
      browser_download_url:
        'https://github.com/measureer/PadLend/releases/download/v1.7.6/PadLend-1.7.6-setup.exe',
      size: 5892557,
    },
  ],
};

function preset(version: string, tag: string, details: unknown) {
  useAppStore.setState({
    version,
    dialog: 'update',
    update: { checking: false, result: resultFromTag(version, tag, details), error: '', checkedAt: '2026/11/15 02:55:16' },
  });
  render(<UpdateDialog />);
}

describe('检查更新对话框', () => {
  it('发现新版本时列出安装包并给出下载入口', () => {
    preset('1.7.5', 'v1.7.6', release);
    expect(screen.getByText(/发现新版本/)).toBeTruthy();
    expect(screen.getByText('v1.7.6')).toBeTruthy();
    expect(screen.getByText(/PadLend-1.7.6-setup.exe/)).toBeTruthy();
    expect(screen.getByText('下载新版本…')).toBeTruthy();
    expect(screen.getByText(/- 修了某处/)).toBeTruthy();
    expect(useAppStore.getState().update.result?.newer).toBe(true);
  });

  it('已是最新时不显示下载按钮', () => {
    preset('1.7.6', 'v1.4.0', null);
    expect(screen.getByText(/已是最新版本/)).toBeTruthy();
    expect(screen.queryByText('下载新版本…')).toBeNull();
  });
});
