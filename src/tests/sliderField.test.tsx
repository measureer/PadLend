/** 滑条 + 数值框双向同步（2.0.0：0.84× / 10 px 这类值可以直接输入）。 */

import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { Slider } from '../components/ui';

const GAIN = {
  min: 25,
  max: 300,
  step: 1,
  text: (value: number) => (value / 100).toFixed(2),
  parse: (text: string) => {
    const parsed = Number.parseFloat(text.replace(',', '.'));
    return Number.isFinite(parsed) ? Math.round(parsed * 100) : null;
  },
};

describe('滑条数值框', () => {
  it('输入 0.50 后按内部单位回调 50', () => {
    const onChange = vi.fn();
    render(<Slider value={84} label="速度" unit="×" onChange={onChange} {...GAIN} />);
    const box = screen.getByLabelText('速度（数值）');
    expect((box as HTMLInputElement).value).toBe('0.84');
    fireEvent.focus(box);
    fireEvent.change(box, { target: { value: '0.50' } });
    fireEvent.blur(box);
    expect(onChange).toHaveBeenCalledWith(50);
  });

  it('越界输入被夹到范围内，非法输入还原', () => {
    const onChange = vi.fn();
    const { rerender } = render(<Slider value={84} label="速度" onChange={onChange} {...GAIN} />);
    const box = screen.getByLabelText('速度（数值）');
    fireEvent.focus(box);
    fireEvent.change(box, { target: { value: '9' } });
    fireEvent.blur(box);
    expect(onChange).toHaveBeenLastCalledWith(300);

    onChange.mockClear();
    rerender(<Slider value={84} label="速度" onChange={onChange} {...GAIN} />);
    const again = screen.getByLabelText('速度（数值）');
    fireEvent.focus(again);
    fireEvent.change(again, { target: { value: 'abc' } });
    fireEvent.blur(again);
    expect(onChange).not.toHaveBeenCalled();
    expect((again as HTMLInputElement).value).toBe('0.84');
  });
});
