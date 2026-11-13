//! PadLend Rust sidecar（B2 迁移）。
//!
//! 与 Python 版 `service/server.py` 共用同一份传输契约：
//! 请求 `{"id":1,"method":"app.bootstrap","params":{}}`，响应
//! `{"id":1,"ok":true,"result":...}` / `{"id":1,"ok":false,"error":"..."}`，
//! 事件 `{"event":"state","data":...}`，每行一个 JSON，UTF-8，\n 结尾。
//!
//! 子命令：
//! * `--debug-raw`：与 `python main.py --debug-raw` 等价，用来做 M0 计数对拍；
//! * `--serve`（默认）：NDJSON RPC 服务，供 Tauri 外壳调用。

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use padlend_core::{rpc, win, VERSION};

#[derive(Debug, Default)]
struct Args {
    debug_raw: bool,
    serve: bool,
    duration_s: f64,
    json: bool,
    simulate: bool,
    no_hooks: bool,
    log_level: String,
    config: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        log_level: "info".to_string(),
        ..Default::default()
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--debug-raw" => args.debug_raw = true,
            "--serve" => args.serve = true,
            "--json" => args.json = true,
            "--simulate" => args.simulate = true,
            "--no-hooks" => args.no_hooks = true,
            "--version" | "-V" => {
                println!("padlend-core {VERSION}");
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "padlend-core {VERSION}\n\n\
                     --debug-raw            统计 Raw Input 事件（对拍 python main.py --debug-raw）\n\
                     --duration <秒>        运行时长，0=直到 Ctrl+C\n\
                     --json                 结束时输出一行 JSON 汇总\n\
                     --serve                NDJSON RPC 服务（默认）\n\
                     --simulate             使用内存假平板\n\
                     --no-hooks             不安装 Raw Input / 键盘钩子\n\
                     --config <路径>        指定 config.yaml\n\
                     --log-level <级别>     debug|info|warning|error"
                );
                std::process::exit(0);
            }
            "--duration" => {
                let value = it.next().ok_or("--duration 缺少参数")?;
                args.duration_s = value.parse().map_err(|_| format!("--duration 非法：{value}"))?;
            }
            "--config" => {
                args.config = Some(it.next().ok_or("--config 缺少参数")?);
            }
            "--log-level" => {
                args.log_level = it.next().ok_or("--log-level 缺少参数")?;
            }
            other => return Err(format!("未知参数：{other}")),
        }
    }
    if !args.debug_raw {
        args.serve = true;
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("[error] {err}");
            return ExitCode::from(2);
        }
    };

    // PITFALLS 5.1：DPI 感知必须早于任何窗口创建，否则遮罩矩形与坐标换算全错。
    // 失败不致命（96 DPI 单屏仍可用），但要在 stderr 留证据。
    if !win::dpi::set_dpi_awareness() {
        eprintln!("[warn] Per-Monitor V2 DPI 感知设置失败；高 DPI 下遮罩可能错位");
    }

    if args.debug_raw {
        return match run_debug_raw(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("[error] {err}");
                ExitCode::from(1)
            }
        };
    }

    let _ = &args.log_level;
    // 与 Python main.py 一致：--simulate 或 PADLEND_SIMULATE 为真即模拟。
    // **默认必须是真实设备**——生产与调试行为不能靠构建类型分叉，否则本地调试时
    // 一切"正常"而发布版连的是真平板。门禁与演示由 tools/rpc_trace.py 显式设
    // PADLEND_SIMULATE=1。
    let simulate = args.simulate || simulate_requested();
    match rpc::serve(rpc::ServeOptions {
        simulate,
        no_hooks: args.no_hooks,
        config_path: args.config.as_ref().map(PathBuf::from),
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[error] {err}");
            ExitCode::from(1)
        }
    }
}

/// 是否请求模拟模式：只看环境变量，缺省为 false（真实设备）。
fn simulate_requested() -> bool {
    match std::env::var("PADLEND_SIMULATE") {
        Ok(value) => matches!(
            value.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

/// 等价于 `python main.py --debug-raw`：每秒打印一次计数，格式保持一致便于 diff。
fn run_debug_raw(args: &Args) -> Result<(), String> {
    let source = win::rawinput::RawInputSource::start()?;
    if !args.json {
        println!("Raw Input 已启动；移动鼠标/滚轮，Ctrl+C 退出");
    }

    let started = Instant::now();
    let mut last = started;
    let mut last_count = 0u64;
    loop {
        std::thread::sleep(Duration::from_millis(1000));
        let now = Instant::now();
        let count = source.count();
        let delta = count - last_count;
        last_count = count;
        if !args.json {
            println!("[raw] {} events / {:.1}s", delta, (now - last).as_secs_f64());
        }
        last = now;
        if args.duration_s > 0.0 && started.elapsed().as_secs_f64() >= args.duration_s {
            break;
        }
    }

    let stats = source.stats();
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "events": stats.events,
                "moves": stats.moves,
                "wheel": stats.wheel,
                "buttons": stats.buttons,
                "absolute": stats.absolute,
                "seconds": (Instant::now() - started).as_secs_f64(),
            })
        );
    }
    Ok(())
}
