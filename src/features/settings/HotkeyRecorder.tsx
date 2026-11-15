/** 快捷键录制控件：点一下进入录制态，按下组合键直接写回设置草稿。
 *
 * 为什么不再用文本框：用户能改文本但快捷键不生效，是因为文本没人解析、引擎只认写死的默认值。
 * 现在录制出来的写法与引擎 parse_combo 同一套规则（见 src/lib/hotkeys.ts），
 * 保存后引擎会重装绑定，立即生效。
 */

import { useEffect, useState } from 'react';
import { comboError, comboFromEvent, comboKeys, MODIFIER_CODES } from '../../lib/hotkeys';

interface HotkeyRecorderProps {
  value: string;
  label: string;
  placeholder?: string;
  onChange: (value: string) => void;
}

export function HotkeyRecorder({ value, label, placeholder, onChange }: HotkeyRecorderProps) {
  const [recording, setRecording] = useState(false);
  const [hint, setHint] = useState('');
  const error = recording ? '' : comboError(value);

  useEffect(() => {
    if (!recording) return undefined;
    const onKeyDown = (event: KeyboardEvent) => {
      // 录制期间吞掉所有按键：否则按 Ctrl+Alt+S 会顺手触发别的动作，Esc 也会关掉窗口。
      event.preventDefault();
      event.stopPropagation();
      if (event.code === 'Escape') {
        setRecording(false);
        setHint('');
        return;
      }
      if (event.code && MODIFIER_CODES.includes(event.code)) {
        setHint('再按一个主键，比如 S 或 PageUp');
        return;
      }
      const next = comboFromEvent(event);
      if (!next) {
        setHint('这个键不能用作快捷键（试试字母、数字、F1-F12 或方向键）');
        return;
      }
      const problem = comboError(next);
      if (problem) {
        setHint(problem);
        return;
      }
      onChange(next);
      setHint('');
      setRecording(false);
    };
    window.addEventListener('keydown', onKeyDown, true);
    return () => window.removeEventListener('keydown', onKeyDown, true);
  }, [recording, onChange]);

  const keys = comboKeys(value);

  return (
    <div className="hotkey-recorder">
      <button
        type="button"
        className={`hotkey-value ${recording ? 'recording' : ''} ${error ? 'invalid' : ''}`}
        aria-label={`${label}：${value || '未设置'}，点击录入新的组合键`}
        onClick={() => {
          setHint('');
          setRecording((current) => !current);
        }}
        onBlur={() => {
          setRecording(false);
          setHint('');
        }}
      >
        {recording ? (
          <span className="hotkey-placeholder">按下组合键…（Esc 取消）</span>
        ) : keys.length ? (
          <span className="hotkey-keys">
            {keys.map((key, index) => (
              <kbd className="kbd" key={`${key}-${index}`}>{key}</kbd>
            ))}
          </span>
        ) : (
          <span className="hotkey-placeholder">{placeholder || '点击录入'}</span>
        )}
        <span className="hotkey-action">{recording ? '录制中' : '点击修改'}</span>
      </button>
      {(hint || error) && <div className={`hotkey-note ${hint ? '' : 'err'}`}>{hint || error}</div>}
    </div>
  );
}
