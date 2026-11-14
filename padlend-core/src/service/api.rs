//! 25 个 RPC 方法的实现（对应 Python service/api.py 的 handlers）。
//!
//! 返回值形状与 tests/fixtures/rpc_golden/simulate.ndjson 对齐；错误文案逐字
//! 沿用 Python（gate 会比对 ok:false 的 error 文本）。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::constants as C;
use crate::methods::Reply;
use crate::tablet::adb::normalize_address;

use super::diagnostics;
use super::engine::Engine;
use super::serializers as S;

/// RPC 连接等待上限（Python deadline = 45.0 秒）。
const CONNECT_TIMEOUT_S: f64 = 30.0;
/// 点击连接时对无线地址做的一次 adb connect 超时。
///
/// 20s 只适合"添加设备"那种明确的新地址；界面点连接时地址通常已经在
/// adb devices 里，真不可达也应该在几秒内给出结果，而不是让"连接中"空转。
const CONNECT_ADB_TIMEOUT_S: f64 = 6.0;
/// 连接过程中每隔多久写一条"仍在连接"日志（用户能看到进度）。
const CONNECT_PROGRESS_S: f64 = 8.0;

/// 分派一个方法；app.shutdown 由 methods 层提前拦截。
pub fn call(engine: &Arc<Engine>, method: &str, params: &Value) -> Reply {
    match dispatch(engine, method, params) {
        Ok(value) => Reply::Ok(value),
        Err(message) => Reply::Error(message),
    }
}

fn dispatch(engine: &Arc<Engine>, method: &str, params: &Value) -> Result<Value, String> {
    match method {
        // -- app ---------------------------------------------------------
        "app.bootstrap" => Ok(engine.snapshot()),
        "app.shutdown" => Ok(json!({ "ok": true })),
        // -- 系统 / 配置 --------------------------------------------------
        "system.paths" => Ok(system_paths(engine)),
        "config.get" => config_get(engine),
        "config.save" => config_save(engine, params),
        // -- 设备 ---------------------------------------------------------
        "devices.scan" => Ok(devices_scan(engine)),
        "devices.locate" => Ok(devices_locate(engine, params)),
        "devices.add" => devices_add(engine, params),
        "devices.connect" => devices_connect(engine, params),
        "devices.disconnect" => Ok(devices_disconnect(engine)),
        "devices.cancel" => Ok(devices_cancel(engine)),
        "devices.remove" => devices_remove(engine, params),
        // -- 布局 ---------------------------------------------------------
        "layout.desktop" => Ok(layout_desktop(engine)),
        "layout.initial" => Ok(layout_initial(engine)),
        "layout.save" => layout_save(engine, params),
        // -- 引擎 ---------------------------------------------------------
        "engine.toggle" => Ok(engine_toggle(engine)),
        "engine.panic" => Ok(engine_panic(engine)),
        "engine.reconnect" => Ok(engine_reconnect(engine)),
        "engine.inject_clipboard" => Ok(engine_inject_clipboard(engine, params)),
        "engine.state" => Ok(engine_state(engine)),
        "engine.metrics" => Ok(engine.current_metrics_value()),
        // -- 诊断 ---------------------------------------------------------
        "diagnostics.run" => diagnostics_run(engine),
        "diagnostics.export" => diagnostics_export(engine, params),
        "diagnostics.static" => Ok(json!({ "text": diagnostics::static_summary() })),
        // -- 向导 / 日志 ---------------------------------------------------
        "wizard.check" => Ok(wizard_check(engine, params)),
        "logs.recent" => Ok(logs_recent(engine, params)),
        other => Err(format!("未知方法：{other}")),
    }
}

// -- 小工具 --------------------------------------------------------------

/// 取字符串参数（去空白），缺失/类型不符返回空串。
fn text_param(params: &Value, key: &str) -> String {
    params
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 取整数参数（接受数字与数字字符串）。
fn int_param(params: &Value, key: &str) -> i64 {
    match params.get(key) {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

fn config_path_text(engine: &Engine) -> String {
    engine.config_path().display().to_string()
}

// -- app / config --------------------------------------------------------

fn system_paths(engine: &Engine) -> Value {
    json!({
        "configPath": config_path_text(engine),
        "logDir": C::log_dir().map(|path| path.display().to_string()).unwrap_or_default(),
        "noticesPath": C::app_dir().join("THIRD_PARTY_NOTICES.md").display().to_string(),
        "version": C::VERSION,
        "license": C::LICENSE,
        "appName": C::APP_NAME,
    })
}

fn config_get(engine: &Arc<Engine>) -> Result<Value, String> {
    Ok(json!({
        "config": engine.config_dict()?,
        "issues": engine.issues(),
        "path": config_path_text(engine),
    }))
}

/// 把引擎里的最新配置推给会话。
///
/// Python 的会话共享同一个 Config 对象，保存即可见；Rust 的会话配置是构造期快照，
/// 只在连接时读取的开关（转发平板声音、熄屏、端口、指针速度）必须显式推送。
fn sync_session_config(engine: &Arc<Engine>) {
    if let Some(session) = engine.session_port() {
        session.sync_config(&engine.config_clone());
    }
}

fn config_save(engine: &Arc<Engine>, params: &Value) -> Result<Value, String> {
    let patch = match params.get("patch") {
        None | Some(Value::Null) => json!({}),
        Some(value) => value.clone(),
    };
    if !patch.is_object() {
        return Err("配置补丁必须是对象".to_string());
    }
    let audio_before = engine.with_config(|config| config.misc.forward_audio);
    let backup = engine.config_clone();
    let mut problems = Vec::new();
    engine.with_config(|config| {
        problems.extend(S::apply_config_patch(config, &patch));
        problems.extend(config.validate());
    });
    if !problems.is_empty() {
        // 先改后校验：失败必须整体回滚，避免污染运行中的配置。
        engine.replace_config(backup);
        return Err(problems.join("；"));
    }
    engine.save_config()?;
    sync_session_config(engine);
    let config = engine.config_dict()?;
    engine.emit("config", json!({ "config": config.clone(), "issues": [] }));
    engine.emit_log("CONFIG", format!("配置已保存：{}", config_path_text(engine)));
    let audio_after = engine.with_config(|config| config.misc.forward_audio);
    if audio_before != audio_after {
        engine.emit_log("AUDIO", "音频转发设置已保存，重新连接平板后生效");
    }
    Ok(json!({ "config": config, "issues": [], "path": config_path_text(engine) }))
}

// -- 设备 ----------------------------------------------------------------

fn devices_scan(engine: &Arc<Engine>) -> Value {
    let (entries, error) = engine.scan_devices();
    let devices = engine.publish_devices(&entries, &error);
    let hint = if !error.is_empty() {
        if error.to_lowercase().contains("adb") {
            "未找到 adb（打包版应内置）".to_string()
        } else {
            error.clone()
        }
    } else {
        let online = devices
            .iter()
            .filter(|item| item.get("online").and_then(Value::as_bool).unwrap_or(false))
            .count();
        format!("{online} 台设备")
    };
    engine.emit_log("DEVICE", format!("扫描设备：{hint}"));
    json!({ "devices": devices, "hint": hint, "error": error })
}

fn devices_locate(engine: &Arc<Engine>, _params: &Value) -> Value {
    // 与 Python 一致：忽略请求参数，查找对象来自设备注册表（simulate 下直接回放缓存）。
    let located = engine.locate_devices();
    json!({ "located": located })
}

fn devices_add(engine: &Arc<Engine>, params: &Value) -> Result<Value, String> {
    let address = normalize_address(&text_param(params, "address"));
    if address.is_empty() {
        return Err("请填写平板上的 IP 地址与端口".to_string());
    }
    let Some(adb) = engine.adb_api() else {
        return Err("adb 接口不可用".to_string());
    };
    let code = text_param(params, "code");
    if !code.is_empty() {
        let _ = adb.pair(&address, &code, 30.0);
    }
    let _ = adb.connect(&address, 20.0);
    devices_connect(engine, &json!({ "serial": address }))
}

fn devices_connect(engine: &Arc<Engine>, params: &Value) -> Result<Value, String> {
    let serial = text_param(params, "serial");
    if serial.is_empty() {
        return Err("请选择一个已授权的设备".to_string());
    }
    let Some(router) = engine.router().cloned() else {
        return Err("引擎未启动".to_string());
    };
    // 先把目标写进引擎配置：会话的候选串来自 device_candidates()，不同步会出现
    // "点了新端口、会话仍连配置/注册表里的旧端口"（真机日志里的 44731）。
    // 取代次要在触发连接之前：此刻之后任何一次 devices.cancel 都会让本轮
    // 轮询立刻收尾（否则用户只能干等 CONNECT_TIMEOUT_S）。
    let epoch = engine.connect_epoch();
    engine.set_requested_serial(&serial);
    engine.emit_log("DEVICE", format!("连接请求：{serial}"));
    if serial.contains(':') {
        if let Some(adb) = engine.adb_api() {
            let _ = adb.connect(&serial, CONNECT_ADB_TIMEOUT_S);
        }
    }
    // adb connect 可能占了整整 CONNECT_ADB_TIMEOUT_S，期间用户已经点了「取消连接」；
    // 这时再 connect_device 会把刚被 disconnect_device 挂起的会话重新拉起来。
    if engine.connect_cancelled(epoch) {
        engine.emit_log("DEVICE", "连接已取消（用户主动）");
        return Ok(json!({ "ok": false, "cancelled": true, "message": "连接已取消" }));
    }
    router.connect_device(Some(serial.as_str()));
    engine.enable_facts();
    let deadline = Instant::now() + Duration::from_secs_f64(CONNECT_TIMEOUT_S);
    let mut found = None;
    let mut last_progress = Instant::now();
    while Instant::now() < deadline {
        if engine.connect_cancelled(epoch) {
            // 取消可能发生在 connect_device 之后，会话已经在后台跑起来了：
            // 再挂起一次，保证被取消的连接不会自己在后面连上。
            router.disconnect_device();
            engine.emit_log("DEVICE", "连接已取消（用户主动）");
            return Ok(json!({ "ok": false, "cancelled": true, "message": "连接已取消" }));
        }
        if let Some(facts) = router.facts() {
            if !facts.serial.trim().is_empty() {
                found = Some(facts);
                break;
            }
        }
        if last_progress.elapsed() >= Duration::from_secs_f64(CONNECT_PROGRESS_S) {
            last_progress = Instant::now();
            engine.emit_log("DEVICE", format!("仍在连接 {serial}（可点「取消连接」中止）"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    match found {
        Some(facts) => {
            engine.remember_device(&serial, "", Some(&facts));
            // 与 Python 的 _capture_mac 一致：无线设备连上后**后台**读一次 Wi-Fi MAC，
            // 路由器换 IP 时还能靠它找回平板。读不到就保持空串，绝不阻塞连接流程。
            if crate::service::registry::is_wireless_serial(&serial) {
                if let Some(adb) = engine.adb_api() {
                    let engine_bg = Arc::clone(engine);
                    let serial_bg = serial.clone();
                    std::thread::spawn(move || {
                        let mac = adb.wifi_mac(Some(serial_bg.as_str()));
                        let mac = mac.trim();
                        if !mac.is_empty() {
                            engine_bg.remember_device(&serial_bg, mac, None);
                        }
                    });
                }
            }
            engine.sync_facts_now();
            engine.emit_log("DEVICE", format!("已连接 {serial}"));
            Ok(json!({ "ok": true, "message": "" }))
        }
        None => {
            let message = format!(
                "连接超时（{CONNECT_TIMEOUT_S:.0}s）：请确认 USB 调试已授权，且 adb 能看到设备"
            );
            engine.emit_log("DEVICE", format!("操作失败：{message}"));
            Ok(json!({ "ok": false, "message": message }))
        }
    }
}

/// 取消正在进行的首连（设备页"取消连接"按钮）。
///
/// 线程化 RPC 之后这条请求不会再被 devices.connect 挡住，所以用户随时能按。
fn devices_cancel(engine: &Arc<Engine>) -> Value {
    engine.cancel_connect();
    json!({ "ok": true })
}

fn devices_disconnect(engine: &Arc<Engine>) -> Value {
    if let Some(router) = engine.router() {
        router.disconnect_device();
    }
    engine.emit_log("DEVICE", "已断开");
    json!({ "ok": true })
}

fn devices_remove(engine: &Arc<Engine>, params: &Value) -> Result<Value, String> {
    let serial = text_param(params, "serial");
    if serial.is_empty() {
        return Ok(json!({ "ok": false }));
    }
    let current = engine.config_clone().device.serial;
    let record = engine.with_registry(|registry| registry.find_by_serial(&serial).cloned());
    let mut removed = false;
    let mut removed_current = false;
    match &record {
        Some(record) => {
            for endpoint in record.endpoints.clone() {
                removed =
                    engine.with_registry(|registry| registry.remove(&endpoint.serial)) || removed;
                for alias in &endpoint.aliases {
                    engine.remove_located(alias);
                }
                engine.remove_located(&endpoint.serial);
                if endpoint.serial == current
                    || endpoint.aliases.iter().any(|alias| alias == &current)
                {
                    removed_current = true;
                }
            }
        }
        None => {
            removed = engine.with_registry(|registry| registry.remove(&serial));
        }
    }
    if removed {
        engine.save_registry();
        engine.remove_located(&serial);
    }
    if removed_current || current == serial {
        let remaining = engine.with_registry(|registry| registry.serials(""));
        engine.with_config(|config| {
            config.device.serial = remaining.first().cloned().unwrap_or_default();
        });
    }
    engine.save_config()?;
    sync_session_config(engine);
    let config = engine.config_dict()?;
    engine.emit("config", json!({ "config": config.clone(), "issues": [] }));
    engine.emit_log("DEVICE", format!("已从列表移除 {serial}"));
    Ok(json!({ "ok": true, "config": config }))
}

// -- 布局 ----------------------------------------------------------------

fn layout_desktop(engine: &Arc<Engine>) -> Value {
    engine.refresh_desktop();
    let monitors = engine.monitors();
    let segments = engine.exterior_segments();
    let bounds = engine.bounding_box();
    let mut value = S::desktop_dict(&monitors, &segments, &bounds);
    value["placement"] = S::placement_dict(engine.placement().as_ref());
    value["tablet"] = S::facts_dict(engine.facts().as_ref());
    value
}

fn layout_initial(engine: &Arc<Engine>) -> Value {
    json!({ "placement": S::placement_dict(engine.placement().as_ref()) })
}

fn layout_save(engine: &Arc<Engine>, params: &Value) -> Result<Value, String> {
    let data = params.get("placement").cloned().unwrap_or(Value::Null);
    if !data.is_object() {
        return Err("没有可保存的布局".to_string());
    }
    let placement = S::placement_from_dict(&data);
    if engine.monitor_device(placement.segment.monitor_handle).is_empty() {
        return Err("摆位对应的显示器已不存在，请重新拖动".to_string());
    }
    let mut identity = text_param(params, "identity");
    if identity.is_empty() {
        identity = engine.layout_identity(None);
    }
    let mut bucket = text_param(params, "orientation");
    if bucket.is_empty() {
        bucket = engine.orientation_bucket(None);
    }
    let path = engine.config_path().to_path_buf();
    let problems = engine.with_config(|config| {
        let mut problems = Vec::new();
        if !identity.is_empty() && (bucket == "landscape" || bucket == "portrait") {
            let mut profile = crate::config::PlacementCfg::default();
            engine.apply_placement_to_config(&placement, &mut profile);
            config.layouts.set(&identity, &bucket, profile);
        }
        engine.apply_placement_to_config(&placement, &mut config.placement);
        // 布局页只校验自己负责的字段。
        problems.extend(
            config
                .validate()
                .into_iter()
                .filter(|problem| problem.starts_with("placement.")),
        );
        if problems.is_empty() {
            if let Err(err) = config.save(&path) {
                problems.push(err.to_string());
            }
        }
        problems
    });
    if !problems.is_empty() {
        return Err(problems.join("；"));
    }
    engine.set_placement_value(placement.clone());
    engine.set_layout_key(&identity, &bucket);
    engine.emit(
        "placement",
        json!({
            "placement": S::placement_dict(Some(&placement)),
            "dirty": false,
            "reason": "saved",
        }),
    );
    let config = engine.config_dict()?;
    engine.emit("config", json!({ "config": config.clone(), "issues": [] }));
    let bucket_text = if bucket == "landscape" { "横屏" } else { "竖屏" };
    engine.emit_log(
        "PLACE",
        format!(
            "布局已保存（{bucket_text}）：吸附 {} offset={:.2}",
            placement.segment.edge.as_str(),
            placement.offset
        ),
    );
    Ok(json!({
        "config": config,
        "placement": S::placement_dict(Some(&placement)),
    }))
}

// -- 引擎 ----------------------------------------------------------------

pub(crate) fn engine_toggle(engine: &Arc<Engine>) -> Value {
    if let Some(router) = engine.router() {
        router.toggle_manual();
    }
    engine.emit_log("MANUAL", "手动接管 / 交还");
    json!({ "ok": true })
}

pub(crate) fn engine_panic(engine: &Arc<Engine>) -> Value {
    if let Some(router) = engine.router() {
        router.emergency_stop();
    }
    engine.mask_release();
    engine.emit_log("PANIC", "急停：已释放遮罩与输入");
    json!({ "ok": true })
}

fn engine_reconnect(engine: &Arc<Engine>) -> Value {
    if let Some(router) = engine.router() {
        router.resync_pointer();
    }
    engine.emit_log("RECONNECT", "重新同步指针");
    json!({ "ok": true })
}

fn engine_inject_clipboard(engine: &Arc<Engine>, params: &Value) -> Value {
    let text = params.get("text").and_then(Value::as_str).unwrap_or_default();
    let mut ok = false;
    if !text.is_empty() {
        if let Some(router) = engine.router() {
            ok = router.inject_clipboard(text);
        }
    }
    json!({ "ok": ok })
}

fn engine_state(engine: &Arc<Engine>) -> Value {
    let facts = engine.facts();
    json!({
        "state": engine.state(),
        "connected": facts.is_some(),
        "facts": S::facts_dict(facts.as_ref()),
        "metrics": engine.metrics_value(),
        "manual": engine.manual(),
    })
}

// -- 诊断 ----------------------------------------------------------------

fn diagnostics_run(engine: &Arc<Engine>) -> Result<Value, String> {
    let mut results: Vec<diagnostics::CommandResult> = Vec::new();
    for (label, args) in diagnostics::STEPS.iter() {
        let (code, stdout, stderr) = engine.run_adb(args);
        results.push(diagnostics::CommandResult {
            label: label.to_string(),
            code,
            stdout,
            stderr,
        });
        engine.emit("diagnostics.progress", json!({ "label": label }));
    }
    let report = diagnostics::build_report(&results);
    let device = report
        .primary_device()
        .map(|entry| (entry.serial.clone(), entry.state.clone()));
    let text = report.to_text();
    let summary = report.summary();
    let errors = report.errors.clone();
    let collected_at = report.collected_at.clone();
    engine.set_report(report);
    engine.emit_log("DIAG", "诊断采集完成");
    let (serial, state) = match device {
        Some((_serial, state)) => ("<serial>".to_string(), state),
        None => (String::new(), String::new()),
    };
    Ok(json!({
        "text": text,
        "summary": summary,
        "errors": errors,
        "device": { "serial": serial, "state": state },
        "collectedAt": collected_at,
    }))
}

fn diagnostics_export(engine: &Arc<Engine>, params: &Value) -> Result<Value, String> {
    let Some(report) = engine.report() else {
        return Err("请先采集诊断信息".to_string());
    };
    let directory = text_param(params, "directory");
    let directory = if directory.is_empty() {
        None
    } else {
        Some(Path::new(&directory))
    };
    let path = report.export(directory)?;
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        engine.emit_log("DIAG", format!("已导出诊断包：{name}"));
    }
    Ok(json!({ "path": path.display().to_string() }))
}

// -- 向导 / 日志 ---------------------------------------------------------

fn wizard_check(engine: &Arc<Engine>, params: &Value) -> Value {
    let step = int_param(params, "step");
    if step == 0 {
        let (code, stdout, stderr) = engine.run_adb(&["devices", "-l"]);
        let serials: Vec<String> = stdout
            .lines()
            .filter(|line| line.contains("\tdevice"))
            .map(|line| line.split_whitespace().next().unwrap_or_default().to_string())
            .filter(|serial| !serial.is_empty())
            .collect();
        if stdout.contains("unauthorized") {
            return json!({
                "ok": false,
                "text": "设备显示 unauthorized：请在平板上点「允许 USB 调试」。",
            });
        }
        if code != 0 || serials.is_empty() {
            let text = if stderr.is_empty() {
                "未检测到设备，请检查数据线与 USB 调试。"
            } else {
                stderr.as_str()
            };
            return json!({ "ok": false, "text": text });
        }
        return json!({
            "ok": true,
            "text": format!("已连接：{}；USB 调试已开启。", serials.join(", ")),
        });
    }
    if step == 1 {
        let (_, group, _) = engine.run_adb(&["shell", "id"]);
        let (_, opened, _) = engine.run_adb(&["shell", "exec 3<>/dev/uhid && echo OPEN_OK"]);
        let has_group = group.contains("uhid");
        let can_open = opened.contains("OPEN_OK");
        let text = [
            format!("uhid 组：{}", if has_group { "有" } else { "无" }),
            format!("/dev/uhid 可打开：{}", if can_open { "是" } else { "否" }),
            format!("相对鼠标注入{}。", if can_open { "可用" } else { "不可用" }),
        ]
        .join("\n");
        return json!({ "ok": can_open, "text": text });
    }
    json!({ "ok": true, "text": "一切就绪，回到主窗口开始使用。" })
}

fn logs_recent(engine: &Arc<Engine>, params: &Value) -> Value {
    let limit = int_param(params, "limit");
    let limit = if limit <= 0 { 200 } else { limit as usize };
    json!({ "logs": engine.logs_recent(limit) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::engine::EngineOptions;
    use std::sync::Mutex;

    fn engine_for(name: &str) -> Arc<Engine> {
        let mut dir = std::env::temp_dir();
        dir.push(format!("padlend-api-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        Engine::open(EngineOptions {
            simulate: true,
            no_hooks: true,
            config_path: Some(dir.join("config.yaml")),
            emit: Some(Arc::new(move |event: &str, data: Value| {
                if let Ok(mut guard) = sink.lock() {
                    guard.push((event.to_string(), data));
                }
            })),
            start_router: Some(false),
        })
        .expect("测试引擎应能构造")
    }

    fn ok_value(reply: Reply) -> Value {
        match reply {
            Reply::Ok(value) => value,
            Reply::Error(message) => panic!("期望成功，得到错误：{message}"),
            Reply::Shutdown(value) => value,
        }
    }

    #[test]
    fn system_paths_has_six_keys() {
        let engine = engine_for("paths");
        let value = ok_value(call(&engine, "system.paths", &json!({})));
        let object = value.as_object().expect("应是对象");
        assert_eq!(object.len(), 6);
        assert!(object.contains_key("configPath"));
        assert!(object.contains_key("noticesPath"));
        assert_eq!(value["license"], "Apache-2.0");
        assert_eq!(value["appName"], "PadLend");
    }

    #[test]
    fn config_get_and_save_roundtrip() {
        let engine = engine_for("config");
        let got = ok_value(call(&engine, "config.get", &json!({})));
        assert!(got["config"].is_object());
        assert!(got["issues"].is_array());
        assert!(got["path"].as_str().map(|text| !text.is_empty()).unwrap_or(false));

        let saved = ok_value(call(
            &engine,
            "config.save",
            &json!({ "patch": { "ui": { "theme": "light" } } }),
        ));
        assert_eq!(saved["issues"], json!([]));
        assert_eq!(saved["config"]["ui"]["theme"], "light");
        assert_eq!(engine.config_clone().ui.theme, "light");

        // 非法端口要整体回滚并返回中文问题。
        match call(
            &engine,
            "config.save",
            &json!({ "patch": { "device": { "server_port": 1 } } }),
        ) {
            Reply::Error(message) => assert!(message.contains("device.server_port")),
            other => panic!("非法端口应失败：{other:?}"),
        }
        assert_eq!(engine.config_clone().ui.theme, "light", "失败后必须回滚");
    }

    #[test]
    fn config_save_requires_object_patch() {
        let engine = engine_for("patch-type");
        match call(&engine, "config.save", &json!({ "patch": 42 })) {
            Reply::Error(message) => assert_eq!(message, "配置补丁必须是对象"),
            other => panic!("应报错：{other:?}"),
        }
    }

    #[test]
    fn layout_save_without_placement_is_rejected_verbatim() {
        let engine = engine_for("layout-save");
        match call(&engine, "layout.save", &json!({})) {
            Reply::Error(message) => assert_eq!(message, "没有可保存的布局"),
            other => panic!("应报错：{other:?}"),
        }
    }

    #[test]
    fn devices_scan_hint_and_shape_when_simulated() {
        let engine = engine_for("scan");
        let value = ok_value(call(&engine, "devices.scan", &json!({})));
        assert!(value["devices"].is_array());
        assert_eq!(value["hint"], "1 台设备");
        assert_eq!(value["error"], "");
        let first = &value["devices"][0];
        for key in [
            "serial",
            "id",
            "state",
            "model",
            "transport",
            "online",
            "remembered",
            "located",
            "locatedDetail",
            "lastSeen",
        ] {
            assert!(first.get(key).is_some(), "缺键 {key}");
        }
        assert_eq!(first["serial"], "sim:5555");
        assert_eq!(first["transport"], "无线");
        assert_eq!(first["online"], true);
    }

    #[test]
    fn engine_state_shape_when_idle() {
        let engine = engine_for("state");
        let value = ok_value(call(&engine, "engine.state", &json!({})));
        assert!(value["state"].is_string());
        assert_eq!(value["connected"], false);
        assert_eq!(value["facts"], Value::Null);
        assert!(value["metrics"].is_object());
        assert_eq!(value["manual"], false);
    }

    #[test]
    fn engine_controls_return_ok() {
        let engine = engine_for("controls");
        for method in ["engine.toggle", "engine.panic", "engine.reconnect"] {
            let value = ok_value(call(&engine, method, &json!({})));
            assert_eq!(value, json!({ "ok": true }), "{method}");
        }
        let injected = ok_value(call(
            &engine,
            "engine.inject_clipboard",
            &json!({ "text": "padlend" }),
        ));
        assert!(injected["ok"].is_boolean());
    }

    #[test]
    fn wizard_step0_reports_simulated_device() {
        let engine = engine_for("wizard");
        let value = ok_value(call(&engine, "wizard.check", &json!({ "step": 0 })));
        assert_eq!(value["ok"], true);
        assert_eq!(value["text"], "已连接：SIMULATEDPAD；USB 调试已开启。");
        let step1 = ok_value(call(&engine, "wizard.check", &json!({ "step": 1 })));
        assert_eq!(step1["ok"], true);
        assert!(step1["text"].as_str().unwrap_or_default().contains("uhid 组：有"));
    }

    #[test]
    fn diagnostics_static_run_export_flow() {
        let engine = engine_for("diag");
        let static_value = ok_value(call(&engine, "diagnostics.static", &json!({})));
        assert!(static_value["text"].as_str().unwrap_or_default().contains("PadLend"));
        match call(&engine, "diagnostics.export", &json!({})) {
            Reply::Error(message) => assert_eq!(message, "请先采集诊断信息"),
            other => panic!("未采集时导出应失败：{other:?}"),
        }
        let run = ok_value(call(&engine, "diagnostics.run", &json!({})));
        assert_eq!(run["device"]["serial"], "<serial>");
        assert_eq!(run["summary"].as_array().map(Vec::len), Some(6));
        assert!(run["errors"].is_array());
        // 导出写到临时目录，检查返回 path 与文件落盘。
        let mut dir = std::env::temp_dir();
        dir.push(format!("padlend-diag-export-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let exported = ok_value(call(
            &engine,
            "diagnostics.export",
            &json!({ "directory": dir.display().to_string() }),
        ));
        let path = exported["path"].as_str().unwrap_or_default().to_string();
        assert!(std::path::Path::new(&path).exists(), "导出文件应存在：{path}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn logs_recent_respects_limit() {
        let engine = engine_for("logs");
        for index in 0..5 {
            engine.emit_log("TEST", format!("第 {index} 条"));
        }
        let value = ok_value(call(&engine, "logs.recent", &json!({ "limit": 3 })));
        assert_eq!(value["logs"].as_array().map(Vec::len), Some(3));
        assert!(value["logs"][0]["timestamp"].is_string());
        let default = ok_value(call(&engine, "logs.recent", &json!({})));
        assert!(default["logs"].as_array().map(Vec::len).unwrap_or(0) >= 5);
    }

    #[test]
    fn unknown_method_is_reported_by_dispatch() {
        let engine = engine_for("unknown");
        match call(&engine, "no.such", &json!({})) {
            Reply::Error(message) => assert_eq!(message, "未知方法：no.such"),
            other => panic!("应报未知方法：{other:?}"),
        }
    }
}
