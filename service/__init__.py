"""PadLend 业务服务层。

Tauri 外壳通过 NDJSON（stdin/stdout）与本进程通信；本包只做协议适配，
所有业务逻辑仍来自 ``core/``、``tablet/``、``win/`` 与 ``config.py``。
1.1.0 起仓库不再包含 PySide6/Qt 代码，sidecar 与桌面外壳都不需要 Qt 运行时。
"""
