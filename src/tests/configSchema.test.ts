import { describe, expect, it } from 'vitest';
import { defaultConfig } from '../api/mock';
import {
  SETTING_GROUPS,
  readField,
  settingsPatch,
  validateConfig,
  writeField,
} from '../lib/configSchema';

const gainField = SETTING_GROUPS[0].fields[0];
const reportField = SETTING_GROUPS[0].fields[1];

describe('声明式设置 schema', () => {
  it('覆盖 13 个可编辑字段，且每个字段都有提示文案', () => {
    const fields = SETTING_GROUPS.flatMap((group) => group.fields);
    expect(fields).toHaveLength(13);
    expect(fields.every((field) => field.hint.length > 0)).toBe(true);
    expect(fields.map((f) => f.id)).toContain('close_to_tray');
    expect(fields.map((f) => f.id)).not.toContain('clipboard');

    const keepScreen = fields.find((f) => f.id === 'keep_screen_on');
    expect(keepScreen?.hint).toContain('临时把');
    expect(keepScreen?.hint).toContain('熄屏时间');
    expect(keepScreen?.hint).toContain('5 小时');
    expect(keepScreen?.hint).toContain('可以关闭');
  });

  it('速度滑条以 1/100 倍率显示并写回', () => {
    const config = defaultConfig();
    expect(readField(config, gainField)).toBe(100);
    const next = writeField(config, gainField, 155);
    expect(next.motion.gain).toBeCloseTo(1.55);
    expect(config.motion.gain).toBe(1.0); // 不可变更新
  });

  it('上报频率写回为数字，且只接受固定档位', () => {
    const config = defaultConfig();
    expect(readField(config, reportField)).toBe('1000');
    expect(writeField(config, reportField, '250').motion.report_hz).toBe(250);
    const invalid = { ...config, motion: { ...config.motion, report_hz: 300 } };
    expect(validateConfig(invalid)).toContain('motion.report_hz 必须是 125/250/500/1000 之一');
  });

  it('校验规则与 config.py 一致并回报具体字段', () => {
    const config = defaultConfig();
    config.motion.gain = 5;
    config.hotkeys.toggle = '';
    config.device.server_port = 80;
    const problems = validateConfig(config);
    expect(problems).toContain('motion.gain 必须在 0.25..3.0');
    expect(problems).toContain('hotkeys.toggle 不能为空');
    expect(problems).toContain('device.server_port 必须在 1024..65535');
  });

  it('设置页补丁不会覆盖按设备方向保存的 layouts', () => {
    const patch = settingsPatch(defaultConfig());
    expect(patch).not.toHaveProperty('layouts');
    expect(patch).not.toHaveProperty('version');
    expect(patch.motion).toBeDefined();
  });
});
