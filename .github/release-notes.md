# PadLend 2.0.5

## 修复

- **平板越用越卡、无线调试自己掉线**：朝向轮询从 2.0.2 起每 2 秒跑一次完整的
  `dumpsys window displays`（实测 851ms～13.4s），2 秒的间隔根本追不上，超时被杀后
  平板上的 dump 还在跑，几轮就叠成好几份抢 WindowManager 锁。现在改成只 grep
  `dumpsys input` 的 Viewport 行（65～150ms），周期 15 秒、失败翻倍退避。

## 说明

- **无线调试换端口后不用再重启应用**：adbd 重启会换端口，现在引擎发现新地址后直接重建会话自动接上。

- 安装包：`PadLend-2.0.5-setup.exe`（NSIS）；免安装：`PadLend-2.0.5-win64.zip`。
- 校验值见 `SHA256SUMS.txt`。

