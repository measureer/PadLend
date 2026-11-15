/** 设置页：由 configSchema 一份声明驱动渲染，草稿本地化、只在保存时提交一次。 */

import { memo, useEffect, useRef, useState } from 'react';
import type { Config } from '../../api/types';
import { Icon, type IconName } from '../../components/Icon';
import { Banner, Button, Card, Field, Select, Slider, Toggle } from '../../components/ui';
import { SETTING_GROUPS, readField, validateConfig, settingsPatch, writeField, type FieldDef, type SettingGroup } from '../../lib/configSchema';
import { saveConfig } from '../../lib/actions';
import { useAppStore } from '../../store/appStore';
import { HotkeyRecorder } from './HotkeyRecorder';

/** 「生效方式」徽章：只有真的不是即时生效的字段才显示，避免每行都挂一个标签。 */
const APPLIES: Record<NonNullable<FieldDef['applies']>, { label: string; tone: string; title: string }> = {
  live: { label: '即时生效', tone: 'ok', title: '保存后引擎立刻重新加载，不用重连或重启。' },
  reconnect: { label: '重连后生效', tone: 'warn', title: '引擎在连接平板时读取这一项：保存后断开再连一次才会用上新值。' },
  restart: { label: '重启后生效', tone: 'warn', title: '引擎只在启动时读这一项：保存后需要重启 PadLend。' },
};

const FieldLabel = memo(function FieldLabel({ field }: { field: FieldDef }) {
  const applies = field.applies ? APPLIES[field.applies] : null;
  return (
    <span className="field-label-row">
      <span>{field.label}</span>
      {applies && <span className={`badge ${applies.tone}`} title={applies.title}>{applies.label}</span>}
    </span>
  );
});

function Control({ field, config, onChange }: {
  field: FieldDef;
  config: Config;
  onChange: (value: string | number | boolean) => void;
}) {
  const value = readField(config, field);
  switch (field.kind) {
    case 'toggle':
      return <Toggle checked={Boolean(value)} label={field.label} onChange={onChange} />;
    case 'slider':
      return (
        <Slider
          value={Number(value)}
          min={field.min ?? 0}
          max={field.max ?? 100}
          step={field.step ?? 1}
          label={field.label}
          format={field.format}
          onChange={onChange}
        />
      );
    case 'select':
      return (
        <Select
          value={String(value)}
          options={field.options ?? []}
          label={field.label}
          onChange={(next) => onChange(next)}
        />
      );
    case 'hotkey':
      return (
        <HotkeyRecorder
          value={String(value)}
          label={field.label}
          placeholder={field.placeholder}
          onChange={onChange}
        />
      );
    default:
      return null;
  }
}

const GroupCard = memo(function GroupCard({ group, config, onChange }: {
  group: SettingGroup;
  config: Config;
  onChange: (field: FieldDef, value: string | number | boolean) => void;
}) {
  return (
    <Card
      className="settings-group"
      title={<span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}><Icon name={group.icon as IconName} size={17} />{group.title}</span>}
      desc={group.description}
    >
      {group.fields.map((field) => (
        <Field key={field.id} label={<FieldLabel field={field} />} hint={field.hint}>
          <Control field={field} config={config} onChange={(value) => onChange(field, value)} />
        </Field>
      ))}
    </Card>
  );
});

export function SettingsPage() {
  const config = useAppStore((s) => s.config);
  const issues = useAppStore((s) => s.configIssues);
  const banner = useAppStore((s) => s.configBanner);
  const configPath = useAppStore((s) => s.configPath);
  const setBanner = useAppStore((s) => s.setConfigBanner);
  const [draft, setDraft] = useState<Config>(config);
  const [dirty, setDirty] = useState(false);
  const lastExternal = useRef(config);

  useEffect(() => {
    if (config !== lastExternal.current) {
      lastExternal.current = config;
      if (!dirty) setDraft(config);
    }
  }, [config, dirty]);

  const change = (field: FieldDef, value: string | number | boolean) => {
    setDraft((current) => writeField(current, field, value));
    setDirty(true);
    setBanner(null);
  };

  const save = async () => {
    const problems = validateConfig(draft);
    if (problems.length) {
      setBanner({ tone: 'err', text: `配置未保存：${problems.join('；')}` });
      return;
    }
    const ok = await saveConfig(settingsPatch(draft));
    if (ok) {
      lastExternal.current = draft;
      setDirty(false);
    }
  };

  return (
    <div className="page">
      <div className="page-head">
        <div>
          <h1 className="page-title">设置</h1>
          <p className="page-subtitle">鼠标、键盘、快捷键与杂项；标了「重连后生效」「重启后生效」的项保存后还需要对应动作。平板摆位在「桌面布局」页调整，按 Ctrl+, 可快速回到这里。</p>
        </div>
        <div className="page-actions">
          {dirty && <span className="badge warn">有未保存的修改</span>}
          <Button variant="primary" icon="save" onClick={() => void save()}>保存设置</Button>
        </div>
      </div>

      {issues.length > 0 && <Banner tone="warn">配置文件曾自动修复：{issues.join('；')}</Banner>}
      {banner && <Banner tone={banner.tone}>{banner.text}</Banner>}

      <div className="settings-grid">
        {(['main', 'side'] as const).map((column) => (
          <div className="settings-column" key={column}>
            {SETTING_GROUPS.filter((group) => group.column === column).map((group) => (
              <GroupCard key={group.id} group={group} config={draft} onChange={change} />
            ))}
          </div>
        ))}
      </div>

      <div className="settings-footer">
        <span className="muted" style={{ fontSize: 'var(--fs-12)' }}>
          配置文件：{configPath || '%APPDATA%\\PadLend\\config.yaml'} · 校验失败时会保留原文件并标红
        </span>
        <Button variant="primary" icon="save" onClick={() => void save()}>保存设置</Button>
      </div>
    </div>
  );
}
