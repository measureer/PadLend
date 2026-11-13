/** 首次设置向导：3 步检测，全部在服务进程/线程池执行，界面保持可交互。 */

import { useEffect, useState } from 'react';
import { Icon } from '../../components/Icon';
import { Button, Modal, Toggle } from '../../components/ui';
import { wizardCheck, wizardFinish } from '../../lib/actions';
import { useAppStore } from '../../store/appStore';

const STEPS = [
  {
    title: '连接平板',
    subtitle: '用数据线连接平板，并在平板上允许 USB 调试。adb 已随 PadLend 内置，无需另装。',
    action: '开始检测',
  },
  {
    title: '检查 UHID',
    subtitle: '虚拟鼠标/键盘依赖 /dev/uhid 可写；scrcpy server 已随包内置。',
    action: '开始检测',
  },
  { title: '完成', subtitle: '一切就绪，回到主窗口开始使用。', action: '' },
];

type CheckState = 'idle' | 'running' | 'ok' | 'warn';

export function SetupWizard() {
  const close = () => useAppStore.getState().openDialog(null);
  const config = useAppStore((s) => s.config);
  const [step, setStep] = useState(0);
  const [states, setStates] = useState<CheckState[]>(STEPS.map(() => 'idle'));
  const [results, setResults] = useState<string[]>(STEPS.map(() => ''));
  const [autoConnect, setAutoConnect] = useState(config.device.auto_connect);
  const [closeToTray, setCloseToTray] = useState(config.ui.close_to_tray);
  const [busy, setBusy] = useState(false);

  const runCheck = async (index: number) => {
    setStates((current) => current.map((value, i) => (i === index ? 'running' : value)));
    setBusy(true);
    try {
      const result = await wizardCheck(index);
      setStates((current) => current.map((value, i) => (i === index ? (result.ok ? 'ok' : 'warn') : value)));
      setResults((current) => current.map((value, i) => (i === index ? result.text : value)));
    } catch (error) {
      setStates((current) => current.map((value, i) => (i === index ? 'warn' : value)));
      setResults((current) => current.map((value, i) => (i === index ? `检测失败：${(error as Error).message}` : value)));
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    if (step < STEPS.length - 1 && states[step] === 'idle') void runCheck(step);
  }, [step]); // eslint-disable-line react-hooks/exhaustive-deps

  const finish = async () => {
    setBusy(true);
    await wizardFinish(autoConnect, closeToTray);
    setBusy(false);
    close();
  };

  const stateLabel = (value: CheckState) => ({ idle: '尚未检测', running: '检测中 …', ok: '检测通过', warn: '需要处理' })[value];
  const stateTone = (value: CheckState) => value === 'ok' ? 'var(--ok)' : value === 'warn' ? 'var(--warn)' : 'var(--text-2)';

  return (
    <Modal
      wide
      title="PadLend 首次设置"
      subtitle="检查平板连接与 UHID；adb 和 scrcpy-server 已随安装包内置。"
      onClose={close}
      footer={
        <>
          <Button variant="ghost" onClick={close}>取消</Button>
          <Button icon="chevronLeft" disabled={step === 0} onClick={() => setStep((n) => n - 1)}>上一步</Button>
          {step < STEPS.length - 1 ? (
            <Button variant="primary" icon="chevronRight" onClick={() => setStep((n) => n + 1)}>下一步</Button>
          ) : (
            <Button variant="primary" icon="check" loading={busy} onClick={() => void finish()}>完成</Button>
          )}
        </>
      }
    >
      <div className="wizard-steps">
        {STEPS.map((item, index) => (
          <div key={item.title} className={`wizard-step ${index === step ? 'active' : ''} ${index < step ? 'done' : ''}`}>
            <span className="num">{index < step ? '✓' : index + 1}</span>
            <span>{item.title}</span>
          </div>
        ))}
      </div>
      <div className="wizard-body" style={{ paddingTop: 'var(--sp-5)' }}>
        <h3 className="card-title">{step + 1} / {STEPS.length} · {STEPS[step].title}</h3>
        <p className="card-desc">{STEPS[step].subtitle}</p>

        {step < STEPS.length - 1 && (
          <>
            <div className="checkline" style={{ marginTop: 'var(--sp-4)' }}>
              <span style={{ color: stateTone(states[step]) }}>
                <Icon name={states[step] === 'ok' ? 'check' : states[step] === 'warn' ? 'alert' : 'info'} size={16} />
              </span>
              <strong style={{ color: stateTone(states[step]) }}>{stateLabel(states[step])}</strong>
              <Button compact icon="refresh" loading={busy || states[step] === 'running'} onClick={() => void runCheck(step)}>
                {STEPS[step].action}
              </Button>
            </div>
            {results[step] && <div className="wizard-result">{results[step]}</div>}
          </>
        )}

        {step === STEPS.length - 1 && (
          <div style={{ marginTop: 'var(--sp-4)', display: 'grid', gap: 'var(--sp-3)' }}>
            <div className="field-row">
              <div><div className="field-label">启动时自动连接平板</div><div className="field-hint">打开应用后自动连上次那台平板。</div></div>
              <div className="field-control"><Toggle checked={autoConnect} label="启动时自动连接平板" onChange={setAutoConnect} /></div>
            </div>
            <div className="field-row">
              <div><div className="field-label">连接后自动进入托盘</div><div className="field-hint">关闭窗口不退出，图标通常被 Windows 11 收在任务栏右下角的「^」里。</div></div>
              <div className="field-control"><Toggle checked={closeToTray} label="关闭窗口时最小化到托盘" onChange={setCloseToTray} /></div>
            </div>
            <div className="help-block">
              热键：Ctrl+Alt+S 手动接管 / 交还；Ctrl+Alt+Q 急停。<br />
              顶栏 ⋯ →「运行诊断…」可以采集并导出脱敏诊断包。
            </div>
          </div>
        )}
      </div>
    </Modal>
  );
}
