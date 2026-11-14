//! NDJSON 传输层（对应 Python service/server.py）。
//!
//! 契约：stdin 每行一个请求对象；stdout 每行一个响应或事件。
//! stdout 只承载协议数据，人类可读日志一律走 stderr。
//!
//! 事件与响应可能来自不同线程（Router 回调 + 请求线程），这里用全局互斥锁
//! 串行化整行写入，避免半行交错。

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::methods::{self, Reply};
use crate::service::engine::{Engine, EngineOptions};

/// serve 的入参（由 main 接线）。
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// 模拟设备：不碰 adb、不建窗。
    pub simulate: bool,
    /// 不装钩子（Rust 侧仅记录，传给引擎）。
    pub no_hooks: bool,
    /// 覆盖配置路径（测试/多实例用）。
    pub config_path: Option<PathBuf>,
}

static STDOUT: OnceLock<Mutex<io::Stdout>> = OnceLock::new();

fn stdout_lock() -> &'static Mutex<io::Stdout> {
    STDOUT.get_or_init(|| Mutex::new(io::stdout()))
}

/// 线程安全地写一行 JSON。
pub(crate) fn write_value(value: &Value) {
    let Ok(text) = serde_json::to_string(value) else {
        return;
    };
    let mut guard = match stdout_lock().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let _ = writeln!(guard, "{text}");
    let _ = guard.flush();
}

/// 写一条事件：{"event":..., "data":...}。
pub(crate) fn write_event(event: &str, data: &Value) {
    write_value(&json!({ "event": event, "data": data }));
}

/// 启动服务：建 Engine → ready → 逐行请求 → service.stopped。
pub fn serve(options: ServeOptions) -> Result<(), String> {
    let engine = Engine::open(EngineOptions {
        simulate: options.simulate,
        no_hooks: options.no_hooks,
        config_path: options.config_path,
        emit: None,
        start_router: None,
    })?;
    if !methods::install(engine.clone()) {
        return Err("引擎已经安装过，不能重复 serve".to_string());
    }

    // 与 Python 一致：启动完成后先发 ready（data 为完整快照）。
    write_value(&json!({ "event": "ready", "data": engine.snapshot() }));

    let stdin = io::stdin();
    let reader = stdin.lock();
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                eprintln!("[rpc] 读取 stdin 失败：{err}");
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                write_value(&json!({
                    "id": Value::Null,
                    "ok": false,
                    "error": format!("请求不是合法 JSON：{err}"),
                }));
                continue;
            }
        };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

        // app.shutdown 需要在写响应之后立刻停掉轮询线程。
        if method == "app.shutdown" {
            engine.shutdown();
            write_value(&json!({ "id": id, "ok": true, "result": { "ok": true } }));
            break;
        }

        // 每个请求独立线程：devices.connect 要跑几十秒（探测候选端口、推 jar、
        // 握手、回退 audio），同步分派会让 stdin 循环停在这里，其它按钮的请求
        // （保存配置、断开、急停）全部排队 → 2026-11-14 用户实测"连接中"时整页
        // 点什么都没反应。stdout 已有全局锁，多线程写不会串行错乱。
        spawn_request(method, params, id);
    }

    engine.shutdown();
    write_value(&json!({ "event": "service.stopped", "data": {} }));
    Ok(())
}

/// 在独立线程里执行一次请求并把响应写回 stdout。
///
/// 只允许"读-改-写"级别的并发：引擎内部状态都由互斥量保护，dispatch 本身
/// 不持有任何跨请求的锁；响应按 id 由前端配对，返回顺序无所谓。
fn spawn_request(method: String, params: Value, id: Value) {
    let started = std::time::Instant::now();
    let thread_method = method.clone();
    let thread_id = id.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("rpc-{method}"))
        .spawn(move || {
            let reply = methods::dispatch(&thread_method, &params);
            match reply {
                Reply::Ok(result) => {
                    write_value(&json!({ "id": thread_id, "ok": true, "result": result }));
                }
                Reply::Error(err) => {
                    eprintln!("[rpc] {thread_method} 失败：{err}");
                    write_value(&json!({ "id": thread_id, "ok": false, "error": err }));
                }
                Reply::Shutdown(result) => {
                    // app.shutdown 在 stdin 循环里特判，正常不会走到这里。
                    write_value(&json!({ "id": thread_id, "ok": true, "result": result }));
                }
            }
            let elapsed = started.elapsed();
            if elapsed >= std::time::Duration::from_millis(1500) {
                eprintln!(
                    "[rpc] {thread_method} 耗时 {}ms（期间其它请求不受影响）",
                    elapsed.as_millis()
                );
            }
        });
    if let Err(error) = spawned {
        eprintln!("[rpc] 请求线程启动失败：{error}");
        write_value(
            &json!({ "id": id, "ok": false, "error": format!("请求线程启动失败：{error}") }),
        );
    }
}
