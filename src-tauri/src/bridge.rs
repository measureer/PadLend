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
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
}

impl Bridge {
    pub fn start(app: AppHandle, audio_port: u16) -> Result<Arc<Self>, String> {
        let (program, args, cwd) = resolve_service();
        let mut command = Command::new(&program);
        // 业务服务的 stderr 是唯一的诊断入口（Rust sidecar 用它打日志），
        // 不能丢进 null：落到 %LOCALAPPDATA%\PadLend\service.log，用户可直接回传。
        let log_path = env::var("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("PadLend").join("service.log"))
            .unwrap_or_else(|_| std::env::temp_dir().join("padlend-service.log"));
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let stderr = std::fs::File::create(&log_path)
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8")
            // 父进程看门狗：外壳崩溃后 sidecar 必须自行 teardown 退出，
            // 否则它持有的全屏遮罩会留在桌面上（1.2.0 缺陷 A 的失效安全）。
            .env("PADLEND_PARENT_PID", std::process::id().to_string())
            // 0 表示音频 sink 没起来；Python 会把它当成“只读掉音频、不播放”。
            .env("PADLEND_AUDIO_PORT", audio_port.to_string());
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
            stdin: Mutex::new(Some(stdin)),
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
            let mut guard = self.stdin.lock().unwrap();
            let Some(stdin) = guard.as_mut() else {
                return Err("业务服务已退出".to_string());
            };
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

    /// 请求优雅退出并等待 sidecar 收尾。
    ///
    /// 1.1.2 把等待丢进后台线程后立即返回：Tauri 主进程随即退出，后台线程
    /// 被一起终止，Python 的遮罩/ClipCursor/UHID/adb 清理可能只做了一半。
    /// 退出路径必须等待，超时才强杀；sidecar 自己也有父进程看门狗兜底。
    /// 上限 8s：遮罩释放最坏会等专用线程 5s，4s 会在它完成前就强杀。
    pub fn shutdown(&self) {
        let request = r#"{"id":0,"method":"app.shutdown","params":{}}"#.to_string();
        {
            let mut guard = self.stdin.lock().unwrap();
            if let Some(mut stdin) = guard.take() {
                let _ = stdin.write_all(request.as_bytes());
                let _ = stdin.write_all(b"\n");
                let _ = stdin.flush();
            }
            // 关掉 stdin 让 Python 的读循环立即 EOF。只发 app.shutdown 不行：
            // 它把 stopping 置位，但主线程还阻塞在 sys.stdin 上，进程不会退出，
            // Rust 只能等满 8s 再强杀。
        }
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child {
            for _ in 0..80 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let pid = child.id();
            // PyInstaller onefile 会再派生一个真正的 Python 子进程；
            // 只 kill 外层父进程会留下它。必须先按进程树强杀，再 wait：
            // 先 kill 父进程的话，taskkill 可能已经找不到这棵树。
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                let pid_arg = pid.to_string();
                let _ = Command::new("taskkill")
                    .args(["/PID", pid_arg.as_str(), "/T", "/F"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();
            }
            #[cfg(not(windows))]
            {
                let _ = pid;
            }
            let _ = child.kill();
            let _ = child.wait();
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
    // A/B 切换点：把 padlend-core.exe 放进可执行目录即切到 Rust 引擎，删掉即回退到
    // Python sidecar（两者的 NDJSON 契约完全相同，见 padlend-core/src/rpc.rs）。
    // 安装包里同时带两个实现；PADLEND_ENGINE=python 可在不删文件的情况下强制回退。
    let prefer_rust = env::var("PADLEND_ENGINE")
        .map(|value| !value.trim().eq_ignore_ascii_case("python"))
        .unwrap_or(true);
    let mut candidates: Vec<&str> = Vec::new();
    if prefer_rust {
        candidates.extend([
            "padlend-core-x86_64-pc-windows-msvc.exe",
            "padlend-core.exe",
            "resources/padlend-core.exe",
        ]);
    }
    candidates.extend([
        "padlend-service-x86_64-pc-windows-msvc.exe",
        "padlend-service.exe",
        "resources/padlend-service.exe",
        "binaries/padlend-service-x86_64-pc-windows-msvc.exe",
    ]);
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
