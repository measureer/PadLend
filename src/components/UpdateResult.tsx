/** 更新检查结果的展示块：发现新版本 / 已是最新 / 安装包名与大小三段。
 * 关于页与更新对话框原来各写一份同样的 JSX，文案容易漂移；现在共用这里。 */

import type { UpdateResult } from '../lib/updates';
import { formatReleaseDate, humanSize } from '../lib/updates';

interface UpdateResultViewProps {
  result: UpdateResult;
  /** 最近一次检查的本地时间（没有则为空串，展示时省略）。 */
  checkedAt: string;
}

export function UpdateResultView({ result, checkedAt }: UpdateResultViewProps) {
  const installer = result.installer ?? null;
  if (!result.newer) {
    return <div>已是最新版本{checkedAt ? `（${checkedAt} 检查）` : ''}。</div>;
  }
  return (
    <>
      <div>发现新版本：<strong>v{result.latest}</strong>{result.release.name ? `（${result.release.name}）` : ''}</div>
      <div className="muted">
        发布于 {formatReleaseDate(result.release.publishedAt) || '时间未知'}
        {installer ? ` · ${installer.name}${installer.size ? ` · ${humanSize(installer.size)}` : ''}` : ' · 该发布没有可直接下载的安装包'}
      </div>
    </>
  );
}
