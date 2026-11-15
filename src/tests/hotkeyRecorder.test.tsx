import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { HotkeyRecorder } from '../features/settings/HotkeyRecorder';

describe('快捷键录制控件', () => {
  it('点一下进入录制态，按下组合键即写回', () => {
    const onChange = vi.fn();
    render(<HotkeyRecorder value="Ctrl+Alt+S" label="接管 / 交还" onChange={onChange} />);
    expect(screen.getByText('Ctrl')).toBeTruthy();

    fireEvent.click(screen.getByRole('button', { name: /接管 \/ 交还/ }));
    expect(screen.getByText(/按下组合键/)).toBeTruthy();

    fireEvent.keyDown(window, { code: 'KeyH', key: 'h', ctrlKey: true, altKey: true });
    expect(onChange).toHaveBeenCalledWith('Ctrl+Alt+H');
    // 录完退出录制态
    expect(screen.queryByText(/按下组合键/)).toBeNull();
  });

  it('只按修饰键时继续等待，不写回', () => {
    const onChange = vi.fn();
    render(<HotkeyRecorder value="" label="急停" onChange={onChange} />);
    fireEvent.click(screen.getByRole('button', { name: /急停/ }));
    fireEvent.keyDown(window, { code: 'ControlLeft', key: 'Control', ctrlKey: true });
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByText(/再按一次就设为双击 Ctrl/)).toBeTruthy();
  });

  it('连按两下同一个修饰键写成 Ctrl+Ctrl', () => {
    const onChange = vi.fn();
    render(<HotkeyRecorder value="Ctrl+Alt+S" label="接管 / 交还" onChange={onChange} />);
    fireEvent.click(screen.getByRole('button', { name: /接管 \/ 交还/ }));
    fireEvent.keyDown(window, { code: 'ControlLeft', key: 'Control', ctrlKey: true });
    expect(onChange).not.toHaveBeenCalled();
    fireEvent.keyDown(window, { code: 'ControlLeft', key: 'Control', ctrlKey: true });
    expect(onChange).toHaveBeenCalledWith('Ctrl+Ctrl');
  });

  it('Esc 取消录制，不改动原值', () => {
    const onChange = vi.fn();
    render(<HotkeyRecorder value="Ctrl+Alt+Q" label="急停" onChange={onChange} />);
    fireEvent.click(screen.getByRole('button', { name: /急停/ }));
    fireEvent.keyDown(window, { code: 'Escape', key: 'Escape' });
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByText('Ctrl')).toBeTruthy();
  });
});
