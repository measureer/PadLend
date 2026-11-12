/** 桌面布局页：可视化摆位、按设备方向保存、预览与正式生效分离。 */

import { useCallback, useEffect, useState } from 'react';
import { backend, RPC } from '../../api/client';
import type { Desktop, Placement } from '../../api/types';
import { Button } from '../../components/ui';
import { saveLayout } from '../../lib/actions';
import { useAppStore } from '../../store/appStore';
import { DesktopCanvas } from './DesktopCanvas';

export function LayoutPage() {
  const desktop = useAppStore((s) => s.desktop);
  const placement = useAppStore((s) => s.placement);
  const facts = useAppStore((s) => s.facts);
  const dirty = useAppStore((s) => s.layoutDirty);
  const setPlacement = useAppStore((s) => s.setPlacement);
  const setDesktop = useAppStore((s) => s.setDesktop);
  const [fitNonce, setFitNonce] = useState(0);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    let alive = true;
    void backend
      .call<Desktop>(RPC.layoutDesktop)
      .then((data) => { if (alive) setDesktop(data); })
      .catch(() => undefined);
    return () => { alive = false; };
  }, [setDesktop]);

  const onPlacementChange = useCallback((next: Placement | null) => {
    setPlacement(next, next !== null);
  }, [setPlacement]);

  const onSave = async () => {
    if (!placement) return;
    setSaving(true);
    await saveLayout(placement);
    setSaving(false);
  };

  return (
    <div className="page">
      <div className="page-head">
        <div>
          <h1 className="page-title">桌面布局</h1>
          <p className="page-subtitle">
            把平板拖到屏幕外轮廓边，点「保存布局」后才会生效；拖动和松手只是预览。每台设备的横屏、竖屏布局分别保存，旋转平板会自动切换。
          </p>
        </div>
        <div className="page-actions canvas-toolbar">
          {dirty && <span className="badge warn">有未保存的布局改动</span>}
          <Button icon="fit" onClick={() => setFitNonce((n) => n + 1)} title="缩放到同时看见所有显示器和平板；滚轮缩放，Ctrl+0 也可恢复">
            显示全部
          </Button>
          <Button variant="primary" icon="save" loading={saving} disabled={!placement} onClick={() => void onSave()}>
            保存布局
          </Button>
        </div>
      </div>

      <DesktopCanvas
        desktop={desktop}
        placement={placement}
        facts={facts}
        fitNonce={fitNonce}
        onPlacementChange={onPlacementChange}
      />

      <div className="banner">
        滚轮缩放 · 按住 Space 平移 · Ctrl+0 复位视野 · 拖动时按 Shift 切换角落优先边 · 双击平板切换横竖 · 右键调整对齐方式或测试穿越
      </div>
    </div>
  );
}
