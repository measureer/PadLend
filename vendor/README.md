# vendor/

## scrcpy-server-v5.0.1.jar

- 来源：<https://github.com/Genymobile/scrcpy/releases/download/v5.0.1/scrcpy-server-v5.0.1>
- 上游文件 SHA-256：`764eb6f79811d5211fe9df341120882ba9994c7a61b897d7bf3fb662e53bc536`
- 本仓库文件 SHA-256：`d01277080ec973d1a5684e32ad15577fb40fc06cab4c3851e3ea4d4443d810ca`

**已重打包（代码未改）**：Android 17 的 `app_process`/ART 通过 `CLASSPATH`
加载压缩的 `classes.dex` 条目时会抛 `ClassNotFoundException`；因此把
`classes.dex` 以 **stored（不压缩）** 方式重新封包，其余条目保持原样。
重打包脚本见 `tools/repack_server_jar.py`：

```bash
python tools/repack_server_jar.py <官方 jar> vendor/scrcpy-server-v5.0.1.jar
```

更新 jar 后必须同步 `vendor/SHA256SUMS`（运行时完整性校验用）。
