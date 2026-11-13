//! --simulate 用的内存假平板（对应 tablet/simulated.py）。
//!
//! 目的只有一个：在没有真机、没有桌面会话的环境里，把 Router 的输入链路从
//! Raw Input 到 HID 报告整条跑通并断言。因此这里不做任何 IO：
//!
//! * SimulatedSession 记录收到的事件（报告/按键/文本/剪贴板/光标），并实现
//!   logic/router 的 SessionPort；
//! * SimulatedAdb 实现 AdbApi，让 session/服务端代码在演示模式下也能构造。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::logic::router::{
    DeviceFacts, HealthReport, RouterError, SessionPort, CursorQuery,
};
use crate::tablet::adb::{AdbApi, AdbError, DeviceEntry};

/// 假平板的默认宽（像素）。
pub const DEFAULT_WIDTH: i32 = 1707;
/// 假平板的默认高（像素）。
pub const DEFAULT_HEIGHT: i32 = 1067;
/// 假设备的 serial。
pub const SIM_SERIAL: &str = "sim:5555";

/// 一条被记录的鼠标报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimReport {
    /// 按钮位。
    pub buttons: i32,
    /// 水平位移。
    pub dx: i32,
    /// 垂直位移。
    pub dy: i32,
    /// 滚轮。
    pub wheel: i32,
}

/// 被记录的键盘报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimKeyReport {
    /// 修饰键位。
    pub modifier: u8,
    /// 按下的 usage 列表。
    pub usages: Vec<u8>,
}

/// 内存假平板（对应 Python SimulatedSession）。
pub struct SimulatedSession {
    serial: String,
    model: String,
    width: i32,
    height: i32,
    orientation: i32,
    xscale: f64,
    pointer_speed: i32,
    started: Mutex<bool>,
    started_at: Mutex<Option<Instant>>,
    facts: Mutex<Option<DeviceFacts>>,
    reports: Mutex<Vec<SimReport>>,
    keys: Mutex<Vec<SimKeyReport>>,
    texts: Mutex<Vec<String>>,
    clipboards: Mutex<Vec<String>>,
    cursor: Mutex<(i32, i32)>,
    pointer_speed_calls: Mutex<usize>,
    clipboard_callback: Mutex<Option<Arc<dyn Fn(String) + Send + Sync>>>,
    facts_callback: Mutex<Option<Arc<dyn Fn(DeviceFacts) + Send + Sync>>>,
    acked: Mutex<HashMap<u64, usize>>,
}

impl std::fmt::Debug for SimulatedSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SimulatedSession")
            .field("serial", &self.serial)
            .field("started", &self.is_started())
            .field("reports", &self.sent())
            .finish()
    }
}

impl Default for SimulatedSession {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulatedSession {
    /// 默认尺寸 1707x1067、xscale 2.0、pointer_speed -3。
    pub fn new() -> Self {
        Self {
            serial: SIM_SERIAL.to_string(),
            model: "Simulated Tablet".to_string(),
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            orientation: 0,
            xscale: 2.0,
            pointer_speed: -3,
            started: Mutex::new(false),
            started_at: Mutex::new(None),
            facts: Mutex::new(None),
            reports: Mutex::new(Vec::new()),
            keys: Mutex::new(Vec::new()),
            texts: Mutex::new(Vec::new()),
            clipboards: Mutex::new(Vec::new()),
            cursor: Mutex::new((0, 0)),
            pointer_speed_calls: Mutex::new(0),
            clipboard_callback: Mutex::new(None),
            facts_callback: Mutex::new(None),
            acked: Mutex::new(HashMap::new()),
        }
    }

    /// 自定义尺寸（布局测试用）。
    pub fn with_size(width: i32, height: i32) -> Self {
        let mut session = Self::new();
        session.width = width;
        session.height = height;
        session
    }

    fn build_facts(&self) -> DeviceFacts {
        DeviceFacts {
            serial: self.serial.clone(),
            model: self.model.clone(),
            width: self.width,
            height: self.height,
            orientation: self.orientation,
            xscale: self.xscale,
            pointer_speed: self.pointer_speed,
            hardware_serial: self.serial.clone(),
        }
    }

    fn is_started(&self) -> bool {
        self.started.lock().map(|guard| *guard).unwrap_or(false)
    }

    /// 记录下来的鼠标报告。
    pub fn reports(&self) -> Vec<SimReport> {
        self.reports
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 记录下来的键盘报告。
    pub fn keys(&self) -> Vec<SimKeyReport> {
        self.keys.lock().map(|guard| guard.clone()).unwrap_or_default()
    }

    /// 记录下来的注入文本。
    pub fn texts(&self) -> Vec<String> {
        self.texts.lock().map(|guard| guard.clone()).unwrap_or_default()
    }

    /// 记录下来的剪贴板写入。
    pub fn clipboards(&self) -> Vec<String> {
        self.clipboards
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 当前光标位置（累加 dx/dy）。
    pub fn cursor(&self) -> (i32, i32) {
        self.cursor.lock().map(|guard| *guard).unwrap_or((0, 0))
    }

    /// ensure_pointer_speed 被调用次数。
    pub fn pointer_speed_calls(&self) -> usize {
        self.pointer_speed_calls
            .lock()
            .map(|guard| *guard)
            .unwrap_or(0)
    }

    /// 按下沿次数（previous == 0 且 bit0 置位）。
    pub fn clicks(&self) -> usize {
        let reports = self.reports();
        let mut count = 0;
        let mut previous = 0;
        for report in reports {
            if previous == 0 && report.buttons & 0x01 != 0 {
                count += 1;
            }
            previous = report.buttons;
        }
        count
    }

    /// 已发送报文数。
    pub fn sent(&self) -> usize {
        self.reports.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    /// 清空所有记录。
    pub fn clear(&self) {
        if let Ok(mut guard) = self.reports.lock() {
            guard.clear();
        }
        if let Ok(mut guard) = self.keys.lock() {
            guard.clear();
        }
        if let Ok(mut guard) = self.texts.lock() {
            guard.clear();
        }
        if let Ok(mut guard) = self.clipboards.lock() {
            guard.clear();
        }
        if let Ok(mut guard) = self.cursor.lock() {
            *guard = (0, 0);
        }
    }

    /// 模拟平板侧发布剪贴板（触发回调）。
    pub fn publish_clipboard(&self, text: &str) {
        if let Ok(mut guard) = self.clipboards.lock() {
            guard.push(text.to_string());
        }
        let callback = self
            .clipboard_callback
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        if let Some(callback) = callback {
            callback(text.to_string());
        }
    }

    /// 平板侧对某条 SET_CLIPBOARD 回 ACK（仅记录）。
    pub fn ack(&self, sequence: u64) {
        if let Ok(mut guard) = self.acked.lock() {
            *guard.entry(sequence).or_insert(0) += 1;
        }
    }

    /// 某条 SET_CLIPBOARD 的 ACK 次数。
    pub fn ack_count(&self, sequence: u64) -> usize {
        self.acked
            .lock()
            .ok()
            .and_then(|guard| guard.get(&sequence).copied())
            .unwrap_or(0)
    }
}

impl SessionPort for SimulatedSession {
    fn send_mouse(&self, buttons: i32, dx: i32, dy: i32, wheel: i32) -> Result<(), RouterError> {
        if !self.is_started() {
            // 与 Python 一致：未 start 时静默丢弃。
            return Ok(());
        }
        if let Ok(mut guard) = self.reports.lock() {
            guard.push(SimReport {
                buttons,
                dx,
                dy,
                wheel,
            });
        }
        if let Ok(mut cursor) = self.cursor.lock() {
            cursor.0 = cursor.0.saturating_add(dx);
            cursor.1 = cursor.1.saturating_add(dy);
        }
        Ok(())
    }

    fn send_key(&self, modifier: u8, usages: &[u8]) -> Result<(), RouterError> {
        if !self.is_started() {
            return Ok(());
        }
        if let Ok(mut guard) = self.keys.lock() {
            guard.push(SimKeyReport {
                modifier,
                usages: usages.to_vec(),
            });
        }
        Ok(())
    }

    fn facts(&self) -> Option<DeviceFacts> {
        if let Ok(guard) = self.facts.lock() {
            if let Some(facts) = guard.as_ref() {
                return Some(facts.clone());
            }
        }
        Some(self.build_facts())
    }

    fn health(&self) -> Result<Option<HealthReport>, RouterError> {
        let started = self.is_started();
        let uptime = self
            .started_at
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .map(|instant| instant.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        Ok(Some(HealthReport {
            alive: started,
            socket_ok: started,
            uhid_created: started,
            server_alive: started,
            consecutive_failures: 0,
            last_error: String::new(),
            uptime_s: uptime,
            sent: self.sent() as i64,
        }))
    }

    fn set_clipboard(&self, text: &str) -> Result<(), RouterError> {
        if text.is_empty() {
            return Ok(());
        }
        if let Ok(mut guard) = self.clipboards.lock() {
            guard.push(text.to_string());
        }
        Ok(())
    }

    fn stop(&self) {
        let was_started = self.is_started();
        if let Ok(mut guard) = self.started.lock() {
            *guard = false;
        }
        if was_started {
            eprintln!("[tablet.simulated] simulated.session.stopped");
        }
    }

    fn start(&self) -> Result<DeviceFacts, RouterError> {
        let mut guard = match self.started.lock() {
            Ok(guard) => guard,
            Err(_) => {
                return Err(RouterError::unexpected("simulated 状态锁损坏"));
            }
        };
        if !*guard {
            *guard = true;
            if let Ok(mut slot) = self.facts.lock() {
                *slot = Some(self.build_facts());
            }
            if let Ok(mut at) = self.started_at.lock() {
                *at = Some(Instant::now());
            }
            eprintln!(
                "[tablet.simulated] simulated.session.started serial={}",
                SIM_SERIAL
            );
        }
        let facts = self.build_facts();
        drop(guard);
        Ok(facts)
    }

    fn reconnect(&self) -> Option<Result<Option<DeviceFacts>, RouterError>> {
        Some(self.start().map(Some))
    }

    fn ensure_pointer_speed(&self) -> Option<Result<(), RouterError>> {
        if let Ok(mut guard) = self.pointer_speed_calls.lock() {
            *guard += 1;
        }
        Some(Ok(()))
    }

    fn set_pointer_speed(&self, speed: i32) -> Option<Result<(), RouterError>> {
        Some(match self.started.lock().map(|guard| *guard).unwrap_or(false) {
            true => {
                let _ = speed;
                Ok(())
            }
            false => Err(RouterError::device_offline("模拟会话未启动")),
        })
    }

    fn set_cursor_pos(&self, x: i32, y: i32) -> bool {
        if let Ok(mut guard) = self.cursor.lock() {
            *guard = (x, y);
        }
        true
    }

    fn cancel_start(&self) -> bool {
        true
    }

    fn send_keycode(&self, vk: i32) -> Result<(), RouterError> {
        let usage = vk.clamp(0, 255) as u8;
        self.send_key(0, &[usage])
    }

    fn inject_text(&self, text: &str) -> Result<(), RouterError> {
        if text.is_empty() {
            return Ok(());
        }
        if let Ok(mut guard) = self.texts.lock() {
            guard.push(text.to_string());
        }
        Ok(())
    }

    fn started(&self) -> bool {
        self.is_started()
    }

    fn set_facts_callback(
        &self,
        callback: Option<Arc<dyn Fn(DeviceFacts) + Send + Sync>>,
    ) -> bool {
        match self.facts_callback.lock() {
            Ok(mut guard) => {
                *guard = callback;
                true
            }
            Err(_) => false,
        }
    }

    fn set_clipboard_callback(
        &self,
        callback: Option<Arc<dyn Fn(String) + Send + Sync>>,
    ) -> bool {
        match self.clipboard_callback.lock() {
            Ok(mut guard) => {
                *guard = callback;
                true
            }
            Err(_) => false,
        }
    }
}

impl SimulatedSession {
    /// 当前光标查询（三个状态都用得到，方便 Router 单测）。
    pub fn cursor_query(&self) -> CursorQuery {
        let (x, y) = self.cursor();
        CursorQuery::Position(x, y)
    }
}

/// 内存假 adb（对应 Python SimulatedAdb）。
#[derive(Debug, Clone)]
pub struct SimulatedAdb {
    serial: String,
}

impl Default for SimulatedAdb {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulatedAdb {
    /// 默认绑定 SIM_SERIAL。
    pub fn new() -> Self {
        Self {
            serial: SIM_SERIAL.to_string(),
        }
    }


    fn device_entry(&self) -> DeviceEntry {
        let mut entry = DeviceEntry::new(self.serial.clone(), "device");
        entry.product = "sim".to_string();
        entry.model = "Simulated Tablet".to_string();
        entry.device = "sim".to_string();
        entry.transport_id = "1".to_string();
        entry
    }
}

impl AdbApi for SimulatedAdb {
    fn serial(&self) -> String {
        self.serial.clone()
    }

    fn with_serial(&self, serial: &str) -> Arc<dyn AdbApi> {
        Arc::new(SimulatedAdb {
            serial: serial.to_string(),
        })
    }

    fn version(&self) -> Result<String, AdbError> {
        Ok("simulated".to_string())
    }

    fn devices(&self) -> Result<Vec<DeviceEntry>, AdbError> {
        Ok(vec![self.device_entry()])
    }

    fn select_device(
        &self,
        _serial: Option<&str>,
        _prefer_usb: bool,
    ) -> Result<DeviceEntry, AdbError> {
        Ok(self.device_entry())
    }

    fn wait_for_device(
        &self,
        _timeout: f64,
        _cancel: &(dyn Fn() -> bool + Sync),
    ) -> Result<DeviceEntry, AdbError> {
        Ok(self.device_entry())
    }

    fn connect(&self, address: &str, _timeout: f64) -> Result<String, AdbError> {
        Ok(format!("connected to {address}"))
    }

    fn pair(&self, address: &str, _code: &str, _timeout: f64) -> Result<String, AdbError> {
        Ok(format!("Successfully paired to {address}"))
    }

    fn disconnect(&self, _address: Option<&str>, _timeout: f64) -> String {
        String::new()
    }

    fn kill_server(&self) -> bool {
        false
    }

    fn mdns_services(&self) -> Vec<String> {
        Vec::new()
    }

    fn wifi_mac(&self, _serial: Option<&str>) -> String {
        String::new()
    }

    fn shell(&self, args: &[&str], _timeout: f64) -> Result<String, AdbError> {
        let joined = args.join(" ");
        match joined.as_str() {
            "getprop ro.product.model" => Ok("Simulated Tablet".to_string()),
            "getprop ro.serialno" => Ok(SIM_SERIAL.to_string()),
            "wm size" => Ok(format!(
                "Physical size: {}x{}",
                DEFAULT_WIDTH, DEFAULT_HEIGHT
            )),
            "dumpsys input" => Ok(format!(
                "  Viewport INTERNAL: displayId=0, orientation=0\n  Cursor Input Mapper:\n    XScale: 2.0\n"
            )),
            "settings get system pointer_speed" => Ok("-3".to_string()),
            "settings get system screen_off_timeout" => Ok("60000".to_string()),
            _ => Ok(String::new()),
        }
    }

    fn shell_ok(&self, _args: &[&str], _timeout: f64) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_is_idempotent_and_returns_facts() {
        let session = SimulatedSession::new();
        assert!(!SessionPort::started(&session));
        let facts = SessionPort::start(&session).expect("应当启动");
        assert_eq!(facts.serial, SIM_SERIAL);
        assert_eq!(facts.model, "Simulated Tablet");
        assert_eq!(facts.width, DEFAULT_WIDTH);
        assert_eq!(facts.height, DEFAULT_HEIGHT);
        assert_eq!(facts.orientation, 0);
        assert!((facts.xscale - 2.0).abs() < 1e-9);
        assert_eq!(facts.pointer_speed, -3);
        SessionPort::send_mouse(&session, 0, 1, 0, 0).expect("应当记录");
        let again = SessionPort::start(&session).expect("应当仍然启动");
        assert_eq!(again.serial, SIM_SERIAL);
        assert_eq!(session.reports().len(), 1);
    }

    #[test]
    fn mouse_reports_are_recorded_and_cursor_moves() {
        let session = SimulatedSession::new();
        SessionPort::start(&session).expect("应当启动");
        SessionPort::send_mouse(&session, 1, 5, -3, 0).expect("应当记录");
        SessionPort::send_mouse(&session, 0, 2, 2, 120).expect("应当记录");
        let reports = session.reports();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].buttons, 1);
        assert_eq!(reports[0].wheel, 0);
        assert_eq!(reports[1].wheel, 120);
        assert_eq!(session.cursor(), (7, -1));
        assert_eq!(session.clicks(), 1);
        assert_eq!(session.sent(), 2);
    }

    #[test]
    fn mouse_before_start_is_dropped() {
        let session = SimulatedSession::new();
        SessionPort::send_mouse(&session, 1, 10, 10, 0).expect("不应报错");
        assert!(session.reports().is_empty());
        assert_eq!(session.cursor(), (0, 0));
    }

    #[test]
    fn keys_text_and_clipboard_are_recorded() {
        let session = SimulatedSession::new();
        SessionPort::start(&session).expect("应当启动");
        SessionPort::send_key(&session, 2, &[4, 5]).expect("应当记录");
        SessionPort::send_keycode(&session, 66).expect("应当记录");
        SessionPort::inject_text(&session, "中文").expect("应当记录");
        SessionPort::inject_text(&session, "").expect("空串忽略");
        SessionPort::set_clipboard(&session, "hello").expect("应当记录");
        SessionPort::set_clipboard(&session, "").expect("空串忽略");
        let keys = session.keys();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].modifier, 2);
        assert_eq!(keys[0].usages, vec![4, 5]);
        assert_eq!(keys[1].usages, vec![66]);
        assert_eq!(session.texts(), vec!["中文".to_string()]);
        assert_eq!(session.clipboards(), vec!["hello".to_string()]);
    }

    #[test]
    fn health_pointer_speed_and_clear() {
        let session = SimulatedSession::new();
        let before = SessionPort::health(&session)
            .expect("应当有健康快照")
            .expect("应当有");
        assert!(!before.alive);
        SessionPort::start(&session).expect("应当启动");
        SessionPort::send_mouse(&session, 0, 1, 1, 0).expect("应当记录");
        let health = SessionPort::health(&session)
            .expect("应当有健康快照")
            .expect("应当有");
        assert!(health.alive && health.socket_ok && health.uhid_created && health.server_alive);
        assert_eq!(health.sent, 1);
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.is_healthy());
        assert!(SessionPort::ensure_pointer_speed(&session)
            .expect("应当支持")
            .is_ok());
        assert_eq!(session.pointer_speed_calls(), 1);
        session.clear();
        assert!(session.reports().is_empty());
        assert_eq!(session.cursor(), (0, 0));
        SessionPort::stop(&session);
        assert!(!SessionPort::started(&session));
    }

    #[test]
    fn clipboard_callback_receives_remote_text() {
        let session = SimulatedSession::new();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        assert!(SessionPort::set_clipboard_callback(
            &session,
            Some(Arc::new(move |text: String| {
                if let Ok(mut guard) = sink.lock() {
                    guard.push(text);
                }
            })),
        ));
        session.publish_clipboard("来自平板");
        let values = seen.lock().map(|guard| guard.clone()).unwrap_or_default();
        assert_eq!(values, vec!["来自平板".to_string()]);
    }

    #[test]
    fn simulated_adb_exposes_a_single_device() {
        let adb = SimulatedAdb::new();
        assert_eq!(AdbApi::version(&adb).expect("应当有版本"), "simulated");
        let devices = AdbApi::devices(&adb).expect("应当有设备");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].serial, SIM_SERIAL);
        assert!(devices[0].online());
        let bound = AdbApi::with_serial(&adb, "10.0.0.2:5555");
        assert_eq!(bound.serial(), "10.0.0.2:5555");
        assert_eq!(
            AdbApi::connect(&adb, "10.0.0.2:5555", 4.0).expect("应当成功"),
            "connected to 10.0.0.2:5555"
        );
        assert_eq!(AdbApi::wifi_mac(&adb, None), "");
        let size = AdbApi::shell(&adb, &["wm", "size"], 5.0).expect("应当有输出");
        assert!(size.contains("Physical size"));
    }
}

