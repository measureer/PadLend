//! 业务引擎组装（对应 Python `service/api.py` 的 `PadLendService` 生命周期）。
//!
//! 职责：
//! * 持有配置、设备注册表、日志环形缓冲、事实镜像、摆位缓存；
//! * 组装 Router（Session + Desktop + Mask）并注入回调；
//! * `--simulate` 时用 `SimulatedSession` + `SimulatedAdb` + 无操作遮罩，**不碰 adb**；
//! * 给 `service::api` 提供 25 个方法所需的最小操作面。
//!
//! 与 Python 的偏差：时间戳是 UTC（无 chrono）、simulate 下遮罩换成 `NoopMask`
//! （不建窗、不锁光标）。

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::{Config, PlacementCfg};
use crate::constants as C;
use crate::logic::geometry::{Align, Edge, EdgeSegment, MonitorInfo, Placement, Rect};
use crate::logic::ports::{DesktopAdapter, MaskAdapter};
use crate::logic::router::{DesktopPort, DeviceFacts, MaskPort, Router, SessionPort};
use crate::tablet::adb::{Adb, AdbApi, AdbErrorCode, DeviceEntry};
use crate::tablet::locator;
use crate::tablet::session::{SessionBuilder, TabletSession};
use crate::tablet::simulated::{SimulatedAdb, SimulatedSession};
use crate::win::desktop::DesktopMap;
use crate::win::mask::MaskConfig;

use super::{clockfmt, diagnostics, registry, serializers};

/// 事件发射器：`(event, data)` → 传输层（生产实现写 stdout）。
pub type EmitFn = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// 日志环形缓冲上限（Python `deque(maxlen=1000)`）。
pub const LOG_CAPACITY: usize = 1000;
/// `snapshot()` 携带的日志条数（Python `[-200:]`）。
pub const SNAPSHOT_LOGS: usize = 200;

/// 一条日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub timestamp: String,
    pub event: String,
    pub message: String,
}

impl LogEntry {
    /// `{"timestamp","event","message"}`。
    pub fn to_value(&self) -> Value {
        json!({
            "timestamp": self.timestamp,
            "event": self.event,
            "message": self.message,
        })
    }
}

/// 引擎构造选项（`Engine::new` 的测试友好版本）。
#[derive(Default)]
pub struct EngineOptions {
    pub simulate: bool,
    pub no_hooks: bool,
    pub config_path: Option<PathBuf>,
    pub emit: Option<EmitFn>,
    pub start_router: Option<bool>,
}

type FactsKey = (String, i32, i32, i32, f64);

/// `--simulate` 用的无操作遮罩：不建窗、不锁光标，只记录调用次数。
#[derive(Debug, Default)]
struct NoopMask {
    released: Mutex<usize>,
}

impl NoopMask {
    fn new() -> Self {
        Self::default()
    }
}

impl MaskPort for NoopMask {
    fn show(&self, _clip: Option<Rect>) -> Result<(), crate::logic::router::RouterError> {
        Ok(())
    }

    fn release(&self) {
        if let Ok(mut count) = self.released.lock() {
            *count += 1;
        }
    }

    fn diagnose(&self) -> String {
        "noop".to_string()
    }
}

/// 取锁（被毒化时继续用内部数据）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 业务引擎。
/// 只启动一次 Raw Input（进程内单例）。
static RAW_INPUT_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub struct Engine {
    simulate: bool,
    no_hooks: bool,
    config_path: PathBuf,
    registry_path: PathBuf,
    first_run: bool,
    config: Mutex<Config>,
    registry: Mutex<registry::Registry>,
    desktop: Arc<DesktopAdapter>,
    mask: Arc<dyn MaskPort>,
    adb: Option<Arc<dyn AdbApi>>,
    system_adb: Option<Arc<Adb>>,
    session: OnceLock<Arc<dyn SessionPort>>,
    router: OnceLock<Arc<Router>>,
    logs: Mutex<VecDeque<LogEntry>>,
    facts: Mutex<Option<DeviceFacts>>,
    facts_key: Mutex<Option<FactsKey>>,
    placement: Mutex<Option<Placement>>,
    layout_key: Mutex<Option<(String, String)>>,
    located: Mutex<BTreeMap<String, Value>>,
    devices: Mutex<Vec<Value>>,
    report: Mutex<Option<diagnostics::DiagnosticsReport>>,
    metrics: Mutex<Value>,
    state: Mutex<String>,
    emit_fn: EmitFn,
    stop: Arc<AtomicBool>,
    watcher: Mutex<Option<JoinHandle<()>>>,
    started: AtomicBool,
    /// 首次 devices.connect 前不镜像事实（保持 bootstrap 的 facts=null）。
    facts_ready: AtomicBool,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Engine")
            .field("simulate", &self.simulate)
            .field("config_path", &self.config_path)
            .finish()
    }
}

impl Engine {
    /// 生产入口：默认配置路径、stdout 事件、启动 Router。
    pub fn new(simulate: bool, no_hooks: bool) -> Result<Arc<Engine>, String> {
        Engine::open(EngineOptions {
            simulate,
            no_hooks,
            ..EngineOptions::default()
        })
    }

    /// 通用入口（测试注入 emitter / 关闭 Router 线程）。
    pub fn open(options: EngineOptions) -> Result<Arc<Engine>, String> {
        let config_path = match options.config_path.clone() {
            Some(path) => path,
            None => C::config_path()
                .map_err(|err| crate::service::ServiceError::ConfigPath(err.to_string()))?,
        };
        let first_run = !config_path.exists();
        let cfg = Config::load(&config_path);
        if first_run {
            let _ = cfg.save(&config_path);
        }
        let registry_path = match config_path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.join("devices.json"),
            _ => PathBuf::from("devices.json"),
        };
        let mut registry = registry::Registry::load(&registry_path);
        if registry.devices.is_empty() {
            let macs: Vec<(String, String)> = cfg
                .device
                .macs
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect();
            if !cfg.device.known.is_empty() || !macs.is_empty() {
                let _ = registry.import_legacy(&cfg.device.serial, &cfg.device.known, &macs);
                let _ = registry.save();
            }
        } else if registry.needs_rebuild {
            let _ = registry.save();
        }

        let desktop = match DesktopAdapter::with_win32() {
            Ok(adapter) => adapter,
            Err(err) => {
                eprintln!("[service] 枚举显示器失败，退化为空桌面：{err}");
                DesktopAdapter::new(DesktopMap::from_monitors(Vec::new()))
            }
        };
        let mask: Arc<dyn MaskPort> = if options.simulate {
            Arc::new(NoopMask::new())
        } else {
            Arc::new(MaskAdapter::with_win32(MaskConfig::default()))
        };
        let system_adb: Option<Arc<Adb>> = if options.simulate {
            None
        } else {
            Some(Arc::new(Adb::new()))
        };
        let adb: Option<Arc<dyn AdbApi>> = if options.simulate {
            Some(Arc::new(SimulatedAdb::new()))
        } else {
            match system_adb.as_ref() {
                Some(adb) => {
                    let api: Arc<dyn AdbApi> = adb.clone();
                    Some(api)
                }
                None => None,
            }
        };

        let emit_fn: EmitFn = match options.emit {
            Some(emit) => emit,
            None => Arc::new(|event: &str, data: Value| {
                crate::rpc::write_event(event, &data);
            }),
        };

        let engine = Arc::new(Engine {
            simulate: options.simulate,
            no_hooks: options.no_hooks,
            config_path,
            registry_path,
            first_run,
            config: Mutex::new(cfg.clone()),
            registry: Mutex::new(registry),
            desktop: Arc::new(desktop),
            mask,
            adb,
            system_adb,
            session: OnceLock::new(),
            router: OnceLock::new(),
            logs: Mutex::new(VecDeque::new()),
            facts: Mutex::new(None),
            facts_key: Mutex::new(None),
            placement: Mutex::new(None),
            layout_key: Mutex::new(None),
            located: Mutex::new(BTreeMap::new()),
            devices: Mutex::new(Vec::new()),
            report: Mutex::new(None),
            metrics: Mutex::new(json!({})),
            state: Mutex::new("idle".to_string()),
            emit_fn,
            stop: Arc::new(AtomicBool::new(false)),
            watcher: Mutex::new(None),
            started: AtomicBool::new(false),
            facts_ready: AtomicBool::new(false),
        });

        engine.init_router(&cfg, engine.adb.clone())?;

        let initial = engine.placement_from_config(&cfg.placement, false);
        if let Some(placement) = &initial {
            if let Some(router) = engine.router.get() {
                router.set_placement(placement.clone());
            }
        }
        *lock(&engine.placement) = initial;
        if let Some(router) = engine.router.get() {
            *lock(&engine.state) = router.state().as_str().to_string();
        }
        *lock(&engine.metrics) = engine.current_metrics_value();

        if options.start_router.unwrap_or(true) {
            if let Some(router) = engine.router.get() {
                router.start();
            }
            engine.spawn_watcher();
        }
        engine.emit_log(
            "APP",
            format!("{} {} 业务服务已就绪", C::APP_NAME, C::VERSION),
        );
        engine.started.store(true, Ordering::SeqCst);
        Ok(engine)
    }

    /// 构造会话与 Router，并挂上回调（回调捕 Weak，避免 Engine↔Router 成环）。
    fn init_router(
        self: &Arc<Self>,
        cfg: &Config,
        adb: Option<Arc<dyn AdbApi>>,
    ) -> Result<(), crate::service::ServiceError> {
        let session: Arc<dyn SessionPort> = if self.simulate {
            Arc::new(SimulatedSession::new())
        } else {
            let Some(adb) = adb else {
                return Err(crate::service::ServiceError::Init(
                    "缺少 adb 接口，无法构造会话".to_string(),
                ));
            };
            let weak = Arc::downgrade(self);
            let weak_candidates = weak.clone();
            let candidates: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
                Arc::new(move || match weak_candidates.upgrade() {
                    Some(engine) => engine.device_candidates(),
                    None => Vec::new(),
                });
            let verify: Arc<dyn Fn(&str, &DeviceFacts) -> bool + Send + Sync> =
                Arc::new(move |serial, facts| match weak.upgrade() {
                    Some(engine) => engine.verify_device_facts(serial, facts),
                    None => true,
                });
            let built: Arc<TabletSession> = SessionBuilder::from_config(adb, cfg)
                .candidates(candidates)
                .verify_facts(verify)
                .build();
            built
        };
        let _ = self.session.set(session.clone());
        // 真实 CPU/内存采样：UI 的指标条要显示真实占用，不能恒 0。
        let router = Arc::new(
            Router::new(cfg.clone(), session, self.desktop.clone(), self.mask.clone())
                .with_process_usage(Box::new(crate::win::process::Win32ProcessUsage::new())),
        );
        self.install_callbacks(&router);
        self.router
            .set(router)
            .map_err(|_| crate::service::ServiceError::Init("Router 已经初始化过".to_string()))?;

        // 服务模式必须自己启动 Raw Input。Python 的 Runtime 会起它，Rust 版最初漏了这一步：
        // 接管能成功（入口脉冲靠会话直接发），但 PC 侧鼠标位移永远进不到 Router，
        // 表现就是"平板指针出现在入口边、然后一动不动"。
        if RAW_INPUT_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(router) = self.router().cloned() {
            let desktop = Arc::clone(&self.desktop);
            let buttons = Arc::new(std::sync::Mutex::new(0i32));
            let last_abs = Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::<u64, (i32, i32)>::new(),
            ));
            let handler: Arc<dyn Fn(crate::decode::MousePacket) + Send + Sync> =
                Arc::new(move |packet| {
                    let mut state = buttons.lock().unwrap_or_else(|err| err.into_inner());
                    for bit in [0x0001u16, 0x0004, 0x0010, 0x0040, 0x0100] {
                        if packet.button_flags & bit != 0 {
                            if let Some(mask) = crate::constants::raw_button_flag(bit) {
                                *state |= i32::from(mask);
                            }
                        }
                    }
                    for bit in [0x0002u16, 0x0008, 0x0020, 0x0080, 0x0200] {
                        if packet.button_flags & bit != 0 {
                            if let Some(mask) = crate::constants::raw_button_flag(bit) {
                                *state &= !i32::from(mask);
                            }
                        }
                    }
                    let (dx, dy) = if packet.is_absolute() {
                        let rect = desktop.bounding_box();
                        let width = i64::from(rect.right - rect.left).max(1);
                        let height = i64::from(rect.bottom - rect.top).max(1);
                        let x = rect.left + (i64::from(packet.x) * width / 65_535) as i32;
                        let y = rect.top + (i64::from(packet.y) * height / 65_535) as i32;
                        let mut last = last_abs.lock().unwrap_or_else(|err| err.into_inner());
                        match last.insert(packet.device, (x, y)) {
                            Some((px, py)) => (x - px, y - py),
                            None => (0, 0),
                        }
                    } else {
                        (packet.x, packet.y)
                    };
                    drop(state);
                    let wheel = if packet.has_wheel() {
                        i32::from(packet.button_data as i16)
                    } else {
                        0
                    };
                    if dx == 0 && dy == 0 && wheel == 0 && !packet.has_button_change() {
                        return;
                    }
                    let delta = crate::logic::events::RawDelta {
                        dx,
                        dy,
                        buttons: *buttons.lock().unwrap_or_else(|err| err.into_inner()),
                        wheel,
                        absolute: packet.is_absolute(),
                        ts: 0.0,
                    };
                    router.on_raw(&delta);
                });
            match crate::win::rawinput::RawInputSource::start_with(handler) {
                // 源必须活到进程结束：忘掉句柄比"drop 掉立刻停止采集"更符合这里的需求。
                Ok(source) => {
                    eprintln!("[engine] Raw Input 已启动（服务模式）");
                    std::mem::forget(source)
                }
                Err(err) => eprintln!("[engine] Raw Input 启动失败：{err}"),
            }
        }
        Ok(())
    }

    fn install_callbacks(self: &Arc<Self>, router: &Arc<Router>) {
        let weak = Arc::downgrade(self);
        let state_weak = weak.clone();
        router.set_on_state_changed(move |old, new, reason| {
            if let Some(engine) = state_weak.upgrade() {
                engine.on_state_changed(old, new, reason);
            }
        });
        let metrics_weak = weak.clone();
        router.set_on_metrics(move |snapshot| {
            if let Some(engine) = metrics_weak.upgrade() {
                engine.on_metrics(snapshot);
            }
        });
        let orientation_weak = weak.clone();
        router.set_on_orientation_changed(move |facts| {
            if let Some(engine) = orientation_weak.upgrade() {
                engine.on_orientation(facts);
            }
        });
        router.set_on_clipboard_received(move |text| {
            if let Some(engine) = weak.upgrade() {
                engine.emit("clipboard", json!({ "text": text }));
            }
        });
    }

    fn on_state_changed(&self, old: crate::logic::state::State, new: crate::logic::state::State, reason: &str) {
        *lock(&self.state) = new.as_str().to_string();
        self.emit(
            "state",
            json!({ "old": old.as_str(), "new": new.as_str(), "reason": reason }),
        );
        let old_text = if old.as_str().is_empty() { "—" } else { old.as_str() };
        let new_text = if new.as_str().is_empty() { "—" } else { new.as_str() };
        self.emit_log(
            "STATE",
            format!("{old_text} → {new_text}（{reason}）"),
        );
    }

    fn on_metrics(&self, snapshot: &crate::logic::metrics::Snapshot) {
        let value = snapshot.to_value().unwrap_or(Value::Null);
        *lock(&self.metrics) = value.clone();
        self.emit("metrics", value);
    }

    fn on_orientation(&self, facts: &DeviceFacts) {
        self.emit("orientation", serializers::facts_dict(Some(facts)));
        self.emit_log(
            "ORIENT",
            format!("屏幕方向：{}x{}（orientation={}）", facts.width, facts.height, facts.orientation),
        );
    }

    fn spawn_watcher(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let stop = self.stop.clone();
        let spawned = thread::Builder::new()
            .name("padlend-facts".to_string())
            .spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(500));
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Some(engine) = weak.upgrade() {
                        engine.poll_facts_once();
                    }
                }
            });
        match spawned {
            Ok(handle) => {
                *lock(&self.watcher) = Some(handle);
            }
            Err(err) => eprintln!("[service] 无法启动事实轮询线程：{err}"),
        }
    }

    /// 轮询一次 `router.facts()`，变化才更新镜像并广播（Python `_facts_loop`）。
    pub fn poll_facts_once(&self) {
        if !self.facts_ready.load(Ordering::SeqCst) {
            return;
        }
        let Some(router) = self.router.get() else {
            return;
        };
        let facts = router.facts();
        let key = facts.as_ref().map(|facts| {
            (
                facts.serial.clone(),
                facts.width,
                facts.height,
                facts.orientation,
                facts.xscale,
            )
        });
        {
            let mut known = lock(&self.facts_key);
            if *known == key {
                return;
            }
            *known = key;
        }
        *lock(&self.facts) = facts.clone();
        self.emit(
            "facts",
            serializers::facts_dict(facts.as_ref()),
        );
        match facts {
            Some(facts) => {
                self.remember_device(&facts.serial, "", Some(&facts));
                self.apply_layout_for_facts(&facts);
                self.emit_log(
                    "DEVICE",
                    format!("设备事实：{} {}×{}", facts.serial, facts.width, facts.height),
                );
            }
            None => {
                *lock(&self.layout_key) = None;
            }
        }
    }

    /// 同步一次事实（连接成功后立刻调用，保证紧随其后的 `engine.state` 能看到事实）。
    pub fn sync_facts_now(&self) {
        self.poll_facts_once();
    }

    /// 允许事实轮询（首次 devices.connect 时开启；此前 bootstrap 的 facts 保持 null）。
    pub fn enable_facts(&self) {
        self.facts_ready.store(true, Ordering::SeqCst);
    }

    /// 记录当前生效的布局键（layout.save 用；身份/方向为空时清空）。
    pub fn set_layout_key(&self, identity: &str, bucket: &str) {
        if identity.is_empty() || bucket.is_empty() {
            *lock(&self.layout_key) = None;
        } else {
            *lock(&self.layout_key) = Some((identity.to_lowercase(), bucket.to_string()));
        }
    }

    // -- 只读视图 ----------------------------------------------------------

    pub fn simulate(&self) -> bool {
        self.simulate
    }

    pub fn no_hooks(&self) -> bool {
        self.no_hooks
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn registry_path(&self) -> &Path {
        &self.registry_path
    }

    pub fn first_run(&self) -> bool {
        self.first_run
    }

    pub fn started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    pub fn router(&self) -> Option<&Arc<Router>> {
        self.router.get()
    }

    pub fn adb_api(&self) -> Option<Arc<dyn AdbApi>> {
        self.adb.clone()
    }

    pub fn system_adb(&self) -> Option<Arc<Adb>> {
        self.system_adb.clone()
    }

    /// 配置快照（克隆）。
    pub fn config_clone(&self) -> Config {
        lock(&self.config).clone()
    }

    /// 在锁内修改配置。
    pub fn with_config<R>(&self, apply: impl FnOnce(&mut Config) -> R) -> R {
        let mut guard = lock(&self.config);
        apply(&mut guard)
    }

    pub fn replace_config(&self, config: Config) {
        *lock(&self.config) = config;
    }

    pub fn issues(&self) -> Vec<String> {
        lock(&self.config).issues().to_vec()
    }

    pub fn config_dict(&self) -> Result<Value, String> {
        let config = self.config_clone();
        serializers::config_dict(&config)
    }

    pub fn save_config(&self) -> Result<(), crate::service::ServiceError> {
        let config = self.config_clone();
        config
            .save(&self.config_path)
            .map_err(|err| crate::service::ServiceError::ConfigSave(err.to_string()))
    }

    pub fn state(&self) -> String {
        lock(&self.state).clone()
    }

    pub fn facts(&self) -> Option<DeviceFacts> {
        lock(&self.facts).clone()
    }

    pub fn placement(&self) -> Option<Placement> {
        lock(&self.placement).clone()
    }

    pub fn manual(&self) -> bool {
        self.router.get().map(|router| router.manual()).unwrap_or(false)
    }

    /// 实时指标（Python `dict(router.metrics_snapshot())`）。
    pub fn current_metrics_value(&self) -> Value {
        match self.router.get() {
            Some(router) => router
                .metrics_snapshot()
                .to_value()
                .unwrap_or_else(|_| Value::Null),
            None => lock(&self.metrics).clone(),
        }
    }

    /// 最后一次推送的指标（`engine.state` 用）。
    pub fn metrics_value(&self) -> Value {
        lock(&self.metrics).clone()
    }

    pub fn mask_release(&self) {
        self.mask.release();
    }

    // -- 事件与日志 --------------------------------------------------------

    pub fn emit(&self, event: &str, data: Value) {
        (self.emit_fn)(event, data);
    }

    /// 追加一条日志并广播 `log` 事件（Python `emit_log`）。
    pub fn emit_log(&self, event: &str, message: impl Into<String>) {
        let entry = LogEntry {
            timestamp: clockfmt::time_of_day(),
            event: event.to_string(),
            message: message.into(),
        };
        {
            let mut logs = lock(&self.logs);
            if logs.len() >= LOG_CAPACITY {
                logs.pop_front();
            }
            logs.push_back(entry.clone());
        }
        self.emit("log", entry.to_value());
    }

    /// 最近 `limit` 条日志（`logs.recent`）。
    pub fn logs_recent(&self, limit: usize) -> Vec<Value> {
        let logs = lock(&self.logs);
        let limit = limit.clamp(1, LOG_CAPACITY);
        let start = logs.len().saturating_sub(limit);
        logs.iter().skip(start).map(LogEntry::to_value).collect()
    }

    /// `snapshot()` 携带的最后 200 条日志。
    pub fn snapshot_logs(&self) -> Vec<Value> {
        let logs = lock(&self.logs);
        let start = logs.len().saturating_sub(SNAPSHOT_LOGS);
        logs.iter().skip(start).map(LogEntry::to_value).collect()
    }

    /// 日志条数（测试用）。
    pub fn log_count(&self) -> usize {
        lock(&self.logs).len()
    }

    // -- 桌面 / 摆位 -------------------------------------------------------

    pub fn monitors(&self) -> Vec<MonitorInfo> {
        self.desktop.monitors()
    }

    pub fn exterior_segments(&self) -> Vec<EdgeSegment> {
        self.desktop.exterior_segments()
    }

    pub fn bounding_box(&self) -> Rect {
        self.desktop.bounding_box()
    }

    pub fn refresh_desktop(&self) {
        let _ = self.desktop.refresh();
    }

    /// 某个显示器句柄对应的设备名（`\\.\DISPLAY1` 之类的原始名）。
    pub fn monitor_device(&self, handle: i64) -> String {
        self.desktop
            .monitors()
            .into_iter()
            .find(|monitor| monitor.handle == handle)
            .map(|monitor| monitor.device)
            .unwrap_or_default()
    }

    /// 从配置段计算摆位（Python `_placement_from_config`）。
    pub fn placement_from_config(&self, source: &PlacementCfg, strict_monitor: bool) -> Option<Placement> {
        let segments = self.desktop.exterior_segments();
        if segments.is_empty() {
            return None;
        }
        let edge = Edge::try_from(source.edge.as_str()).unwrap_or(Edge::Right);
        let monitor = source.monitor.trim().to_lowercase();
        let mut candidates: Vec<&EdgeSegment> = segments
            .iter()
            .filter(|segment| segment.edge == edge)
            .collect();
        if !monitor.is_empty() {
            let exact: Vec<&EdgeSegment> = candidates
                .iter()
                .copied()
                .filter(|segment| self.monitor_device(segment.monitor_handle).to_lowercase() == monitor)
                .collect();
            if !exact.is_empty() {
                candidates = exact;
            } else if strict_monitor {
                return None;
            }
        }
        if candidates.is_empty() {
            if strict_monitor && !source.edge.trim().is_empty() {
                return None;
            }
            candidates = segments.iter().collect();
        }
        let chosen = candidates
            .into_iter()
            .max_by_key(|segment| segment.length())?
            .clone();
        let align = Align::try_from(source.align.as_str()).unwrap_or(Align::Proportional);
        let corner_priority = if source.corner_priority.trim().is_empty() {
            None
        } else {
            Edge::try_from(source.corner_priority.as_str()).ok()
        };
        Some(Placement {
            segment: chosen,
            offset: source.offset.clamp(0.0, 1.0),
            align,
            corner_priority,
        })
    }

    /// 把摆位写回配置段（Python `_apply_placement_to_config`）。
    pub fn apply_placement_to_config(&self, placement: &Placement, target: &mut PlacementCfg) {
        target.edge = placement.segment.edge.as_str().to_string();
        target.span_start = i64::from(placement.segment.span_start);
        target.span_end = i64::from(placement.segment.span_end);
        let device = self.monitor_device(placement.segment.monitor_handle);
        if !device.is_empty() {
            target.monitor = device;
        }
        target.offset = placement.offset;
        target.align = placement.align.as_str().to_string();
        target.corner_priority = placement
            .corner_priority
            .map(|edge| edge.as_str().to_string())
            .unwrap_or_default();
    }

    pub fn set_placement_value(&self, placement: Placement) {
        if let Some(router) = self.router.get() {
            router.set_placement(placement.clone());
        }
        *lock(&self.placement) = Some(placement);
    }

    /// 设备身份：硬件序列号优先，否则 serial。
    pub fn layout_identity(&self, facts: Option<&DeviceFacts>) -> String {
        let owned = lock(&self.facts).clone();
        let facts = facts.or(owned.as_ref());
        match facts {
            Some(facts) => {
                let hardware = facts.hardware_serial.trim();
                if hardware.is_empty() {
                    facts.serial.trim().to_string()
                } else {
                    hardware.to_string()
                }
            }
            None => String::new(),
        }
    }

    /// 屏幕方向桶：宽高优先，其次 orientation 位。
    pub fn orientation_bucket(&self, facts: Option<&DeviceFacts>) -> String {
        let owned = lock(&self.facts).clone();
        let facts = facts.or(owned.as_ref());
        let Some(facts) = facts else {
            return String::new();
        };
        if facts.width > 0 && facts.height > 0 {
            if facts.width >= facts.height {
                "landscape".to_string()
            } else {
                "portrait".to_string()
            }
        } else if facts.orientation & 1 == 1 {
            "portrait".to_string()
        } else {
            "landscape".to_string()
        }
    }

    /// 事实变化时切换每设备布局（Python `_apply_layout_for_facts`）。
    pub fn apply_layout_for_facts(&self, facts: &DeviceFacts) {
        let identity = self.layout_identity(Some(facts));
        let bucket = self.orientation_bucket(Some(facts));
        if identity.is_empty() || bucket.is_empty() {
            return;
        }
        let key = (identity.to_lowercase(), bucket.clone());
        if lock(&self.layout_key).as_ref() == Some(&key) {
            return;
        }
        let previous = lock(&self.layout_key).clone();
        let profile = {
            let config = lock(&self.config);
            config.layouts.get(&identity, &bucket).cloned()
        };
        if let Some(profile) = profile {
            let Some(placement) = self.placement_from_config(&profile, true) else {
                self.emit_log(
                    "LAYOUT",
                    format!("设备 {identity} 的{bucket}布局引用的显示器不存在，已跳过"),
                );
                return;
            };
            *lock(&self.layout_key) = Some(key);
            self.publish_placement(placement, "profile");
            self.emit_log("LAYOUT", format!("已切换到该设备的{bucket}布局"));
            return;
        }
        *lock(&self.layout_key) = Some(key);
        if let Some(previous) = previous {
            if previous.0 == identity.to_lowercase() {
                return;
            }
        }
        let global = lock(&self.config).placement.clone();
        if let Some(placement) = self.placement_from_config(&global, false) {
            self.publish_placement(placement, "global");
        }
    }

    fn publish_placement(&self, placement: Placement, reason: &str) {
        self.set_placement_value(placement.clone());
        self.emit(
            "placement",
            json!({
                "placement": serializers::placement_dict(Some(&placement)),
                "dirty": false,
                "reason": reason,
            }),
        );
    }

    // -- 设备注册表 --------------------------------------------------------

    pub fn with_registry<R>(&self, apply: impl FnOnce(&mut registry::Registry) -> R) -> R {
        let mut guard = lock(&self.registry);
        apply(&mut guard)
    }

    pub fn save_registry(&self) -> bool {
        let mut guard = lock(&self.registry);
        guard.save()
    }

    pub fn registry_snapshot(&self) -> registry::Registry {
        lock(&self.registry).clone()
    }

    /// 设备连接候选地址（当前设备的主端点优先）。
    /// 记住"用户这次点的是哪台设备"。
    ///
    /// `Router` 里的 `Config` 是只读副本，`connect_device` 只更新它自己的
    /// `requested_serial`；会话的候选列表来自本函数的 `self.config`，
    /// 不同步就会出现"点 44581、会话还在连 44731"的情况。
    pub fn set_requested_serial(&self, serial: &str) {
        lock(&self.config).device.serial = serial.to_string();
    }

    pub fn device_candidates(&self) -> Vec<String> {
        let current = lock(&self.config).device.serial.trim().to_string();
        if current.is_empty() {
            return Vec::new();
        }
        let registry = lock(&self.registry);
        let Some(record) = registry.find_by_serial(&current) else {
            return vec![current];
        };
        let primary = record.primary_serial(&current);
        let mut out: Vec<String> = vec![primary.clone()];
        let mut rest: Vec<&registry::Endpoint> = record
            .endpoints
            .iter()
            .filter(|endpoint| !endpoint.matches(&primary))
            .collect();
        rest.sort_by(|left, right| {
            (right.last_ok.as_str(), right.last_seen.as_str())
                .cmp(&(left.last_ok.as_str(), left.last_seen.as_str()))
        });
        for endpoint in rest {
            out.push(endpoint.serial.clone());
        }
        out.truncate(registry::MAX_ENDPOINTS);
        out
    }

    /// 身份校验：硬件序列号不符就拒绝（Python `_verify_device_facts`）。
    pub fn verify_device_facts(&self, serial: &str, facts: &DeviceFacts) -> bool {
        let record = {
            let registry = lock(&self.registry);
            registry.find_by_serial(serial).cloned()
        };
        let Some(record) = record else {
            self.remember_device(serial, "", Some(facts));
            return true;
        };
        let expected = record.hardware_serial.trim().to_string();
        let actual = registry::canonical_hardware_serial(&facts.hardware_serial, &facts.serial);
        if !expected.is_empty() && !actual.is_empty() && !expected.eq_ignore_ascii_case(&actual) {
            let detail = format!(
                "硬件序列号不符：注册表为 {expected}，实际为 {actual}；该地址可能已被另一台设备占用"
            );
            {
                let mut registry = lock(&self.registry);
                registry.mark_failure(serial, &detail);
                let _ = registry.save();
            }
            self.emit_log("DEVICE", format!("{serial} 身份校验失败：{detail}"));
            return false;
        }
        if !expected.is_empty() && actual.is_empty() {
            self.emit_log(
                "DEVICE",
                format!("无法读取 {serial} 的硬件序列号，按记录端点接受连接"),
            );
        }
        self.remember_device(serial, "", Some(facts));
        true
    }

    /// 记住一次连接（Python `_remember_device`）：合并记录、迁移布局、写回配置。
    pub fn remember_device(&self, serial: &str, mac: &str, facts: Option<&DeviceFacts>) {
        let text = serial.trim().to_string();
        if text.is_empty() {
            return;
        }
        let (hardware, model) = match facts {
            Some(facts) => (facts.hardware_serial.clone(), facts.model.clone()),
            None => (String::new(), String::new()),
        };
        let before = {
            let registry = lock(&self.registry);
            registry.find_by_serial(&text).cloned()
        };
        let mut candidates: Vec<String> = vec![text.clone()];
        if let Some(record) = &before {
            if !record.hardware_serial.trim().is_empty() {
                candidates.push(record.hardware_serial.clone());
            }
            for endpoint in &record.endpoints {
                candidates.push(endpoint.serial.clone());
            }
        }
        let source = if facts.is_some() { "connect" } else { "" };
        let new_identity = {
            let mut registry = lock(&self.registry);
            let Some(id) = registry.upsert(&text, &hardware, &model, mac, "", source) else {
                return;
            };
            let identity = registry
                .find_by_id(&id)
                .map(|record| {
                    if record.hardware_serial.trim().is_empty() {
                        text.clone()
                    } else {
                        record.hardware_serial.clone()
                    }
                })
                .unwrap_or_else(|| text.clone());
            let _ = registry.save();
            identity
        };

        let mut layout_moved = false;
        let mut seen: Vec<String> = Vec::new();
        for old in candidates {
            let trimmed = old.trim().to_string();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(&new_identity) {
                continue;
            }
            if seen.iter().any(|item| item.eq_ignore_ascii_case(&trimmed)) {
                continue;
            }
            seen.push(trimmed.clone());
            if self.move_layout_identity(&trimmed, &new_identity) {
                layout_moved = true;
            }
        }

        let serial_changed = {
            let mut config = lock(&self.config);
            if config.device.serial == text {
                false
            } else {
                config.device.serial = text.clone();
                true
            }
        };
        if serial_changed || layout_moved {
            let _ = self.save_config();
            let value = self.config_dict().unwrap_or(Value::Null);
            self.emit(
                "config",
                json!({ "config": value, "issues": Value::Array(Vec::new()) }),
            );
        }
    }

    /// 布局键迁移（Python `_move_layout_identity`）。
    pub fn move_layout_identity(&self, old: &str, new: &str) -> bool {
        let old_key = crate::config::normalize_layout_key(old);
        let new_key = crate::config::normalize_layout_key(new);
        if old_key.is_empty() || new_key.is_empty() || old_key == new_key {
            return false;
        }
        let mut config = lock(&self.config);
        let profiles = match config.layouts.devices.remove(&old_key) {
            Some(profiles) => profiles,
            None => return false,
        };
        if let Some(target) = config.layouts.devices.entry_or_insert_default(new_key.clone()) {
            for (bucket, profile) in profiles.iter() {
                if !target.contains_key(bucket) {
                    target.insert(bucket.to_string(), profile.clone());
                }
            }
        }
        drop(config);
        let mut key = lock(&self.layout_key);
        if let Some((known, bucket)) = key.clone() {
            if known == old_key {
                *key = Some((new_key, bucket));
            }
        }
        true
    }

    /// 广播设备列表（Python `_publish_devices`）。
    pub fn publish_devices(&self, entries: &[DeviceEntry], error: &str) -> Vec<Value> {
        let current = lock(&self.config).device.serial.trim().to_string();
        let located = lock(&self.located).clone();
        let mut devices: Vec<Value> = Vec::new();
        let mut seen_serials: Vec<String> = Vec::new();
        let mut seen_ids: Vec<String> = Vec::new();

        for entry in entries {
            let serial = entry.serial.trim().to_string();
            if serial.is_empty() || seen_serials.iter().any(|item| item == &serial) {
                continue;
            }
            let record = {
                let registry = lock(&self.registry);
                registry.find_by_serial(&serial).cloned()
            };
            let mut dto = serializers::device_entry_dict(entry);
            let located_entry = record
                .as_ref()
                .and_then(|record| located_for(&located, record))
                .or_else(|| located.get(&serial).cloned());
            if let Some(entry) = &located_entry {
                dto["located"] = entry.get("status").cloned().unwrap_or_else(|| json!(""));
                dto["locatedDetail"] = entry.get("detail").cloned().unwrap_or_else(|| json!(""));
            }
            if let Some(record) = &record {
                if seen_ids.iter().any(|item| item == &record.id) {
                    continue;
                }
                dto["id"] = json!(record.id);
                dto["lastSeen"] = json!(record.last_seen);
                seen_ids.push(record.id.clone());
            }
            seen_serials.push(serial);
            devices.push(dto);
        }

        let known_records = {
            let registry = lock(&self.registry);
            registry.devices.clone()
        };
        for record in &known_records {
            let serial = record.primary_serial(&current);
            if serial.is_empty()
                || seen_serials.iter().any(|item| item == &serial)
                || seen_ids.iter().any(|item| item == &record.id)
            {
                continue;
            }
            let located_entry = located_for(&located, record)
                .or_else(|| located.get(&serial).cloned());
            let transport = match record.endpoint(&serial) {
                Some(endpoint) if endpoint.transport == "wifi" => "无线",
                _ => "USB",
            };
            let model = if record.model.trim().is_empty() {
                "上次连接的设备".to_string()
            } else {
                record.model.clone()
            };
            let located_status = located_entry
                .as_ref()
                .and_then(|entry| entry.get("status"))
                .cloned()
                .unwrap_or_else(|| json!("offline"));
            let located_detail = located_entry
                .as_ref()
                .and_then(|entry| entry.get("detail"))
                .cloned()
                .unwrap_or_else(|| json!("上次连接，当前未接入"));
            devices.push(json!({
                "serial": serial,
                "id": record.id,
                "state": "offline",
                "model": model,
                "transport": transport,
                "online": false,
                "remembered": true,
                "located": located_status,
                "locatedDetail": located_detail,
                "lastSeen": record.last_seen,
            }));
        }

        *lock(&self.devices) = devices.clone();
        self.emit(
            "devices",
            json!({ "devices": devices, "error": error }),
        );
        devices
    }

    pub fn devices_snapshot(&self) -> Vec<Value> {
        lock(&self.devices).clone()
    }

    pub fn located_snapshot(&self) -> BTreeMap<String, Value> {
        lock(&self.located).clone()
    }

    pub fn set_located(&self, serial: &str, located: Value) {
        lock(&self.located).insert(serial.to_string(), located);
    }

    pub fn remove_located(&self, serial: &str) {
        lock(&self.located).remove(serial);
    }

    pub fn report(&self) -> Option<diagnostics::DiagnosticsReport> {
        lock(&self.report).clone()
    }

    pub fn set_report(&self, report: diagnostics::DiagnosticsReport) {
        *lock(&self.report) = Some(report);
    }

    // -- adb 调用面 --------------------------------------------------------

    /// 跑一条 adb 命令；simulate 下返回固定假输出（Python `_run_adb`）。
    pub fn run_adb(&self, args: &[&str]) -> (i32, String, String) {
        if self.simulate {
            return (0, simulated_adb_output(args), String::new());
        }
        let Some(adb) = self.system_adb.as_ref() else {
            return (-1, String::new(), "adb 接口不可用".to_string());
        };
        match adb.run(args, 12.0, AdbErrorCode::Unexpected, false) {
            Ok(output) => (
                output.returncode,
                output.stdout_text(),
                output.stderr_text(),
            ),
            Err(err) => (-1, String::new(), err.detail),
        }
    }

    /// 一次扫描结果（`devices.scan` 用）。
    pub fn scan_devices(&self) -> (Vec<DeviceEntry>, String) {
        match self.adb.as_ref() {
            Some(adb) => match adb.devices() {
                Ok(entries) => (entries, String::new()),
                Err(err) => (Vec::new(), err.detail),
            },
            None => (Vec::new(), "adb 接口不可用".to_string()),
        }
    }

    /// 局域网查找（`devices.locate`）：只覆盖"无线端点 → 新地址"的主路径。
    pub fn locate_devices(&self) -> BTreeMap<String, Value> {
        if self.simulate {
            return self.located_snapshot();
        }
        let targets: Vec<(String, String, String)> = {
            let registry = lock(&self.registry);
            let mut targets: Vec<(String, String, String)> = Vec::new();
            for record in &registry.devices {
                for endpoint in &record.endpoints {
                    if endpoint.transport != "wifi" {
                        continue;
                    }
                    targets.push((
                        endpoint.serial.clone(),
                        record.macs.first().cloned().unwrap_or_default(),
                        record.hardware_serial.clone(),
                    ));
                    if targets.len() >= registry::MAX_ENDPOINTS {
                        break;
                    }
                }
            }
            targets
        };
        let Some(adb) = self.adb.as_ref() else {
            return self.located_snapshot();
        };
        let services = {
            let query = || adb.mdns_services();
            locator::discovered_mdns(Some(&query), 2.0)
        };
        let mut online = 0;
        for (serial, mac, hardware) in targets {
            let lookup = || services.clone();
            let arp = || locator::arp_table();
            let located = locator::locate(
                &serial,
                &mac,
                &hardware,
                None,
                Some(&lookup),
                Some(&arp),
                1.0,
            );
            if located.online() {
                online += 1;
            }
            if located.reachable() {
                if located.status == Some(locator::LocateStatus::Moved) && !located.address.is_empty()
                {
                    let mut registry = lock(&self.registry);
                    let _ = registry.rename_endpoint(&serial, &located.address, &located.mac, "locate");
                    let _ = registry.save();
                    let mut config = lock(&self.config);
                    if config.device.serial == serial {
                        config.device.serial = located.address.clone();
                        drop(config);
                        let _ = self.save_config();
                        let value = self.config_dict().unwrap_or(Value::Null);
                        self.emit(
                            "config",
                            json!({ "config": value, "issues": Value::Array(Vec::new()) }),
                        );
                    }
                }
            }
            self.set_located(&serial, serializers::located_dict(&located));
        }
        self.publish_devices(&[], "");
        self.emit_log("DEVICE", format!("局域网查找：{online} 台在线"));
        self.located_snapshot()
    }

    /// 停止轮询线程、停 Router、释放遮罩（幂等）。
    pub fn shutdown(&self) {
        if !self.started.swap(false, Ordering::SeqCst) {
            // 仍旧执行清理：重复调用不会产生副作用。
        }
        self.stop.store(true, Ordering::SeqCst);
        let handle = lock(&self.watcher).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        if let Some(router) = self.router.get() {
            router.stop();
        }
        self.mask_release();
    }

    /// `app.bootstrap` 的完整快照（Python `PadLendService.snapshot`）。
    pub fn snapshot(&self) -> Value {
        let config = self.config_clone();
        let config_value = serializers::config_dict(&config).unwrap_or(Value::Null);
        let facts = self.facts();
        let monitors = self.monitors();
        let segments = self.exterior_segments();
        let bounds = self.bounding_box();
        let placement = self.placement();
        let devices = self.devices_snapshot();
        json!({
            "version": C::VERSION,
            "config": config_value,
            "configIssues": config.issues(),
            "configPath": self.config_path.display().to_string(),
            "firstRun": self.first_run,
            "simulated": self.simulate,
            "theme": config.ui.theme,
            "state": self.state(),
            "connected": facts.is_some(),
            "facts": serializers::facts_dict(facts.as_ref()),
            "metrics": self.metrics_value(),
            "desktop": serializers::desktop_dict(&monitors, &segments, &bounds),
            "placement": serializers::placement_dict(placement.as_ref()),
            "devices": devices,
            "logs": self.snapshot_logs(),
        })
    }
}

/// 在 `located` 表里找某条记录对应的结论（端点 serial/别名）。
fn located_for(
    located: &BTreeMap<String, Value>,
    record: &registry::DeviceRecord,
) -> Option<Value> {
    for endpoint in &record.endpoints {
        if let Some(found) = located.get(&endpoint.serial) {
            return Some(found.clone());
        }
        for alias in &endpoint.aliases {
            if let Some(found) = located.get(alias) {
                return Some(found.clone());
            }
        }
    }
    None
}

/// simulate 下的固定 adb 输出（与 Python `_simulated_adb_output` 逐字一致）。
pub fn simulated_adb_output(args: &[&str]) -> String {
    let joined = args.join(" ");
    if args.first().copied() == Some("devices") {
        return "List of devices attached\nSIMULATEDPAD\tdevice usb:1-1 product:sim model:PadLend_Demo\n"
            .to_string();
    }
    if args == ["shell", "dumpsys", "input"] {
        return "Cursor Input Mapper:\n  XScale: 1.000\n".to_string();
    }
    if joined.contains("pointer_speed") {
        return "-3\n".to_string();
    }
    if args == ["shell", "id"] {
        return "uid=2000(shell) gid=2000(shell) groups=1003,uhid,1000\n".to_string();
    }
    if joined.contains("uhid") {
        return "OPEN_OK\n".to_string();
    }
    if joined.contains("adb_enabled") {
        return "1\n".to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_engine(name: &str) -> (Arc<Engine>, Arc<Mutex<Vec<(String, Value)>>>) {
        let mut dir = std::env::temp_dir();
        dir.push(format!("padlend-engine-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let config_path = dir.join("config.yaml");
        let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let engine = Engine::open(EngineOptions {
            simulate: true,
            no_hooks: true,
            config_path: Some(config_path),
            emit: Some(Arc::new(move |event: &str, data: Value| {
                if let Ok(mut guard) = sink.lock() {
                    guard.push((event.to_string(), data));
                }
            })),
            start_router: Some(false),
        })
        .expect("测试引擎应能构造");
        (engine, events)
    }

    #[test]
    fn snapshot_has_the_contract_keys() {
        let (engine, _events) = test_engine("snapshot");
        let value = engine.snapshot();
        let object = value.as_object().expect("快照应是对象");
        let mut keys: Vec<&String> = object.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "config",
                "configIssues",
                "configPath",
                "connected",
                "desktop",
                "devices",
                "facts",
                "firstRun",
                "logs",
                "metrics",
                "placement",
                "simulated",
                "state",
                "theme",
                "version",
            ]
        );
        assert_eq!(value["state"], "idle");
        assert_eq!(value["connected"], false);
        assert_eq!(value["facts"], Value::Null);
        assert!(value["logs"].as_array().map(|logs| !logs.is_empty()).unwrap_or(false));
        assert!(value["desktop"]["monitors"].is_array());
        assert!(value["desktop"]["boundingBox"].is_object());
        assert!(value["devices"].is_array());
    }

    #[test]
    fn log_ring_buffer_caps_at_capacity_and_limits_reads() {
        let (engine, _events) = test_engine("logs");
        for index in 0..(LOG_CAPACITY + 50) {
            engine.emit_log("TEST", format!("日志 {index}"));
        }
        assert_eq!(engine.log_count(), LOG_CAPACITY);
        let recent = engine.logs_recent(20);
        assert_eq!(recent.len(), 20);
        assert!(recent[19]["timestamp"].is_string());
        assert_eq!(recent[19]["event"], "TEST");
        // limit 夹取：0 → 1 条，超大 → 全量上限。
        assert_eq!(engine.logs_recent(0).len(), 1);
        assert_eq!(engine.logs_recent(10_000).len(), LOG_CAPACITY);
        // snapshot 只带最后 200 条。
        assert_eq!(engine.snapshot_logs().len(), SNAPSHOT_LOGS);
    }

    #[test]
    fn simulated_adb_output_matches_python() {
        assert!(simulated_adb_output(&["devices"]).contains("SIMULATEDPAD"));
        assert_eq!(
            simulated_adb_output(&["shell", "dumpsys", "input"]),
            "Cursor Input Mapper:\n  XScale: 1.000\n"
        );
        assert_eq!(simulated_adb_output(&["shell", "id"]).contains("uhid"), true);
        assert_eq!(
            simulated_adb_output(&["shell", "exec 3<>/dev/uhid && echo OPEN_OK"]),
            "OPEN_OK\n"
        );
        assert_eq!(simulated_adb_output(&["noop"]), "");
    }

    #[test]
    fn run_adb_uses_simulated_output_without_touching_adb() {
        let (engine, _events) = test_engine("run-adb");
        let (code, stdout, stderr) = engine.run_adb(&["devices", "-l"]);
        assert_eq!(code, 0);
        assert!(stdout.contains("SIMULATEDPAD"));
        assert!(stderr.is_empty());
        assert!(engine.system_adb().is_none(), "simulate 不得创建真 adb");
    }

    #[test]
    fn engine_reports_desktop_enumeration_smoke() {
        let adapter = match DesktopAdapter::with_win32() {
            Ok(adapter) => adapter,
            Err(err) => {
                println!("桌面枚举不可用：{err}");
                return;
            }
        };
        let monitors = adapter.monitors();
        println!("显示器数量：{}", monitors.len());
        let segments = adapter.exterior_segments();
        println!("边段数量：{}", segments.len());
        // 有显示器时边段必为 4 的整数倍（每台 4 条边）。
        if !monitors.is_empty() {
            assert_eq!(segments.len() % 4, 0);
        }
    }

    #[test]
    fn remember_device_updates_config_serial_and_registry() {
        let (engine, _events) = test_engine("remember");
        let facts = DeviceFacts {
            serial: "sim:5555".to_string(),
            model: "Simulated_Tablet".to_string(),
            width: 1707,
            height: 1067,
            orientation: 0,
            xscale: 2.0,
            pointer_speed: -3,
            hardware_serial: "HW-SIM".to_string(),
        };
        engine.remember_device("sim:5555", "", Some(&facts));
        assert_eq!(engine.config_clone().device.serial, "sim:5555");
        let registry = engine.registry_snapshot();
        let record = registry.find_by_hardware("HW-SIM").expect("应记录设备");
        assert_eq!(record.model, "Simulated_Tablet");
        assert!(record.endpoints.iter().any(|endpoint| endpoint.serial == "sim:5555"));
    }

    #[test]
    fn layout_identity_prefers_hardware_serial() {
        let (engine, _events) = test_engine("identity");
        let mut facts = DeviceFacts {
            serial: "sim:5555".to_string(),
            model: "Sim".to_string(),
            width: 1707,
            height: 1067,
            orientation: 0,
            xscale: 2.0,
            pointer_speed: -3,
            hardware_serial: "HW1".to_string(),
        };
        assert_eq!(engine.layout_identity(Some(&facts)), "HW1");
        assert_eq!(engine.orientation_bucket(Some(&facts)), "landscape");
        facts.hardware_serial = String::new();
        assert_eq!(engine.layout_identity(Some(&facts)), "sim:5555");
        facts.width = 1067;
        facts.height = 1707;
        assert_eq!(engine.orientation_bucket(Some(&facts)), "portrait");
    }

    #[test]
    fn move_layout_identity_relocates_profiles() {
        let (engine, _events) = test_engine("move-layout");
        engine.with_config(|config| {
            config.layouts.set(
                "sim:5555",
                "landscape",
                PlacementCfg {
                    edge: "left".to_string(),
                    span_end: 900,
                    ..PlacementCfg::default()
                },
            );
        });
        assert!(engine.move_layout_identity("sim:5555", "HW1"));
        let config = engine.config_clone();
        assert!(config.layouts.get("HW1", "landscape").is_some());
        assert!(config.layouts.get("sim:5555", "landscape").is_none());
        // 第二次迁移同一身份已无 profile → false。
        assert!(!engine.move_layout_identity("sim:5555", "HW1"));
    }
}
