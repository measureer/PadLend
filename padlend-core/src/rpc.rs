//! NDJSON 传输层（对应 `service/server.py`）。
//!
//! 契约：stdin 每行一个请求对象；stdout 每行一个响应或事件。
//! **stdout 只承载协议数据**，人类可读日志一律走 stderr —— 否则外壳的解析会崩。

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::methods::{dispatch, Reply};
use crate::VERSION;

pub fn serve() -> Result<(), String> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // 与 Python 版一致：启动完成后先发 ready 事件，外壳据此认为通道可用。
    write_line(&mut out, &json!({"event": "ready", "data": {"version": VERSION, "engine": "rust"}}))?;

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
                eprintln!("[rpc] 非法请求行：{err}");
                continue;
            }
        };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        match dispatch(&method, &params) {
            Reply::Ok(result) => {
                write_line(&mut out, &json!({"id": id, "ok": true, "result": result}))?;
            }
            Reply::Error(err) => {
                eprintln!("[rpc] {method} 失败：{err}");
                write_line(&mut out, &json!({"id": id, "ok": false, "error": err}))?;
            }
            Reply::Shutdown(result) => {
                write_line(&mut out, &json!({"id": id, "ok": true, "result": result}))?;
                break;
            }
        }
    }

    let _ = write_line(&mut out, &json!({"event": "service.stopped", "data": {}}));
    Ok(())
}

fn write_line<W: Write>(out: &mut W, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string(value).map_err(|err| format!("序列化失败：{err}"))?;
    writeln!(out, "{text}").map_err(|err| format!("写 stdout 失败：{err}"))?;
    out.flush().map_err(|err| format!("flush 失败：{err}"))
}
