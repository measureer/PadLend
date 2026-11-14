/** 检查更新的纯逻辑：版本比较、安装包推算、网页重定向与 API 兜底、错误处理。 */

import { describe, expect, it, vi } from "vitest";
import {
  compareVersions,
  resultFromTag,
  fetchLatestRelease,
  findLatestTag,
  guessInstaller,
  humanSize,
  LATEST_PAGE,
  parseRelease,
  parseVersion,
  pickInstaller,
  tagFromUrl,
} from "../lib/updates";

const release = {
  tag_name: "v1.7.6",
  name: "PadLend 1.7.6",
  body: "- 修复了某处",
  published_at: "2026-11-16T02:00:00Z",
  html_url: "https://github.com/measureer/PadLend/releases/tag/v1.7.6",
  assets: [
    { name: "PadLend-1.7.6-win64.zip", size: 7_900_000, browser_download_url: "https://example.com/p.zip" },
    { name: "PadLend-1.7.6-setup.exe", size: 5_800_000, browser_download_url: "https://example.com/p.exe" },
  ],
};

function redirect(tag: string): Response {
  return {
    ok: true,
    status: 200,
    url: "https://github.com/measureer/PadLend/releases/tag/" + tag,
    json: async () => ({}),
  } as unknown as Response;
}

function reply(body: unknown, status = 200): Response {
  return { ok: status >= 200 && status < 300, status, url: LATEST_PAGE, json: async () => body } as unknown as Response;
}

describe("版本比较", () => {
  it("解析带 v 前缀与多段版本号", () => {
    expect(parseVersion("v1.7.5")).toEqual([1, 7, 5]);
    expect(parseVersion("1.8")).toEqual([1, 8]);
    expect(parseVersion("abc")).toBeNull();
  });

  it("按段比较，缺段当 0", () => {
    expect(compareVersions("1.7.6", "1.7.5")).toBe(1);
    expect(compareVersions("v1.7.5", "1.7.5")).toBe(0);
    expect(compareVersions("1.7", "1.7.10")).toBe(-1);
    expect(compareVersions("2.0.0", "1.99.99")).toBe(1);
    expect(compareVersions("坏版本", "1.0.0")).toBe(0);
  });
});

describe("安装包挑选与推算", () => {
  it("优先 setup.exe，其次任意 exe，最后 zip", () => {
    expect(pickInstaller(parseRelease(release).assets)?.name).toBe("PadLend-1.7.6-setup.exe");
    expect(pickInstaller([{ name: "a.zip", size: 1, downloadUrl: "" }, { name: "b.exe", size: 1, downloadUrl: "" }])?.name).toBe("b.exe");
    expect(pickInstaller([{ name: "a.zip", size: 1, downloadUrl: "" }])?.name).toBe("a.zip");
    expect(pickInstaller([])).toBeNull();
  });

  it("没有资产列表时按发布命名约定推算安装器", () => {
    const asset = guessInstaller("v1.7.6");
    expect(asset?.name).toBe("PadLend-1.7.6-setup.exe");
    expect(asset?.downloadUrl).toBe("https://github.com/measureer/PadLend/releases/download/v1.7.6/PadLend-1.7.6-setup.exe");
    expect(guessInstaller("nightly")).toBeNull();
  });

  it("人类可读大小", () => {
    expect(humanSize(5_800_000)).toBe("5.5 MB");
    expect(humanSize(800_000)).toBe("781 KB");
    expect(humanSize(0)).toBe("");
  });
});

describe("GitHub 响应解析与检查", () => {
  it("从重定向地址取 tag", () => {
    expect(tagFromUrl("https://github.com/a/b/releases/tag/v1.7.6")).toBe("v1.7.6");
    expect(tagFromUrl("https://github.com/a/b/releases/tag/v1.7.6?x=1")).toBe("v1.7.6");
    expect(tagFromUrl("https://github.com/a/b/releases")).toBeNull();
  });

  it("解析 release 字段与资产", () => {
    const parsed = parseRelease(release);
    expect(parsed.tag).toBe("v1.7.6");
    expect(parsed.assets).toHaveLength(2);
    expect(parsed.assets[1].downloadUrl).toBe("https://example.com/p.exe");
    expect(parseRelease({}).assets).toEqual([]);
  });

  it("网页重定向可用时优先走它（不耗 API 配额）", async () => {
    const fetcher = vi.fn(async (url: string) => (url === LATEST_PAGE ? redirect("v1.7.6") : reply(release)));
    const result = await fetchLatestRelease("1.7.5", fetcher as never);
    expect(fetcher).toHaveBeenCalledWith(LATEST_PAGE, expect.objectContaining({ redirect: "follow" }));
    expect(result.newer).toBe(true);
    expect(result.latest).toBe("1.7.6");
    expect(result.detailed).toBe(true);
    expect(result.installer?.name).toBe("PadLend-1.7.6-setup.exe");
  });

  it("API 限流时仍能用 tag 判断版本，并推算下载地址", async () => {
    const fetcher = vi.fn(async (url: string) => (url === LATEST_PAGE ? redirect("v1.7.6") : reply({}, 403)));
    const result = await fetchLatestRelease("1.7.5", fetcher as never);
    expect(result.newer).toBe(true);
    expect(result.detailed).toBe(false);
    expect(result.release.tag).toBe("v1.7.6");
    expect(result.installer?.downloadUrl).toContain("/releases/download/v1.7.6/PadLend-1.7.6-setup.exe");
  });

  it("已是最新版本（GitHub 上反而更旧）", async () => {
    const fetcher = vi.fn(async (url: string) => (url === LATEST_PAGE ? redirect("v1.4.0") : reply({ ...release, tag_name: "v1.4.0" })));
    const result = await fetchLatestRelease("1.7.5", fetcher as never);
    expect(result.newer).toBe(false);
  });

  it("网页与 API 都失败时给出可读错误", async () => {
    const noTag = vi.fn(async (url: string) =>
      url === LATEST_PAGE
        ? ({ ok: true, status: 200, url: "https://github.com/measureer/PadLend/releases" } as unknown as Response)
        : reply({}, 404),
    );
    await expect(fetchLatestRelease("1.7.5", noTag as never)).rejects.toThrow("还没有发布任何 Release");

    const rateLimited = vi.fn(async (url: string) => {
      if (url === LATEST_PAGE) throw new Error("timeout");
      return reply({}, 403);
    });
    await expect(fetchLatestRelease("1.7.5", rateLimited as never)).rejects.toThrow("限流");
  });

  it("外壳给的 tag + 详情 JSON 走同一条结论", () => {
    const withDetails = resultFromTag("1.7.5", "v1.7.6", release);
    expect(withDetails.newer).toBe(true);
    expect(withDetails.detailed).toBe(true);
    expect(withDetails.release.notes).toBe("- 修复了某处");
    expect(withDetails.installer?.name).toBe("PadLend-1.7.6-setup.exe");

    // 没有详情：仍然能判断版本，并按约定推算安装器
    const bare = resultFromTag("1.7.5", "v1.7.6");
    expect(bare.newer).toBe(true);
    expect(bare.detailed).toBe(false);
    expect(bare.installer?.downloadUrl).toContain("/releases/download/v1.7.6/");

    // 详情里的 tag 缺失时用外壳给的 tag 补上
    const partial = resultFromTag("1.7.5", "v1.7.6", { name: "x" });
    expect(partial.release.tag).toBe("v1.7.6");
  });

  it("findLatestTag 直接可用", async () => {
    const tag = await findLatestTag(vi.fn(async () => redirect("v2.0.0")) as never);
    expect(tag).toBe("v2.0.0");
  });
});
