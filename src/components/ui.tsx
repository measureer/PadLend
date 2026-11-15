/** 基础组件库：所有页面共用同一套交互与视觉，杜绝重复样式代码。 */

import { memo, useEffect, useState, type ButtonHTMLAttributes, type ReactNode } from 'react';
import type { AppState } from '../api/types';
import { STATE_TONES, stateLabel } from '../lib/format';
import { useAppStore } from '../store/appStore';
import { Icon, type IconName } from './Icon';

type ButtonVariant = 'default' | 'primary' | 'ghost' | 'danger';

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  icon?: IconName;
  loading?: boolean;
  compact?: boolean;
}

export const Button = memo(function Button({
  variant = 'default', icon, loading, compact, className = '', children, disabled, ...rest
}: ButtonProps) {
  return (
    <button
      {...rest}
      className={`btn ${variant !== 'default' ? variant : ''} ${compact ? 'compact' : ''} ${className}`}
      disabled={disabled || loading}
    >
      {loading ? <span className="spinner" /> : icon ? <Icon name={icon} size={16} /> : null}
      {children}
    </button>
  );
});

interface IconButtonProps {
  icon: IconName;
  label: string;
  onClick?: () => void;
  active?: boolean;
  size?: number;
  className?: string;
}

export const IconButton = memo(function IconButton({
  icon, label, onClick, active, size = 18, className = '',
}: IconButtonProps) {
  return (
    <button
      type="button"
      className={`icon-btn ${active ? 'active' : ''} ${className}`}
      onClick={onClick}
      aria-label={label}
      title={label}
    >
      <Icon name={icon} size={size} />
    </button>
  );
});

interface CardProps {
  title?: ReactNode;
  desc?: ReactNode;
  actions?: ReactNode;
  footer?: ReactNode;
  className?: string;
  children: ReactNode;
}

export const Card = memo(function Card({ title, desc, actions, footer, className = '', children }: CardProps) {
  return (
    <section className={`card ${className}`}>
      {(title || actions) && (
        <header className="card-head">
          <div>
            {title && <h2 className="card-title">{title}</h2>}
            {desc && <p className="card-desc">{desc}</p>}
          </div>
          {actions}
        </header>
      )}
      <div className="card-body">{children}</div>
      {footer && <footer className="card-foot">{footer}</footer>}
    </section>
  );
});

interface FieldProps {
  label: ReactNode;
  hint?: ReactNode;
  children: ReactNode;
}

export const Field = memo(function Field({ label, hint, children }: FieldProps) {
  return (
    <div className="field-row">
      <div>
        <div className="field-label">{label}</div>
        {hint && <div className="field-hint">{hint}</div>}
      </div>
      <div className="field-control">{children}</div>
    </div>
  );
});

interface ToggleProps {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
}

export const Toggle = memo(function Toggle({ checked, onChange, label }: ToggleProps) {
  return (
    <button
      type="button"
      className="toggle"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      onClick={() => onChange(!checked)}
    />
  );
});

interface SliderProps {
  value: number;
  min: number;
  max: number;
  step?: number;
  onChange: (value: number) => void;
  format?: (value: number) => string;
  /** 数值框里显示的文本（默认直接显示 value，例如 0.84 需要换算成 84）。 */
  text?: (value: number) => string;
  /** 把数值框里输入的文本解析回内部值；返回 null 表示输入无效。 */
  parse?: (text: string) => number | null;
  unit?: string;
  label: string;
}

/** 滑条 + 可输入数值框：两者双向同步，回车/失焦提交，Esc 还原。 */
export const Slider = memo(function Slider({
  value, min, max, step = 1, onChange, format, text, parse, unit, label,
}: SliderProps) {
  const toText = (next: number) => (text ? text(next) : String(next));
  const [draft, setDraft] = useState(() => toText(value));
  const [editing, setEditing] = useState(false);
  useEffect(() => {
    if (!editing) setDraft(toText(value));
    // toText 依赖调用方传入的 text/format，随 value 一起变
  }, [value, editing, text, format]);
  const commit = () => {
    const parsed = parse ? parse(draft) : Number(draft);
    if (parsed === null || !Number.isFinite(parsed)) {
      setDraft(toText(value));
      return;
    }
    const stepped = step > 0 ? Math.round(parsed / step) * step : parsed;
    const next = Math.min(max, Math.max(min, stepped));
    setDraft(toText(next));
    if (next !== value) onChange(next);
  };
  return (
    <>
      <input
        className="slider"
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        aria-label={label}
        onChange={(e) => onChange(Number(e.target.value))}
      />
      <span className="slider-input">
        <input
          className="input field-number mono"
          value={draft}
          aria-label={`${label}（数值）`}
          onFocus={() => setEditing(true)}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={() => { setEditing(false); commit(); }}
          onKeyDown={(e) => {
            if (e.key === 'Enter') e.currentTarget.blur();
            if (e.key === 'Escape') { setDraft(toText(value)); setEditing(false); e.currentTarget.blur(); }
          }}
        />
        {unit ? <span className="field-unit mono">{unit}</span> : null}
      </span>
    </>
  );
});

interface NumberFieldProps {
  value: number;
  min: number;
  max: number;
  step?: number;
  onChange: (value: number) => void;
  /** 框里显示的文本（默认直接显示 value）。 */
  text?: (value: number) => string;
  /** 把输入文本解析回内部值；返回 null 表示输入无效。 */
  parse?: (text: string) => number | null;
  unit?: string;
  label: string;
}

/** 纯数字输入框（没有滑条）：回车 / 失焦提交，按 min/max/step 规范化，Esc 还原。 */
export const NumberField = memo(function NumberField({
  value, min, max, step = 1, onChange, text, parse, unit, label,
}: NumberFieldProps) {
  const toText = (next: number) => (text ? text(next) : String(next));
  const [draft, setDraft] = useState(() => toText(value));
  const [editing, setEditing] = useState(false);
  useEffect(() => {
    if (!editing) setDraft(toText(value));
    // toText 依赖调用方传入的 text，随 value 一起变
  }, [value, editing, text]);
  const commit = () => {
    const parsed = parse ? parse(draft) : Number(draft);
    if (parsed === null || !Number.isFinite(parsed)) {
      setDraft(toText(value));
      return;
    }
    const stepped = step > 0 ? Number((Math.round(parsed / step) * step).toFixed(4)) : parsed;
    const next = Math.min(max, Math.max(min, stepped));
    setDraft(toText(next));
    if (next !== value) onChange(next);
  };
  return (
    <span className="number-field">
      <input
        className="input field-number mono"
        value={draft}
        aria-label={label}
        onFocus={() => setEditing(true)}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => { setEditing(false); commit(); }}
        onKeyDown={(e) => {
          if (e.key === 'Enter') e.currentTarget.blur();
          if (e.key === 'Escape') { setDraft(toText(value)); setEditing(false); e.currentTarget.blur(); }
        }}
      />
      {unit ? <span className="field-unit mono">{unit}</span> : null}
    </span>
  );
});

interface SelectProps {
  value: string;
  options: { value: string; label: string }[];
  onChange: (value: string) => void;
  label: string;
}

export const Select = memo(function Select({ value, options, onChange, label }: SelectProps) {
  return (
    <select className="select" value={value} aria-label={label} onChange={(e) => onChange(e.target.value)}>
      {options.map((option) => (
        <option key={option.value} value={option.value}>{option.label}</option>
      ))}
    </select>
  );
});

interface TextInputProps {
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  invalid?: boolean;
  maxLength?: number;
  label: string;
  onKeyDown?: (event: React.KeyboardEvent<HTMLInputElement>) => void;
}

export const TextInput = memo(function TextInput({
  value, onChange, placeholder, invalid, maxLength, label, onKeyDown,
}: TextInputProps) {
  return (
    <input
      className={`input ${invalid ? 'invalid' : ''}`}
      value={value}
      placeholder={placeholder}
      maxLength={maxLength}
      aria-label={label}
      onChange={(e) => onChange(e.target.value)}
      onKeyDown={onKeyDown}
    />
  );
});

export const StatusPill = memo(function StatusPill({ state }: { state: AppState }) {
  const tone = STATE_TONES[state] ?? 'disconnected';
  return (
    <div className="status-pill" data-tone={tone} title={`当前状态：${stateLabel(state)}`}>
      <span className="status-dot" />
      <span>{stateLabel(state)}</span>
    </div>
  );
});

export const Banner = memo(function Banner({ tone, children }: { tone: 'ok' | 'err' | 'warn'; children: ReactNode }) {
  return (
    <div className={`banner ${tone}`}>
      <Icon name={tone === 'ok' ? 'check' : 'alert'} size={15} />
      <span>{children}</span>
    </div>
  );
});

interface ModalProps {
  title: string;
  subtitle?: ReactNode;
  footer?: ReactNode;
  wide?: boolean;
  onClose: () => void;
  children: ReactNode;
}

export function Modal({ title, subtitle, footer, wide, onClose, children }: ModalProps) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);
  return (
    <div className="modal-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className={`modal ${wide ? 'wide' : ''}`} role="dialog" aria-modal="true" aria-label={title}>
        <header className="modal-head">
          <div>
            <h2 className="modal-title">{title}</h2>
            {subtitle && <p className="modal-subtitle">{subtitle}</p>}
          </div>
          <IconButton icon="close" label="关闭" onClick={onClose} />
        </header>
        <div className="modal-body">{children}</div>
        {footer && <footer className="modal-foot">{footer}</footer>}
      </div>
    </div>
  );
}

export function ToastStack() {
  const toasts = useAppStore((s) => s.toasts);
  const dismiss = useAppStore((s) => s.dismissToast);
  useEffect(() => {
    if (!toasts.length) return;
    const timers = toasts.map((toast) => window.setTimeout(() => dismiss(toast.id), 3800));
    return () => timers.forEach(window.clearTimeout);
  }, [toasts, dismiss]);
  return (
    <div className="toast-stack" aria-live="polite">
      {toasts.map((toast) => (
        <div key={toast.id} className={`toast ${toast.tone}`} onClick={() => dismiss(toast.id)}>
          {toast.text}
        </div>
      ))}
    </div>
  );
}

export const EmptyState = memo(function EmptyState({ title, hint }: { title: string; hint: ReactNode }) {
  return (
    <div className="device-empty">
      <div className="empty-art" />
      <div style={{ fontWeight: 600, color: 'var(--text)' }}>{title}</div>
      <div style={{ maxWidth: 380, whiteSpace: 'pre-line', fontSize: 'var(--fs-12)' }}>{hint}</div>
    </div>
  );
});
