import { describe, expect, it } from 'vitest';
import {
  comboError,
  comboFromEvent,
  comboKeys,
  doubleTapModifier,
  keyNameFromCode,
  modifierNameFromCode,
} from '../lib/hotkeys';

/** 造一个最小键盘事件（只带录制控件真正读的四个修饰位）。 */
function key(code: string, mods: Partial<{ ctrl: boolean; alt: boolean; shift: boolean; win: boolean }> = {}, value = '') {
  return {
    code,
    key: value,
    ctrlKey: Boolean(mods.ctrl),
    altKey: Boolean(mods.alt),
    shiftKey: Boolean(mods.shift),
    metaKey: Boolean(mods.win),
  };
}

describe('快捷键录制与校验（与引擎 parse_combo 对齐）', () => {
  it('把常见按键映射成引擎认得的名字', () => {
    expect(keyNameFromCode('KeyS')).toBe('S');
    expect(keyNameFromCode('Digit1')).toBe('1');
    expect(keyNameFromCode('F5')).toBe('F5');
    expect(keyNameFromCode('PageUp')).toBe('PageUp');
    expect(keyNameFromCode('ArrowUp')).toBe('Up');
    expect(keyNameFromCode('Escape')).toBe('Escape');
    expect(keyNameFromCode('Semicolon')).toBe('Semicolon');
    // 小键盘乘号这类引擎没有的键返回 null，录制时提示换一个。
    expect(keyNameFromCode('NumpadMultiply')).toBeNull();
  });

  it('录制出 Ctrl+Alt+X 这类文本', () => {
    expect(comboFromEvent(key('KeyB', { ctrl: true, alt: true }))).toBe('Ctrl+Alt+B');
    expect(comboFromEvent(key('PageUp', { ctrl: true, alt: true }))).toBe('Ctrl+Alt+PageUp');
    // 只按修饰键不产出文本：录制控件要继续等下一位。
    expect(comboFromEvent(key('ControlLeft', { ctrl: true }))).toBeNull();
  });

  it('校验规则：必须带 Ctrl / Alt / Win，且主键要认得', () => {
    expect(comboError('Ctrl+Alt+S')).toBe('');
    expect(comboError('Ctrl+Shift+F5')).toBe('');
    expect(comboError('Shift+B')).toContain('至少要有 Ctrl / Alt / Win');
    expect(comboError('')).toContain('不能为空');
    // "Ctrl++" 与引擎一致：末段为空表示主键就是 +。
    expect(comboError('Ctrl+')).toBe('');
    expect(comboError('Ctrl+Alt')).toContain('缺少主键');
    expect(comboError('Ctrl+A+B')).toContain('只能有一个主键');
    expect(comboError('Ctrl+Alt+NumpadMultiply')).toContain('不支持的按键');
  });

  it('双击修饰键：Ctrl+Ctrl 合法，Ctrl+Alt 仍算缺主键', () => {
    expect(doubleTapModifier('Ctrl+Ctrl')).toBe('Ctrl');
    expect(doubleTapModifier(' alt + alt ')).toBe('Alt');
    expect(doubleTapModifier('Ctrl+Alt')).toBeNull();
    expect(doubleTapModifier('Ctrl+S')).toBeNull();
    expect(comboError('Ctrl+Ctrl')).toBe('');
    expect(comboError('Alt+Alt')).toBe('');
    expect(comboError('Ctrl+Alt')).toContain('缺少主键');
    expect(comboError('Ctrl+Shift')).toContain('缺少主键');
    expect(modifierNameFromCode('ControlLeft')).toBe('Ctrl');
    expect(modifierNameFromCode('AltRight')).toBe('Alt');
    expect(modifierNameFromCode('MetaLeft')).toBe('Win');
    expect(modifierNameFromCode('KeyA')).toBeNull();
  });

  it('键帽拆分用于渲染，不丢 + 这种主键', () => {
    expect(comboKeys('Ctrl+Alt+PageUp')).toEqual(['Ctrl', 'Alt', 'PageUp']);
    expect(comboKeys('')).toEqual([]);
  });
});
