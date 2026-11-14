PadLend（C:\Code\PadLend）约定：每个版本发版都要 git 提交（用户 m02174「以后每个版本都要提交」）；提交信息用英文 imperative 式 subject，正文说明根因与修法；一个版本一个提交，不要攒着不提交。
§
PadLend 发布节奏（用户 2026-11-06 指示）：小版本不要直接发 GitHub Release；先本地提交/构建，攒几个版本后再合并发布一个 release。
§
PadLend 踩坑记录在 docs/PITFALLS.md：修 bug 前先翻一遍避免重蹈覆辙，修完把新坑（根因+修法）补进去。
§
PadLend 开发机（Windows）已装 Rust 1.96.0（rustup，C:\Users\admin\.cargo\bin）和 VS 2022 Build Tools 17.14.45（MSVC 14.44 + Windows SDK 10.0.26100，C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools）；cargo check/build（MSVC）通过，需先运行 VsDevCmd.bat 或 Developer PowerShell。另有 GNU 工具链 + MinGW-w64 GCC 16.2.0（C:\Users\admin\msys64\mingw64\bin）作为备选。
§
PadLend 发布流程：不要下载/本地校验 GitHub Release 资产（用户 2026-11-14 m04016 明确指示「这一步可以跳过 以后不要做了」）；确认 Release 工作流成功、资产列表与大小即可。
§
PadLend Rust 侧（padlend-core，windows crate 0.62.2）：几何类型在 src/logic/geometry.rs（不是 core/），Win32 封装在 src/win/；MONITORINFOF_PRIMARY 属于 Win32::UI::WindowsAndMessaging（不在 Graphics::Gdi）；GetDpiForMonitor / SetProcessDpiAwarenessContext 在 Win32::UI::HiDpi，未开该 feature 时用 LoadLibraryW + GetProcAddress 动态解析。
§
PadLend 引擎现状（2026-11-14 起 Python 实现不再维护，只维护 Rust）：Rust 引擎在 padlend-core/（独立 crate，windows 0.62.2），26 个 RPC 方法已全部实现（1.6.0 起新增 devices.cancel）。外壳 src-tauri/src/bridge.rs 的 resolve_service 在“外壳 exe 所在目录”按 padlend-core-x86_64-pc-windows-msvc.exe → padlend-core.exe 找引擎，可用 PADLEND_SERVICE_CMD 覆盖。tools/build_core.py 只把 release 产物写到 src-tauri/binaries，而 dev/release 实际加载 src-tauri/target/{debug,release}/ 里的拷贝（旧文件会盖过新构建），更新引擎后必须同步拷到这两个目录或重新 tauri build；用户实测跑 src-tauri/target/release/padlend-desktop.exe。重新构建前先 `Get-Process padlend-core | Stop-Process -Force`，否则 exe 被占。桌面锁屏时 SendInput/SetCursorPos 返回 ERROR_ACCESS_DENIED，活体门禁用例需解锁后跑。
§
PadLend Rust 引擎 2026-11-14 修了三条真机问题（详见 docs/PITFALLS.md 第 10 节）：① 服务模式此前从未启动 RawInputSource（只有 --debug-raw 启动），表现为接管后平板指针一动不动，现由 engine 的 start_with + MousePacket→RawDelta 接线；② devices.connect 必须先用 Engine::set_requested_serial 把目标写回引擎配置，否则会话按配置里的旧端口重试、报连接超时；③ 诊断导出默认目录已从桌面改为下载。
§
PadLend 联调机上平板走 EasyTier 虚拟局域网：本机 clz-work 10.144.144.3、NAS clz-nas 10.144.144.1、平板 10.144.144.5:34739（config.yaml device.serial）。排查“连不上”先跑 `easytier-cli peer`（C:\Program Files\EasyTier\easytier-cli.exe）看平板在不在虚拟网里，再看 adb devices / adb mdns services；虚拟网不转发 mDNS 组播。
§
PadLend 1.6.0（提交 9199cd2，2026-11-14）修了三条：① rpc.rs 每个请求独立线程，devices.connect 不再堵死 stdin 循环（慢请求打 `[rpc] <method> 耗时 Nms`）；② 新增 devices.cancel + 前端「取消连接」，会话各阶段检查取消标记，取消落在 adb connect 阶段时不会再把会话拉起来，CONNECT_TIMEOUT_S 45→30；③ 音频前先读 ro.build.version.sdk，Android<11 只开控制通道；④ registry 合并换端口的重复记录（rename 合并、adopt_live_endpoint、load 时 merge_duplicates）。构建这条机器上没有 npm，用 node_modules/@tauri-apps/cli/tauri.js build --config '{"build":{"beforeBuildCommand":""}}'，前端先用 node vite 构建到 frontend-dist。
