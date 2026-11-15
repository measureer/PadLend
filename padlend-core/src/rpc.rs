//! NDJSON 传输层（对应 Python service/server.py）。
//!
//! 契约：stdin 每行一个请求对象；stdout 每行一个响应或事件。
//! stdout 只承载协议数据，人类可读日志一律走 stderr。
//!
//! 事件与响应可能来自不同线程（Router 回调 + 请求线程），这里用全局互斥锁
//! 串行化整行写入，避免半行交错。

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

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

/// 同时在飞的请求上限。
///
/// 每个请求一个线程且没有上限时，前端连点/脚本刷请求能把线程数堆到几千，
/// 内存和 adb server 一起被打爆。32 足够覆盖"多个按钮同时按"，超出的请求
/// 立刻回一条明确错误，不排队（排队只会让界面看起来卡死）。
const MAX_IN_FLIGHT: usize = 32;
/// 退出时等待在飞请求的上限：关窗不能卡几秒以上。
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1500);

static STDOUT: OnceLock<Mutex<io::Stdout>> = OnceLock::new();

fn stdout_lock() -> &'static Mutex<io::Stdout> {
    STDOUT.get_or_init(|| Mutex::new(io::stdout()))
}

/// 在飞请求计数 + 退出时的汇合点。
struct InFlight {
    limit: usize,
    active: Mutex<usize>,
    idle: Condvar,
}

impl InFlight {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            active: Mutex::new(0),
            idle: Condvar::new(),
        }
    }

    /// 申请一个名额；已满返回 `None`（调用方立刻回"并发请求过多"）。
    fn try_enter(self: &Arc<Self>) -> Option<InFlightGuard> {
        let mut active = lock_count(&self.active);
        if *active >= self.limit {
            return None;
        }
        *active += 1;
        Some(InFlightGuard {
            gate: Arc::clone(self),
        })
    }

    fn count(&self) -> usize {
        *lock_count(&self.active)
    }

    /// 等所有在飞请求收尾或超时；返回剩余数量（0 = 都完成了）。
    fn wait_idle(&self, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        let mut active = lock_count(&self.active);
        while *active > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            active = match self.idle.wait_timeout(active, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        *active
    }
}

/// 名额的 RAII 释放：线程 panic 也会归还（手工递减漏掉一次就永远卡在"满"）。
struct InFlightGuard {
    gate: Arc<InFlight>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut active = lock_count(&self.gate.active);
        *active = (*active).saturating_sub(1);
        drop(active);
        self.gate.idle.notify_all();
    }
}

fn lock_count(mutex: &Mutex<usize>) -> std::sync::MutexGuard<'_, usize> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

static IN_FLIGHT: OnceLock<Arc<InFlight>> = OnceLock::new();

fn in_flight() -> &'static Arc<InFlight> {
    IN_FLIGHT.get_or_init(|| Arc::new(InFlight::new(MAX_IN_FLIGHT)))
}

/// 线程安全地写一行 JSON；失败只记 stderr，不中断调用方。
pub(crate) fn write_value(value: &Value) {
    if let Err(err) = try_write_value(value) {
        eprintln!("[rpc] 写 stdout 失败：{err}（前端可能收不到这次响应）");
    }
}

/// 写一行 JSON，序列化或写管道失败都交给调用方处理。
fn try_write_value(value: &Value) -> io::Result<()> {
    let text = serde_json::to_string(value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let mut guard = match stdout_lock().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    writeln!(guard, "{text}")?;
    guard.flush()
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
    // 在飞请求可能还在写响应：等一小会儿，别让 service.stopped 抢在它们的响应前面。
    // 关窗不能卡太久，超时就照常退出（只记一行 stderr）。
    let before = in_flight().count();
    let remaining = in_flight().wait_idle(SHUTDOWN_GRACE);
    if remaining > 0 {
        eprintln!(
            "[rpc] 退出时仍有 {remaining}/{before} 个请求未完成（已等待 {}ms）",
            SHUTDOWN_GRACE.as_millis()
        );
    } else if before > 0 {
        eprintln!("[rpc] 退出前已等完 {before} 个在飞请求");
    }
    write_value(&json!({ "event": "service.stopped", "data": {} }));
    Ok(())
}

/// 在独立线程里执行一次请求并把响应写回 stdout。
///
/// 只允许"读-改-写"级别的并发：引擎内部状态都由互斥量保护，dispatch 本身
/// 不持有任何跨请求的锁；响应按 id 由前端配对，返回顺序无所谓。
fn spawn_request(method: String, params: Value, id: Value) {
    // 名额在 spawn 之前拿：线程总数不会超过 MAX_IN_FLIGHT，超出的请求立刻回错误。
    let Some(guard) = in_flight().try_enter() else {
        eprintln!("[rpc] 并发请求已达上限 {MAX_IN_FLIGHT}，拒绝 {method}");
        write_value(&json!({
            "id": id,
            "ok": false,
            "error": format!("并发请求过多（上限 {MAX_IN_FLIGHT}），{method} 未执行，请稍后重试"),
        }));
        return;
    };
    let started = std::time::Instant::now();
    let thread_method = method.clone();
    let thread_id = id.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("rpc-{method}"))
        .spawn(move || {
            // 名额跟着闭包走：线程跑完（或 spawn 失败闭包被丢）都会归还。
            let _guard = guard;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_rejects_when_full_and_releases() {
        let gate = Arc::new(InFlight::new(2));
        let first = gate.try_enter().expect("第一个名额");
        let second = gate.try_enter().expect("第二个名额");
        assert_eq!(gate.count(), 2);
        assert!(gate.try_enter().is_none(), "满员必须拒绝");
        drop(first);
        assert_eq!(gate.count(), 1);
        let third = gate.try_enter().expect("释放后应能再进");
        assert_eq!(gate.count(), 2);
        drop(second);
        drop(third);
        assert_eq!(gate.count(), 0);
    }

    #[test]
    fn wait_idle_returns_zero_after_release() {
        let gate = Arc::new(InFlight::new(4));
        let guard = gate.try_enter().expect("名额");
        let waiter = Arc::clone(&gate);
        let handle =
            std::thread::spawn(move || waiter.wait_idle(std::time::Duration::from_millis(2000)));
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(guard);
        assert_eq!(handle.join().expect("等待线程"), 0, "释放后应等到 0");
    }

    #[test]
    fn wait_idle_times_out_with_remaining() {
        let gate = Arc::new(InFlight::new(4));
        let _guard = gate.try_enter().expect("名额");
        assert_eq!(gate.wait_idle(std::time::Duration::from_millis(30)), 1);
    }
}
