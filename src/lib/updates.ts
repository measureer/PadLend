/** 检查更新：从 GitHub Releases 读最新发布，与本机版本比较。
 *
 * 主路径走 `/releases/latest` 的**网页重定向**（不消耗 API 配额，重定向后的 URL 就是 tag），
 * 再尽量用 REST API 补充发布说明与资产列表；API 401/403/限流都只是「拿不到详情」，不影响判断版本。
 * （实测 api.github.com 在共享出口 IP 上很容易 403 rate limit，只用 API 会经常查不了。）
 *
 * 只依赖浏览器 fetch：WebView2 走系统代理，正好覆盖需要代理的机器。不发任何本地信息。
 */

export const RELEASES_REPO = 'measureer/PadLend';
export const RELEASES_PAGE = `https://github.com/${RELEASES_REPO}/releases`;
export const LATEST_PAGE = `${RELEASES_PAGE}/latest`;
export const RELEASES_API = `https://api.github.com/repos/${RELEASES_REPO}/releases/latest`;

export interface ReleaseAsset {
  name: string;
  size: number;
  downloadUrl: string;
}

export interface LatestRelease {
  tag: string;
  name: string;
  notes: string;
  publishedAt: string;
  pageUrl: string;
  assets: ReleaseAsset[];
}

export interface UpdateResult {
  current: string;
  latest: string;
  newer: boolean;
  release: LatestRelease;
  /** 优先挑 NSIS 安装器，其次任意 exe，最后 zip；没有资产时按发布约定推算。 */
  installer: ReleaseAsset | null;
  /** true 表示拿到了发布说明/资产列表（REST API 成功）。 */
  detailed: boolean;
}

/** "v1.7.5" / "1.7.5-beta.1" → [1,7,5]；无法解析返回 null。 */
export function parseVersion(text: string): number[] | null {
  const match = /^(?:v|V)?(\d+(?:\.\d+)*)/.exec(String(text ?? '').trim());
  if (!match) return null;
  return match[1].split('.').map((part) => Number(part));
}

/** 比较两个版本号：a 新于 b 返回 1，相同 0，旧于 -1；无法解析返回 0。 */
export function compareVersions(a: string, b: string): number {
  const left = parseVersion(a);
  const right = parseVersion(b);
  if (!left || !right) return 0;
  const length = Math.max(left.length, right.length);
  for (let i = 0; i < length; i += 1) {
    const diff = (left[i] ?? 0) - (right[i] ?? 0);
    if (diff !== 0) return diff > 0 ? 1 : -1;
  }
  return 0;
}

/** 新版本安装包：优先 *-setup.exe，其次任意 .exe，最后 win64.zip。 */
export function pickInstaller(assets: ReleaseAsset[]): ReleaseAsset | null {
  const lower = (asset: ReleaseAsset) => asset.name.toLowerCase();
  return (
    assets.find((asset) => lower(asset).endsWith('-setup.exe')) ??
    assets.find((asset) => lower(asset).endsWith('.exe')) ??
    assets.find((asset) => lower(asset).endsWith('.zip')) ??
    null
  );
}

export function humanSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '';
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/** 重定向后的 URL → 发布 tag（`/releases/tag/v1.7.6` → `v1.7.6`）。 */
export function tagFromUrl(url: string): string | null {
  const match = /\/releases\/tag\/([^/?#]+)/.exec(String(url ?? ''));
  return match ? decodeURIComponent(match[1]) : null;
}

/** 按本仓库的发布命名约定推算资产下载地址（release.ps1 就是这么命名的）。 */
export function guessInstaller(tag: string): ReleaseAsset | null {
  const version = tag.replace(/^v/i, '');
  if (!/^\d+(\.\d+)*$/.test(version)) return null;
  const name = `PadLend-${version}-setup.exe`;
  return { name, size: 0, downloadUrl: `${RELEASES_PAGE}/download/${tag}/${name}` };
}

/** GitHub 一次 release 的 JSON → 我们关心的字段（宽松解析，字段缺失不抛）。 */
export function parseRelease(raw: unknown): LatestRelease {
  const data = (raw ?? {}) as Record<string, unknown>;
  const assets = Array.isArray(data.assets) ? data.assets : [];
  return {
    tag: String(data.tag_name ?? ''),
    name: String(data.name ?? data.tag_name ?? ''),
    notes: String(data.body ?? ''),
    publishedAt: String(data.published_at ?? ''),
    pageUrl: String(data.html_url ?? RELEASES_PAGE),
    assets: assets
      .map((item) => {
        const asset = (item ?? {}) as Record<string, unknown>;
        return {
          name: String(asset.name ?? ''),
          size: Number(asset.size ?? 0),
          downloadUrl: String(asset.browser_download_url ?? ''),
        };
      })
      .filter((asset) => asset.name !== ''),
  };
}

export type Fetcher = (input: string, init?: RequestInit) => Promise<Response>;

function emptyRelease(tag: string): LatestRelease {
  return { tag, name: '', notes: '', publishedAt: '', pageUrl: `${RELEASES_PAGE}/tag/${tag}`, assets: [] };
}

/** 主路径：跟随 `/releases/latest` 的网页重定向，拿到最新 tag。 */
export async function findLatestTag(fetcher: Fetcher = fetch): Promise<string> {
  let response: Response;
  try {
    response = await fetcher(LATEST_PAGE, { redirect: 'follow', cache: 'no-store' });
  } catch (error) {
    throw new Error(`无法连接 GitHub：${(error as Error).message}`);
  }
  if (response.status === 404) throw new Error('仓库还没有发布任何 Release');
  if (!response.ok) throw new Error(`GitHub 返回 HTTP ${response.status}`);
  const tag = tagFromUrl(response.url);
  if (!tag) throw new Error('没有找到发布标签（可能还没有发布任何 Release）');
  return tag;
}

/** 外壳（Rust/WinHTTP）给出的 tag + 可选 release JSON → UpdateResult。 */
export function resultFromTag(current: string, tag: string, releaseJson?: unknown): UpdateResult {
  if (releaseJson && typeof releaseJson === 'object') {
    const release = parseRelease(releaseJson);
    if (release.tag) return buildResult(current, release, true);
    release.tag = tag;
    return buildResult(current, release, true);
  }
  return buildResult(current, emptyRelease(tag), false);
}

function buildResult(current: string, release: LatestRelease, detailed: boolean): UpdateResult {
  const latest = release.tag.replace(/^v/i, '');
  return {
    current,
    latest: latest || current,
    newer: compareVersions(latest, current) > 0,
    release,
    installer: pickInstaller(release.assets) ?? guessInstaller(release.tag),
    detailed,
  };
}

/** 检查更新：先拿 tag，再尽量补详情；网络/无 Release 才抛错。 */
export async function fetchLatestRelease(current: string, fetcher: Fetcher = fetch): Promise<UpdateResult> {
  let tag: string;
  try {
    tag = await findLatestTag(fetcher);
  } catch (error) {
    // 网页端失败（被代理拦、DNS 等）时退回 REST API 再试一次。
    let response: Response;
    try {
      response = await fetcher(RELEASES_API, { headers: { Accept: 'application/vnd.github+json' }, cache: 'no-store' });
    } catch (apiError) {
      throw new Error(`${(error as Error).message}；API 也失败：${(apiError as Error).message}`);
    }
    if (response.status === 404) throw new Error('仓库还没有发布任何 Release');
    if (response.status === 403 || response.status === 429) {
      throw new Error('GitHub 接口限流，请稍后再试');
    }
    if (!response.ok) throw new Error(`GitHub 返回 HTTP ${response.status}`);
    return buildResult(current, parseRelease(await response.json()), true);
  }

  // 详情失败只是少了说明和资产列表，版本判断照常给结论。
  try {
    const response = await fetcher(RELEASES_API, { headers: { Accept: 'application/vnd.github+json' }, cache: 'no-store' });
    if (response.ok) {
      const release = parseRelease(await response.json());
      if (release.tag) return buildResult(current, release, true);
    }
  } catch {
    // 忽略：用网页重定向拿到的 tag 就够了。
  }
  return buildResult(current, emptyRelease(tag), false);
}
