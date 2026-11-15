/**
 * 指针校准：把平板指针钉到左上角，让用户一路推到屏幕最底部，
 * 用「平板高 ÷ 累计 HID 计数」反推 1 个计数对应多少像素。
 *
 * 为什么需要：Android 的指针速度在更上层且读不到（DESIGN F4），实测本机
 * 约 2.5~2.9 px/计数，而 xscale 理论值 1.312 —— 比例用错，估计器会与真实
 * 指针分叉，表现就是「指针还在屏幕中部就交还 PC」（PITFALLS 19/20）。
 */

import { useEffect, useRef, useState } from 'react';
import { Button, Field, Modal } from '../../components/ui';
import { calibrateBegin, calibrateEnd, calibrateState, type CalibrationReading } from '../../lib/calibration';
import { useAppStore } from '../../store/appStore';

type Phase = 'intro' | 'running' | 'done';

function Dialog({ current, onClose, onMeasured }: {
  current: number;
  onClose: () => void;
  onMeasured: (value: number) => void;
}) {
  const toggleKey = useAppStore((s) => s.config.hotkeys.toggle);
  const [phase, setPhase] = useState<Phase>('intro');
  const [reading, setReading] = useState<CalibrationReading | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const timer = useRef<number | null>(null);

  const stopPolling = () => {
    if (timer.current !== null) {
      window.clearInterval(timer.current);
      timer.current = null;
    }
  };
  // 对话框关掉/卸载时必须停掉轮询，否则后台一直打 RPC。
  useEffect(() => stopPolling, []);

  const begin = async () => {
    setError('');
    setBusy(true);
    stopPolling();
    try {
      const first = await calibrateBegin();
      setReading(first);
      setPhase('running');
      timer.current = window.setInterval(() => {
        void calibrateState()
          .then((next) => {
            setReading(next);
            // active=false 只有一种可能：用户交还了控制权（或链路断了）。
            if (!next.active) {
              stopPolling();
              setPhase('done');
            }
          })
          .catch((err: unknown) => {
            stopPolling();
            setError(err instanceof Error ? err.message : String(err));
            setPhase('done');
          });
      }, 400);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const cancel = () => {
    stopPolling();
    void calibrateEnd().catch(() => undefined);
    onClose();
  };

  const measured = reading && reading.pxPerCount > 0 ? reading.pxPerCount : 0;

  const footer = phase === 'intro'
    ? (
      <>
        <Button onClick={onClose}>取消</Button>
        <Button variant="primary" icon="play" loading={busy} onClick={() => void begin()}>开始校准</Button>
      </>
    )
    : phase === 'running'
      ? (
        <>
          <Button onClick={cancel}>取消</Button>
          <Button variant="primary" disabled>等待你交还控制…</Button>
        </>
      )
      : (
        <>
          <Button onClick={cancel}>关闭</Button>
          <Button onClick={() => void begin()}>重新校准</Button>
          <Button
            variant="primary"
            icon="check"
            disabled={measured <= 0}
            onClick={() => {
              // 百分比精度足够（模型只用它换算位移）。
              // 2.0.2：校准值可能落在 (0,1)（推得猛时指针被屏幕边缘夹住、像素
              // 不再增长，计数却继续累积），这种值过不了配置校验；夹到合法域
              // 1..10 再交给设置页。
              const value = Math.min(10, Math.max(1, Math.round(measured * 100) / 100));
              onMeasured(value);
              onClose();
            }}
          >
            使用这个结果
          </Button>
        </>
      );

  return (
    <Modal title="校准指针比例" subtitle="让引擎知道 1 个 HID 计数在平板上到底走多少像素" onClose={cancel} footer={footer}>
      <ol className="calib-steps">
        <li>点「开始校准」，引擎会把平板指针钉到<strong>左上角</strong>，并由 PadLend 接管鼠标。</li>
        <li>往下推鼠标，一直推到平板上的指针<strong>停在屏幕最底部</strong>再停手（只往下，别往上）。</li>
        <li>按 <kbd className="kbd">{toggleKey || 'Ctrl+Alt+S'}</kbd> 交还控制权（托盘菜单里的「接管 / 交还」也一样）。</li>
      </ol>
      {phase === 'running' && (
        <p className="calib-status">
          <span className="spinner" /> 校准中：已累计 <span className="mono">{reading?.countsY ?? 0}</span> 计数
          {reading && reading.height > 0 ? `（目标约为 ${reading.height} 像素高）` : ''}… 等你交还控制权。
        </p>
      )}
      {phase === 'done' && (
        measured > 0 ? (
          <p className="calib-status ok">
            测得 <span className="mono">{measured.toFixed(2)} px/计数</span>
            {' '}（累计 {reading?.countsY ?? 0} 计数 / 平板高 {reading?.height ?? 0} px）。
            <br />点「使用这个结果」写进设置里的「指针比例」，再点「保存设置」。
            推得越快这个数越小（指针加速），建议再往下调 10–20%：宁可略晚交还，也别在屏幕中间掉回 PC。
          </p>
        ) : (
          <p className="calib-status err">
            这次没测到有效位移（累计 {reading?.countsY ?? 0} 计数）。多半是还没推到屏幕底部就交还了，或者中途向上推了。点「重新校准」再来一次。
          </p>
        )
      )}
      {phase === 'intro' && current > 0 && (
        <p className="calib-hint">当前已校准为 {current.toFixed(2)} px/计数；重新校准会覆盖它。</p>
      )}
      {error && <p className="calib-status err">校准失败：{error}</p>}
    </Modal>
  );
}

/** 设置页「鼠标」组里的一行：显示当前值 + 打开校准对话框。 */
export function PointerCalibration({ value, onMeasured }: {
  value: number;
  onMeasured: (value: number) => void;
}) {
  const [open, setOpen] = useState(false);
  return (
    <Field
      label={(
        <span className="field-label-row">
          <span>指针校准</span>
          <span className="badge ok" title="保存后引擎立刻用新的比例换算，不用重连。">即时生效</span>
        </span>
      )}
      hint="不能读 Android 的指针速度，只能实测：点「校准…」把指针推到屏幕底部，测完自动填进上面的「指针比例」。实测值随手速浮动，填低一点更稳。"
    >
      <div className="calib-row">
        <span className="mono">{value > 0 ? `${value.toFixed(2)} px/计数` : '未校准（理论值）'}</span>
        <Button icon="sliders" onClick={() => setOpen(true)}>校准…</Button>
      </div>
      {open && (
        <Dialog current={value} onClose={() => setOpen(false)} onMeasured={onMeasured} />
      )}
    </Field>
  );
}
