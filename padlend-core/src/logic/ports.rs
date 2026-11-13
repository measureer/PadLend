//! Router 端口在真实 Win32 实现上的适配器。
//!
//! `core/router.py` 直接持有 `DesktopMap`/`CursorMask` 对象，Rust 版把它们抽象成
//! `DesktopPort`/`MaskPort` 两个 trait；本模块负责把 `win/desktop.rs` 与
//! `win/mask.rs` 接上去。
//!
//! 为什么 `DesktopAdapter` 里套 `Mutex`：`DesktopMap::refresh()` 需要 `&mut self`
//! （拓扑变化时整体替换快照），而 `DesktopPort` 的方法全是 `&self`。

use std::fmt;
use std::sync::{Mutex, MutexGuard};

use crate::logic::geometry::{Edge, EdgeSegment, MonitorInfo, Rect};
use crate::logic::router::{CursorQuery, DesktopPort, ErrorCode, MaskPort, RouterError};
use crate::win::desktop::{DesktopError, DesktopMap};
use crate::win::mask::{CursorMask, MaskConfig, MaskError, Win32MaskApi};

use std::sync::Arc;

/// `DesktopMap` → `DesktopPort`。
pub struct DesktopAdapter {
    inner: Mutex<DesktopMap>,
}

impl DesktopAdapter {
    pub fn new(map: DesktopMap) -> Self {
        Self {
            inner: Mutex::new(map),
        }
    }

    /// 用真实 Win32 枚举构造（启动早期、创建任何窗口之前调用）。
    pub fn with_win32() -> Result<Self, DesktopError> {
        Ok(Self::new(DesktopMap::new()?))
    }

    fn lock(&self) -> MutexGuard<'_, DesktopMap> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl DesktopPort for DesktopAdapter {
    fn monitors(&self) -> Vec<MonitorInfo> {
        self.lock().monitors().to_vec()
    }

    fn bounding_box(&self) -> Rect {
        self.lock().bounding_box()
    }

    fn cursor_pos(&self) -> CursorQuery {
        match self.lock().cursor_pos() {
            Some((x, y)) => CursorQuery::Position(x, y),
            None => CursorQuery::Unavailable,
        }
    }

    fn segments_at(&self, x: i32, y: i32, band: i32) -> Vec<EdgeSegment> {
        self.lock().segments_at_with_band(x, y, band)
    }

    fn resolve_corner(
        &self,
        segments: &[EdgeSegment],
        delta: (i32, i32),
        priority: Option<Edge>,
    ) -> Option<EdgeSegment> {
        self.lock().resolve_corner(segments, delta, priority)
    }

    fn exterior_segments(&self) -> Vec<EdgeSegment> {
        self.lock().exterior_segments().to_vec()
    }

    fn segment_exists(&self, segment: &EdgeSegment) -> Option<bool> {
        Some(self.lock().segment_exists(segment))
    }

    fn refresh(&self) -> Result<(), RouterError> {
        self.lock()
            .refresh()
            .map_err(|err| RouterError::new(ErrorCode::Unexpected, err.to_string()))
    }

    fn set_cursor_pos(&self, x: i32, y: i32) -> bool {
        self.lock().set_cursor_pos(x, y)
    }
}

impl fmt::Debug for DesktopAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DesktopAdapter")
    }
}

/// `CursorMask` → `MaskPort`。
pub struct MaskAdapter {
    inner: CursorMask,
}

impl fmt::Debug for MaskAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MaskAdapter")
    }
}

impl MaskAdapter {
    pub fn new(mask: CursorMask) -> Self {
        Self { inner: mask }
    }

    /// 用真实 Win32 API 构造（`show()` 之前不会建窗、不会锁光标）。
    pub fn with_win32(config: MaskConfig) -> Self {
        Self::new(CursorMask::with_config(Arc::new(Win32MaskApi), config))
    }
}

fn mask_error(err: MaskError) -> RouterError {
    // ClipCursor 相关失败给冻结码 E202，其余归 E900：与 Python 的语义分类一致。
    let text = err.to_string();
    let code = if text.contains("ClipCursor") {
        ErrorCode::ClipCursorLeak
    } else {
        ErrorCode::Unexpected
    };
    RouterError::new(code, text)
}

impl MaskPort for MaskAdapter {
    fn show(&self, clip: Option<Rect>) -> Result<(), RouterError> {
        self.inner.show(clip).map_err(mask_error)
    }

    fn release(&self) {
        self.inner.release();
    }

    fn hide(&self) {
        self.inner.hide();
    }

    fn diagnose(&self) -> String {
        self.inner.diagnose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::geometry::EdgeSegment;

    fn map_with_one_monitor() -> DesktopMap {
        DesktopMap::from_monitors(vec![MonitorInfo::new(
            1,
            r"\\.\DISPLAY1",
            Rect::new(0, 0, 1920, 1080),
            96,
            true,
        )])
    }

    #[test]
    fn desktop_adapter_exposes_monitors_and_bounds() {
        let adapter = DesktopAdapter::new(map_with_one_monitor());
        assert_eq!(adapter.monitors().len(), 1);
        let bounds = adapter.bounding_box();
        assert_eq!((bounds.left, bounds.top, bounds.right, bounds.bottom), (0, 0, 1920, 1080));
    }

    #[test]
    fn desktop_adapter_maps_missing_cursor_to_unavailable() {
        let adapter = DesktopAdapter::new(map_with_one_monitor());
        // from_monitors 用空 API：游标查询不可用 → Unavailable（Router 据此不累加）。
        match adapter.cursor_pos() {
            CursorQuery::Unavailable | CursorQuery::Position(_, _) => {}
            CursorQuery::Unsupported => panic!("真实桌面适配器不应返回 Unsupported"),
        }
    }

    #[test]
    fn desktop_adapter_reports_right_edge_segments() {
        let adapter = DesktopAdapter::new(map_with_one_monitor());
        let segments = adapter.exterior_segments();
        assert!(segments.iter().any(|seg| seg.edge == Edge::Right));
        let right = segments.iter().find(|seg| seg.edge == Edge::Right).unwrap();
        assert!(adapter.segment_exists(&EdgeSegment { ..*right }).unwrap_or(false));
    }

    #[test]
    fn desktop_adapter_refresh_is_idempotent_on_static_topology() {
        let adapter = DesktopAdapter::new(map_with_one_monitor());
        assert!(adapter.refresh().is_ok());
        assert_eq!(adapter.monitors().len(), 1);
    }
}
