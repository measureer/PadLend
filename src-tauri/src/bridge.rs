//! Python 业务 sidecar 的异步 JSON-RPC 客户端。
//!
//! * 命令调用走 `stdin`，响应与事件走 `stdout`（NDJSON）；
//! * `call()` 是 async 的，等待期间不占用任何线程，WebView 永远不被阻塞；
//! * 服务进程里发生的事件直接转发为 Tauri `emit`，前端只需订阅一次；
//! * 退出时先发 `app.shutdown` 让 Python 优雅释放 UHID / 遮罩 / 指针速度，
//!   超时才强杀。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>;

pub struct Bridge {
    next_id: AtomicU64,
    pending: Pending,
    stdin: Mutex<ChildStdin>,
    child: Mutex<Option<Child>>,
}

impl Bridge {
    pub fn start(app: AppHandle) -> Result<Arc<Self>, String> {
        let (program, args, cwd) = resolve_service();
        let mut command = Command::new(&program);
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8");
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        // sidecar 用 Python 控制台子系统保留 stdio 管道，但不弹黑框。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("无法启动业务服务 {program}：{e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "业务服务 stdin 不可用".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "业务服务 stdout 不可用".to_string())?;

        let bridge = Arc::new(Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            stdin: Mutex::new(stdin),
            child: Mutex::new(Some(child)),
        });

        let reader_bridge = Arc::clone(&bridge);
        let reader_app = app.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                if let Some(id) = value.get("id").and_then(Value::as_u64) {
                    if id == 0 {
                        continue; // 一次性通知（如 shutdown）没有等待者
                    }
                    let result = if value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                        Ok(value.get("result").cloned().unwrap_or(Value::Null))
                    } else {
                        Err(value
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("业务服务未返回错误详情")
                            .to_string())
                    };
                    if let Some(sender) = reader_bridge.pending.lock().unwrap().remove(&id) {
                        let _ = sender.send(result);
                    }
                } else if let Some(event) = value.get("event").and_then(Value::as_str) {
                    let name = normalize_event(event);
                    let data = value.get("data").cloned().unwrap_or(Value::Null);
                    let _ = reader_app.emit(&name, data);
                }
            }
            // 进程退出：把仍在等待的请求全部唤醒，避免前端 Promise 永远挂起。
            let mut pending = reader_bridge.pending.lock().unwrap();
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err("业务服务已退出".to_string()));
            }
            let _ = reader_app.emit("service:disconnected", json!({}));
        });

        Ok(bridge)
    }

    /// 调用一个业务方法。`write` 只在同步代码块里持锁，绝不跨 `.await`。
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let request = json!({ "id": id, "method": method, "params": params });
        let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        line.push('\n');
        {
            let mut stdin = self.stdin.lock().unwrap();
            stdin
                .write_all(line.as_bytes())
                .map_err(|e| format!("写入业务服务失败：{e}"))?;
            stdin.flush().map_err(|e| format!("刷新业务服务失败：{e}"))?;
        }

        match tokio::time::timeout(Duration::from_secs(120), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("业务服务连接已断开".to_string()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("业务方法 {method} 超时"))
            }
        }
    }

    /// 请求优雅退出；不阻塞调用方（应用退出路径）。
    pub fn shutdown(&self) {
        let request = r#"{"id":0,"method":"app.shutdown","params":{}}"#.to_string();
        {
            let mut stdin = self.stdin.lock().unwrap();
            let _ = stdin.write_all(request.as_bytes());
            let _ = stdin.write_all(b"\n");
            let _ = stdin.flush();
        }
        if let Some(mut child) = self.child.lock().unwrap().take() {
            std::thread::spawn(move || {
                for _ in 0..40 {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
            });
        }
    }
}

fn normalize_event(name: &str) -> String {
    name.replace('.', ":")
}

/// 解析 sidecar 启动方式：环境变量 → 随包 exe → 开发期 Python 源码。
fn resolve_service() -> (String, Vec<String>, Option<PathBuf>) {
    if let Ok(raw) = env::var("PADLEND_SERVICE_CMD") {
        let mut parts = raw.split_whitespace().map(str::to_string);
        if let Some(program) = parts.next() {
            return (program, parts.collect(), None);
        }
    }

    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let candidates = [
        "padlend-service-x86_64-pc-windows-msvc.exe",
        "padlend-service.exe",
        "resources/padlend-service.exe",
        "binaries/padlend-service-x86_64-pc-windows-msvc.exe",
    ];
    for name in candidates {
        let path = exe_dir.join(name);
        if path.is_file() {
            let mut args = Vec::new();
            if env::var("PADLEND_SIMULATE").is_ok() {
                args.push("--simulate".to_string());
            }
            return (path.to_string_lossy().to_string(), args, Some(exe_dir.clone()));
        }
    }

    let python = env::var("PADLEND_PYTHON").unwrap_or_else(|_| "python".to_string());
    let mut args = vec!["-m".to_string(), "service.server".to_string()];
    if env::var("PADLEND_SIMULATE").is_ok() {
        args.push("--simulate".to_string());
    }
    let root = env::var("PADLEND_PROJECT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            if cwd.join("service/server.py").is_file() {
                cwd
            } else {
                cwd.parent().map(PathBuf::from).unwrap_or(cwd)
            }
        });
    // 打包运行时，业务模块位于 resource_dir；开发时位于项目根。
    let module_root = if root.join("service/server.py").is_file() {
        root
    } else {
        let resource = exe_dir.join("resources");
        if resource.join("service/server.py").is_file() { resource } else { root }
    };
    (python, args, Some(module_root))
}
