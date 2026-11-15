/** 指针校准入口 + 对话框流程（mock 后端）。 */

import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { PointerCalibration } from '../features/settings/PointerCalibration';

describe('指针校准', () => {
  it('显示当前比例并打开校准对话框', () => {
    render(<PointerCalibration value={0} onMeasured={() => undefined} />);
    expect(screen.getByText('未校准（理论值）')).toBeTruthy();
    fireEvent.click(screen.getByText('校准…'));
    expect(screen.getByText('校准指针比例')).toBeTruthy();
    expect(screen.getByText('开始校准')).toBeTruthy();
  });

  it('开始校准后进入轮询，取消会关闭对话框', async () => {
    const onMeasured = vi.fn();
    render(<PointerCalibration value={2.86} onMeasured={onMeasured} />);
    expect(screen.getByText('2.86 px/计数')).toBeTruthy();
    fireEvent.click(screen.getByText('校准…'));
    fireEvent.click(screen.getByText('开始校准'));
    await waitFor(() => expect(screen.getByText(/校准中：已累计/)).toBeTruthy());
    fireEvent.click(screen.getByText('取消'));
    await waitFor(() => expect(screen.queryByText('校准指针比例')).toBeNull());
    expect(onMeasured).not.toHaveBeenCalled();
  });
});
