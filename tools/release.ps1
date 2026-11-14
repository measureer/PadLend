<#
.SYNOPSIS
    PadLend 发布脚本：前端 → Rust 引擎 → Tauri 外壳 → NSIS 安装包 → 免安装 zip → SHA256。

.DESCRIPTION
    1.7.0 起发版不再涉及 Python：不再打 PyInstaller sidecar，也不再依赖 .venv。
    版本号以 src-tauri/tauri.conf.json 为唯一来源（安装器与 RPC ready 事件都用它）。

    dist/ 里旧的 PadLend-* 产物会先被清掉：之前 make_sha256 会把 1.1.x 遗留文件
    一起算进 SHA256SUMS.txt，导致校验和对应着一堆早已过期的安装包。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File tools\release.ps1
    powershell -ExecutionPolicy Bypass -File tools\release.ps1 -SkipTests -NoBundle
#>
[CmdletBinding()]
param(
    [switch]$SkipTests,
    # 只编译不打包（等价于 tauri build --no-bundle），用于推送 tag 前验证能编过。
    [switch]$NoBundle
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$Dist = Join-Path $Root "dist"
$FrontendDist = Join-Path $Root "frontend-dist"
$Binaries = Join-Path $Root "src-tauri\binaries"
$ReleaseDir = Join-Path $Root "src-tauri\target\release"
$AppName = "PadLend"
$Triple = "x86_64-pc-windows-msvc"

function Write-Step([string]$Message) {
    Write-Host "[release] $Message" -ForegroundColor Cyan
}
function Write-Fail([string]$Message) {
    Write-Host "[release] ERROR: $Message" -ForegroundColor Red
    exit 1
}
function Invoke-Checked([scriptblock]$Command, [string]$What) {
    & $Command
    if ($LASTEXITCODE -ne 0) { Write-Fail "$What 失败（exit $LASTEXITCODE）" }
}

$conf = Get-Content (Join-Path $Root "src-tauri\tauri.conf.json") -Raw | ConvertFrom-Json
$Version = $conf.version
if (-not $Version) { Write-Fail "无法从 tauri.conf.json 读取 version" }
Write-Step "版本：$Version"

# ---- 1. 前端 -------------------------------------------------------------
if (-not (Test-Path (Join-Path $Root "node_modules"))) {
    Invoke-Checked { npm ci } "npm ci"
}
Invoke-Checked { npm run build } "前端构建"
if (-not (Test-Path (Join-Path $FrontendDist "index.html"))) {
    Write-Fail "frontend-dist\index.html 不存在"
}

# ---- 2. 引擎 -------------------------------------------------------------
& (Join-Path $PSScriptRoot "build-core.ps1") -Profile release
if (-not (Test-Path (Join-Path $Binaries "padlend-core-$Triple.exe"))) {
    Write-Fail "padlend-core 未生成"
}

# ---- 3. 测试 -------------------------------------------------------------
if (-not $SkipTests) {
    Invoke-Checked { cargo test --manifest-path (Join-Path $Root "padlend-core\Cargo.toml") } "引擎测试"
    Invoke-Checked { npx vitest run } "前端测试"
}

# ---- 4. 外壳与安装器 -----------------------------------------------------
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Step "本机没有 cargo，跳过 Tauri 外壳（CI 会构建并发布安装包）"
    exit 0
}
if ($NoBundle) {
    Invoke-Checked { npx tauri build --no-bundle } "tauri build"
    Write-Step "完成（--no-bundle）"
    exit 0
}
Invoke-Checked { npx tauri build } "tauri build"

# ---- 5. 产物 -------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $Dist | Out-Null
Get-ChildItem $Dist -File | Where-Object { $_.Name -like "$AppName-*" } | ForEach-Object {
    Write-Step "清理旧产物：$($_.Name)"
    Remove-Item $_.FullName -Force
}

$installerDir = Join-Path $ReleaseDir "bundle\nsis"
# 必须按版本号挑：bundle\nsis 里会留着历史版本的安装器（1.6.0 曾把 1.6.0 的包当成新包）。
$installers = @(Get-ChildItem $installerDir -Filter ("PadLend_" + $Version + "_*-setup.exe") -ErrorAction SilentlyContinue)
if ($installers.Count -eq 0) {
    $installers = @(Get-ChildItem $installerDir -Filter "PadLend_*-setup.exe" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1)
}
if ($installers.Count -eq 0) { Write-Fail "没有找到 NSIS 安装器：$installerDir" }
$installer = $installers[0]
if ($installer.Name -notlike ("PadLend_" + $Version + "_*")) {
    Write-Fail "安装器版本与 $Version 不符：$($installer.Name)"
}
Copy-Item $installer.FullName (Join-Path $Dist "$AppName-$Version-setup.exe") -Force
Write-Step ("安装器：{0}（{1:N2} MB）" -f $installer.Name, ($installer.Length / 1MB))

# 免安装 zip：exe + 引擎 + vendor + 文档与许可证。
$stage = Join-Path $Dist "_stage"
$appDir = Join-Path $stage $AppName
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $appDir | Out-Null

$desktop = Join-Path $ReleaseDir "padlend-desktop.exe"
if (-not (Test-Path $desktop)) { Write-Fail "找不到 $desktop" }
Copy-Item $desktop (Join-Path $appDir "$AppName.exe") -Force
Copy-Item (Join-Path $Binaries "padlend-core-$Triple.exe") (Join-Path $appDir "padlend-core.exe") -Force

$jar = Join-Path $Root "vendor\scrcpy-server-v5.0.1.jar"
if (Test-Path $jar) {
    New-Item -ItemType Directory -Force -Path (Join-Path $appDir "vendor") | Out-Null
    Copy-Item $jar (Join-Path $appDir "vendor\$([IO.Path]::GetFileName($jar))") -Force
}
$adbSrc = Join-Path $Root "vendor\adb\platform-tools"
if (Test-Path $adbSrc) {
    Copy-Item $adbSrc (Join-Path $appDir "vendor\adb\platform-tools") -Recurse -Force
}
foreach ($name in @("README.md", "LICENSE", "THIRD_PARTY_NOTICES.md")) {
    if (Test-Path (Join-Path $Root $name)) { Copy-Item (Join-Path $Root $name) (Join-Path $appDir $name) -Force }
}
$licenses = Join-Path $Root "LICENSES"
if (Test-Path $licenses) {
    Copy-Item $licenses (Join-Path $appDir "LICENSES") -Recurse -Force
}

$zip = Join-Path $Dist "$AppName-$Version-win64.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip -CompressionLevel Optimal
Remove-Item $stage -Recurse -Force
Write-Step ("免安装包：{0}（{1:N2} MB）" -f [IO.Path]::GetFileName($zip), ((Get-Item $zip).Length / 1MB))

# ---- 6. 校验和 -----------------------------------------------------------
$files = @(Get-ChildItem $Dist -File | Where-Object { $_.Name -like "$AppName-*" } | Sort-Object Name)
$lines = foreach ($file in $files) {
    $hash = (Get-FileHash $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Step ("sha256 {0}…  {1}" -f $hash.Substring(0, 16), $file.Name)
    "$hash  $($file.Name)"
}
$lines | Set-Content -Path (Join-Path $Dist "SHA256SUMS.txt") -Encoding utf8

Write-Step "完成：$AppName $Version"
Get-ChildItem $Dist -File | Select-Object Name, Length | Format-Table -AutoSize
