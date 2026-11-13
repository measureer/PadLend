//! 平板指针位置估计器（对应 tablet/cursor.py，DESIGN 6.4 / 8.2，冻结契约）。
//!
//! 平板是**相对设备**，且系统不暴露指针坐标（F5），所以位置只能由 PC 端镜像。
//! 换算链（6.4.1）：PC 物理像素位移 Δp --×gain--> HID 相对计数 c（整数，clamp ±127）
//! --×XScale--> 平板像素 Δx。
//!
//! 关键不变量（I3）：每次 feed() 之后立即夹紧，位置恒在 [0, W-1] × [0, H-1]。
//! 这与 Android PointerController 的夹紧行为对齐，否则估计器会越走越偏。
//!
//! 线程约定：只在 Router 线程访问，本类型内部不加锁。

use crate::logic::geometry::Edge;

/// 每帧 HID 计数的上限（与 constants::HID_CLAMP 一致）。
const HID_CLAMP: i32 = crate::constants::HID_CLAMP;

/// 尺寸非法的错误；对应 Python 的 ValueError。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EstimatorError {
    /// 宽或高 <= 0（Python：invalid tablet size {width}x{height}）。
    #[error("invalid tablet size {width}x{height}")]
    InvalidSize {
        /// 传入的宽。
        width: i32,
        /// 传入的高。
        height: i32,
    },
}

/// 平板指针位置的唯一真相源（6.4）。
#[derive(Debug, Clone, PartialEq)]
pub struct CursorEstimator {
    /// 平板逻辑宽（浮点保存，与 Python 的 self.W 一致）。
    w: f64,
    /// 平板逻辑高。
    h: f64,
    /// 一个 HID 计数对应的平板像素（系统 XScale）。
    k: f64,
    x: f64,
    y: f64,
    /// 小数残差，防慢速拖动丢步（D12：不丢老数据）。
    resid: [f64; 2],
    /// 最近一次实际发出的 HID 计数（诊断用）。
    last_report: (i32, i32),
}

impl CursorEstimator {
    /// 构造估计器；尺寸非法时返回 [EstimatorError::InvalidSize]。
    ///
    /// xscale <= 0 退化为 1.0（与 Python 的 float(xscale) if xscale > 0 else 1.0 一致）。
    pub fn new(width: i32, height: i32, xscale: f64) -> Result<Self, EstimatorError> {
        if width <= 0 || height <= 0 {
            return Err(EstimatorError::InvalidSize { width, height });
        }
        Ok(Self {
            w: f64::from(width),
            h: f64::from(height),
            k: if xscale > 0.0 { xscale } else { 1.0 },
            // 6.4.2：初始在 (0,0)；真实入口位置由 Router 用 warp_to 摆放。
            x: 0.0,
            y: 0.0,
            resid: [0.0, 0.0],
            last_report: (0, 0),
        })
    }

    /// 当前位置（平板逻辑像素）。
    pub fn position(&self) -> (f64, f64) {
        (self.x, self.y)
    }

    /// 平板宽（整数截断，与 Python int(self.W) 一致）。
    pub fn width(&self) -> i32 {
        self.w as i32
    }

    /// 平板高。
    pub fn height(&self) -> i32 {
        self.h as i32
    }

    /// 当前 XScale（测试与诊断用；Python 的 self.k）。
    pub fn xscale(&self) -> f64 {
        self.k
    }

    /// 小数残差（测试用；Python 的 self.resid）。
    pub fn residual(&self) -> (f64, f64) {
        (self.resid[0], self.resid[1])
    }

    /// 最近一次发出的 HID 计数。
    pub fn last_report(&self) -> (i32, i32) {
        self.last_report
    }

    /// 重置位置。缺省归中（平板旋转/重启后指针位置未知，6.4.3）。
    pub fn reset(&mut self, x: Option<f64>, y: Option<f64>) {
        let x = x.unwrap_or((self.w - 1.0) / 2.0);
        let y = y.unwrap_or((self.h - 1.0) / 2.0);
        self.x = x;
        self.y = y;
        self.resid = [0.0, 0.0];
        self.clamp();
    }

    /// 与系统行为对齐：PointerController 也会夹紧，我们不夹就会越走越偏。
    pub fn clamp(&mut self) {
        self.x = self.x.clamp(0.0, self.w - 1.0);
        self.y = self.y.clamp(0.0, self.h - 1.0);
    }

    /// PC 物理像素位移 → 一帧 HID 相对计数（±clamp）。
    ///
    /// 残差保留在 resid 中，慢速细碎移动不会丢步。
    /// clamp 先夹到 [1, HID_CLAMP]（Python：max(1, min(int(clamp), C.HID_CLAMP))）。
    pub fn feed(&mut self, dpx: f64, dpy: f64, gain: f64, clamp: i32) -> (i32, i32) {
        let clamp = clamp.clamp(1, HID_CLAMP);
        self.resid[0] += dpx * gain;
        self.resid[1] += dpy * gain;

        // Python 的 int() 向零截断，与 Rust 的 as i32 语义一致。
        let mut cx = self.resid[0] as i32;
        let mut cy = self.resid[1] as i32;
        self.resid[0] -= f64::from(cx);
        self.resid[1] -= f64::from(cy);

        cx = cx.clamp(-clamp, clamp);
        cy = cy.clamp(-clamp, clamp);

        self.x += f64::from(cx) * self.k;
        self.y += f64::from(cy) * self.k;
        self.clamp();
        self.last_report = (cx, cy);
        (cx, cy)
    }

    /// 默认 clamp（对应 Python 的默认参数 clamp=C.HID_CLAMP）。
    pub fn feed_default(&mut self, dpx: f64, dpy: f64, gain: f64) -> (i32, i32) {
        self.feed(dpx, dpy, gain, HID_CLAMP)
    }

    /// 估计指针是否已抵达平板边界；多个边界同时命中时按 6.4.2 的优先级。
    pub fn hits(&self) -> Option<Edge> {
        if self.x <= 0.0 {
            return Some(Edge::Left);
        }
        if self.x >= self.w - 1.0 {
            return Some(Edge::Right);
        }
        if self.y <= 0.0 {
            return Some(Edge::Top);
        }
        if self.y >= self.h - 1.0 {
            return Some(Edge::Bottom);
        }
        None
    }

    /// 设计文档 6.5.3 用 hit()，契约 8.2 用 hits()；两个名字都保留。
    pub fn hit(&self) -> Option<Edge> {
        self.hits()
    }

    /// 相对设备做"绝对定位"：穿屏时把平板指针搬到入口点（6.4.2）。
    ///
    /// 返回需要一次性 flush 的 HID 报告列表（26 个报文 ≈ 3ms）。
    pub fn warp_to(&mut self, tx: f64, ty: f64, max_reports: usize) -> Vec<(i32, i32)> {
        let mut out: Vec<(i32, i32)> = Vec::new();
        let tx = tx.clamp(0.0, self.w - 1.0);
        let ty = ty.clamp(0.0, self.h - 1.0);
        for _ in 0..max_reports {
            let dx = tx - self.x;
            let dy = ty - self.y;
            if dx.abs() < self.k && dy.abs() < self.k {
                break;
            }
            let cx = ((dx / self.k) as i32).clamp(-HID_CLAMP, HID_CLAMP);
            let cy = ((dy / self.k) as i32).clamp(-HID_CLAMP, HID_CLAMP);
            if cx == 0 && cy == 0 {
                // k > 1 时残余小于 k 的整数步长无法表达；交给 clamp 收敛。
                break;
            }
            self.x += f64::from(cx) * self.k;
            self.y += f64::from(cy) * self.k;
            out.push((cx, cy));
        }
        self.clamp();
        if let Some(last) = out.last() {
            self.last_report = *last;
        }
        out
    }

    /// 默认 max_reports = 64。
    pub fn warp_to_default(&mut self, tx: f64, ty: f64) -> Vec<(i32, i32)> {
        self.warp_to(tx, ty, 64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants as C;

    // 逐条对应 tests/test_estimator.py

    #[test]
    fn initial_position_and_clamp() {
        let mut est = CursorEstimator::new(3200, 2136, 1.312).expect("构造");
        assert_eq!(est.position(), (0.0, 0.0));
        est.clamp();
        assert_eq!(est.position(), (0.0, 0.0));
        assert_eq!(est.width(), 3200);
        assert_eq!(est.height(), 2136);
    }

    #[test]
    fn feed_scales_by_xscale() {
        let mut est = CursorEstimator::new(3200, 2136, 2.0).expect("构造");
        assert_eq!(est.feed(10.0, 5.0, 1.0, C::HID_CLAMP), (10, 5));
        assert_eq!(est.position(), (20.0, 10.0));
    }

    #[test]
    fn gain_applies_before_integer_step() {
        let mut est = CursorEstimator::new(3200, 2136, 1.0).expect("构造");
        assert_eq!(est.feed(10.0, 0.0, 0.5, C::HID_CLAMP), (5, 0));
        assert_eq!(est.position(), (5.0, 0.0));
    }

    #[test]
    fn residual_preserves_subpixel_motion() {
        let mut est = CursorEstimator::new(3200, 2136, 1.0).expect("构造");
        let reports = [
            est.feed_default(0.4, 0.0, 1.0),
            est.feed_default(0.4, 0.0, 1.0),
            est.feed_default(0.4, 0.0, 1.0),
        ];
        // 0.4*3 = 1.2 -> 第三次才凑够 1 个计数，残差 0.2 保留
        assert_eq!(reports, [(0, 0), (0, 0), (1, 0)]);
        assert_eq!(est.position(), (1.0, 0.0));
        assert!((est.residual().0 - 0.2).abs() < 1e-9);
    }

    #[test]
    fn hid_clamp_is_applied_per_frame() {
        let mut est = CursorEstimator::new(3200, 2136, 1.0).expect("构造");
        let (cx, cy) = est.feed_default(10_000.0, -10_000.0, 1.0);
        assert_eq!(cx, C::HID_CLAMP);
        assert_eq!(cy, -C::HID_CLAMP);
        // 位置随后被 clamp 到屏幕内（Android PointerController 同样会夹紧）
        assert_eq!(est.position(), (127.0, 0.0));
    }

    #[test]
    fn position_is_always_clamped() {
        let mut est = CursorEstimator::new(100, 80, 2.0).expect("构造");
        for _ in 0..10 {
            est.feed_default(127.0, 127.0, 1.0);
        }
        assert!(est.position().0 <= 99.0 && est.position().1 <= 79.0);
        assert_eq!(est.hits(), Some(Edge::Right)); // 优先级 LEFT > RIGHT > TOP > BOTTOM

        for _ in 0..20 {
            est.feed_default(-127.0, -127.0, 1.0);
        }
        assert!(est.position().0 >= 0.0 && est.position().1 >= 0.0);
        assert_eq!(est.hits(), Some(Edge::Left));
    }

    #[test]
    fn hits_each_edge() {
        let mut est = CursorEstimator::new(100, 80, 1.0).expect("构造");
        est.x = 0.0;
        est.y = 40.0;
        assert_eq!(est.hits(), Some(Edge::Left));
        est.x = 99.0;
        est.y = 40.0;
        assert_eq!(est.hits(), Some(Edge::Right));
        est.x = 50.0;
        est.y = 0.0;
        assert_eq!(est.hits(), Some(Edge::Top));
        est.x = 50.0;
        est.y = 79.0;
        assert_eq!(est.hits(), Some(Edge::Bottom));
        est.x = 50.0;
        est.y = 40.0;
        assert_eq!(est.hits(), None);
        // 6.4.2 的 hit() 别名与 hits() 必须完全一致
        assert_eq!(est.hit(), est.hits());
    }

    #[test]
    fn reset_default_centers_and_clears_residual() {
        let mut est = CursorEstimator::new(3200, 2136, 1.312).expect("构造");
        est.feed_default(50.0, 50.0, 1.0);
        est.reset(None, None);
        assert_eq!(est.position(), ((3200.0 - 1.0) / 2.0, (2136.0 - 1.0) / 2.0));
        assert_eq!(est.residual(), (0.0, 0.0));
    }

    #[test]
    fn warp_to_moves_and_returns_reports() {
        let mut est = CursorEstimator::new(3200, 2136, 1.312).expect("构造");
        est.reset(Some(1600.0), Some(1068.0));
        let reports = est.warp_to_default(0.0, 0.0);
        assert!(!reports.is_empty(), "远距离 warp 必须产生报文");
        assert!(reports.len() <= 64);
        // 到达精度受整数计数 × k 限制
        let (x, y) = est.position();
        assert!(x.abs() <= est.k);
        assert!(y.abs() <= est.k);
        assert!((0.0..=3199.0).contains(&x));
        assert!((0.0..=2135.0).contains(&y));
        for (cx, cy) in &reports {
            assert!((-C::HID_CLAMP..=C::HID_CLAMP).contains(cx));
            assert!((-C::HID_CLAMP..=C::HID_CLAMP).contains(cy));
        }
    }

    #[test]
    fn warp_to_same_position_is_noop() {
        let mut est = CursorEstimator::new(1000, 1000, 1.0).expect("构造");
        est.reset(Some(10.0), Some(10.0));
        assert!(est.warp_to_default(10.0, 10.0).is_empty());
        assert_eq!(est.position(), (10.0, 10.0));
    }

    #[test]
    fn warp_to_clamps_target() {
        let mut est = CursorEstimator::new(100, 100, 1.0).expect("构造");
        est.warp_to_default(500.0, -500.0);
        let (x, y) = est.position();
        assert!((0.0..=99.0).contains(&x));
        assert!((0.0..=99.0).contains(&y));
    }

    #[test]
    fn invalid_size_is_rejected_like_python_value_error() {
        let err = CursorEstimator::new(0, 100, 1.0).expect_err("宽 0 必须拒绝");
        assert_eq!(
            err,
            EstimatorError::InvalidSize {
                width: 0,
                height: 100
            }
        );
        assert_eq!(err.to_string(), "invalid tablet size 0x100");
        assert!(CursorEstimator::new(10, -1, 1.0).is_err());
    }

    #[test]
    fn non_positive_xscale_falls_back_to_one() {
        let est = CursorEstimator::new(100, 100, 0.0).expect("构造");
        assert_eq!(est.xscale(), 1.0);
        let est = CursorEstimator::new(100, 100, -2.0).expect("构造");
        assert_eq!(est.xscale(), 1.0);
    }
}
