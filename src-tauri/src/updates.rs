//! 检查更新：用 WinHTTP 读 GitHub 的发布信息。
//!
//! 为什么不在前端 fetch：
//! - github.com 的网页端点没有 CORS 头，WebView2 里 fetch 会被浏览器拦掉（只有 api.github.com 带 CORS）；
//! - api.github.com 未认证时每个出口 IP 每小时 60 次，实测经常直接 403 rate limit；
//! - WinHTTP 走 Windows 的系统代理（注册表 / PAC），正好覆盖需要代理的机器。
//!
//! 做法：只请求 `/releases/latest` 并要求**不要跟随重定向**，读 `Location` 头就能拿到 tag；
//! 再把 api.github.com 的结果当「详情增强」，失败也不影响版本判断。

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption,
    WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPTION_REDIRECT_POLICY,
    WINHTTP_OPTION_REDIRECT_POLICY_NEVER, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION,
    WINHTTP_QUERY_STATUS_CODE,
};

const REPO: &str = "measureer/PadLend";
const AGENT: &str = concat!("PadLend/", env!("CARGO_PKG_VERSION"));

/// 句柄薄包装：离开作用域就关，出错提前返回也不会泄漏。
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 发一个 GET，返回 (状态码, Location 头, 响应体)。`follow` 为 false 时不跟随重定向。
fn http_get(host: &str, path: &str, follow: bool) -> Result<(u32, Option<String>, String), String> {
    let agent = wide(AGENT);
    let host_w = wide(host);
    let verb = wide("GET");
    let path_w = wide(path);
    unsafe {
        let session = Handle(WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err("WinHttpOpen 失败（无法初始化 HTTP 客户端）".into());
        }
        // 网络卡住时别让「检查更新」转一辈子：解析 5s、连接 5s、收发各 10s。
        let _ = WinHttpSetTimeouts(session.0, 5000, 5000, 10000, 10000);
        let connect = Handle(WinHttpConnect(
            session.0,
            PCWSTR(host_w.as_ptr()),
            443,
            0,
        ));
        if connect.0.is_null() {
            return Err(format!("WinHttpConnect 失败：{host}"));
        }
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err("WinHttpOpenRequest 失败".into());
        }
        if !follow {
            let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_le_bytes();
            WinHttpSetOption(
                Some(request.0 as *const c_void),
                WINHTTP_OPTION_REDIRECT_POLICY,
                Some(&policy),
            )
            .map_err(|e| format!("WinHttpSetOption 失败：{e}"))?;
        }
        WinHttpSendRequest(request.0, None, None, 0, 0, 0)
            .map_err(|e| format!("WinHttpSendRequest 失败：{e}"))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|e| format!("WinHttpReceiveResponse 失败：{e}"))?;

        let mut status: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|e| format!("读取状态码失败：{e}"))?;

        let mut location = None;
        if (300..400).contains(&status) {
            let mut buffer = [0u16; 2048];
            let mut len = (buffer.len() * 2) as u32;
            if WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_LOCATION,
                PCWSTR::null(),
                Some(buffer.as_mut_ptr() as *mut c_void),
                &mut len,
                std::ptr::null_mut(),
            )
            .is_ok()
            {
                let chars = (len as usize) / 2;
                let end = buffer.iter().position(|c| *c == 0).unwrap_or(chars);
                location = Some(String::from_utf16_lossy(&buffer[..end]));
            }
        }

        let mut body = String::new();
        if status == 200 {
            let mut chunk = [0u8; 8192];
            loop {
                let mut read: u32 = 0;
                WinHttpReadData(
                    request.0,
                    chunk.as_mut_ptr() as *mut c_void,
                    chunk.len() as u32,
                    &mut read,
                )
                .map_err(|e| format!("读取响应失败：{e}"))?;
                if read == 0 {
                    break;
                }
                body.push_str(&String::from_utf8_lossy(&chunk[..read as usize]));
                if body.len() > 512 * 1024 {
                    break;
                }
            }
        }

        Ok((status, location, body))
    }
}

/// `/releases/tag/v1.7.6` → `v1.7.6`。
pub fn tag_from_url(url: &str) -> Option<String> {
    let rest = url.split("/releases/tag/").nth(1)?;
    let tag = rest.split(['?', '#', '/']).next().unwrap_or(rest);
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

/// 最新发布的 tag。优先读网页重定向（不耗 API 配额），失败再退回 REST API。
pub fn latest_tag() -> Result<String, String> {
    let path = format!("/{REPO}/releases/latest");
    match http_get("github.com", &path, false) {
        Ok((_, Some(location), _)) => {
            return tag_from_url(&location)
                .ok_or_else(|| format!("重定向地址里没有发布标签：{location}"));
        }
        Ok((404, _, _)) => return Err("仓库还没有发布任何 Release".into()),
        Ok((200, _, _)) => return Err("网页端点未按预期重定向（HTTP 200），请改用 REST API 或手动查看 Releases".into()),
        Ok((status, _, _)) => {
            crate::shell_log!("[update] 网页端点返回 HTTP {status}，改用 REST API");
        }
        Err(err) => crate::shell_log!("[update] 网页重定向不可用：{err}"),
    }

    let api_path = format!("/repos/{REPO}/releases/latest");
    let (status, _, body) = http_get("api.github.com", &api_path, true)?;
    if status == 404 {
        return Err("仓库还没有发布任何 Release".into());
    }
    if status == 403 || status == 429 {
        return Err("GitHub 接口限流，请稍后再试".into());
    }
    if status != 200 {
        return Err(format!("GitHub 返回 HTTP {status}"));
    }
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("解析 GitHub 响应失败：{e}"))?;
    value
        .get("tag_name")
        .and_then(|tag| tag.as_str())
        .map(|tag| tag.to_string())
        .ok_or_else(|| "GitHub 响应里没有 tag_name".to_string())
}

/// 发布详情（说明、资产列表）；拿不到就返回 None，不影响版本判断。
pub fn release_details() -> Option<serde_json::Value> {
    let api_path = format!("/repos/{REPO}/releases/latest");
    match http_get("api.github.com", &api_path, true) {
        Ok((200, _, body)) => serde_json::from_str(&body).ok(),
        Ok((status, _, _)) => {
            crate::shell_log!("[update] 发布详情不可用：HTTP {status}");
            None
        }
        Err(err) => {
            crate::shell_log!("[update] 发布详情不可用：{err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tag_from_url;

    /// 联网自检（手动跑）：cargo test --lib -- --ignored live_latest_tag
    #[test]
    #[ignore]
    fn live_latest_tag() {
        let tag = super::latest_tag().expect("应该能读到最新 tag");
        eprintln!("latest tag = {tag}");
        let details = super::release_details();
        eprintln!("details = {}", if details.is_some() { "ok" } else { "none" });
        assert!(!tag.is_empty());
    }

    #[test]
    fn extracts_tag_from_redirect_url() {
        assert_eq!(tag_from_url("https://github.com/a/b/releases/tag/v1.7.6").as_deref(), Some("v1.7.6"));
        assert_eq!(tag_from_url("https://github.com/a/b/releases/tag/v1.4.0?x=1").as_deref(), Some("v1.4.0"));
        assert!(tag_from_url("https://github.com/a/b/releases").is_none());
    }
}
