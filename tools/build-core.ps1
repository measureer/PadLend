<#
.SYNOPSIS
    编译 Rust 业务引擎 padlend-core，并把产物放到 Tauri 期望的位置。

.DESCRIPTION
    1.7.0 起 Python sidecar 已移除，安装包与外壳只带 padlend-core。
    Tauri 的 externalBin 要求源文件叫 padlend-core-<target-triple>.exe，
    而手工运行 src-tauri/target/{release,debug}/padlend-desktop.exe 时用的是
    该目录里的 padlend-core.exe 副本——不刷新它会继续跑旧引擎，所以两处都写。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File tools\build-core.ps1
    powershell -ExecutionPolicy Bypass -File tools\build-core.ps1 -Profile debug
#>
[CmdletBinding()]
param(
    [ValidateSet("release", "debug")]
    [string]$Profile = "release",
    # 引擎正在被运行中的 PadLend 使用时会被锁住；默认先结束它。
    [switch]$KeepRunning
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$CoreDir = Join-Path $Root "padlend-core"
$Binaries = Join-Path $Root "src-tauri\binaries"
$Triple = "x86_64-pc-windows-msvc"
$OutName = "padlend-core-$Triple.exe"
$CargoProfile = if ($Profile -eq "debug") { "dev" } else { "release" }

function Write-Step([string]$Message) {
    Write-Host "[build-core] $Message" -ForegroundColor Cyan
}

# cargo 不在 PATH 时补上 rustup 的默认安装位置。
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoHome = Join-Path $env:USERPROFILE ".cargo\bin"
    if (Test-Path (Join-Path $cargoHome "cargo.exe")) {
        $env:PATH = "$cargoHome;$env:PATH"
    }
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "找不到 cargo：请先安装 Rust（rustup）后重开终端。"
}

# 没有 MSVC 环境时用 vswhere 找到 Build Tools 并加载 Developer 环境。
if (-not $env:VSCMD_VER) {
    $vswhere = Join-Path ([Environment]::GetFolderPath("ProgramFilesX86")) "Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $install = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($install) {
            $devShell = Join-Path $install "Common7\Tools\Launch-VsDevShell.ps1"
            if (Test-Path $devShell) {
                Write-Step "加载 MSVC 环境：$install"
                & $devShell -Arch amd64 -HostArch amd64 -SkipAutomaticLocation | Out-Null
            }
        }
    }
}

if (-not $KeepRunning) {
    Get-Process padlend-core -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Step "结束占用中的 padlend-core（PID $($_.Id)）"
        Stop-Process -Id $_.Id -Force
    }
}

Write-Step "cargo build --profile $CargoProfile"
& cargo build --manifest-path (Join-Path $CoreDir "Cargo.toml") --profile $CargoProfile
if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（exit $LASTEXITCODE）" }

$built = Join-Path $CoreDir "target\$Profile\padlend-core.exe"
if (-not (Test-Path $built)) { throw "构建产物不存在：$built" }

New-Item -ItemType Directory -Force -Path $Binaries | Out-Null
Copy-Item $built (Join-Path $Binaries $OutName) -Force
Write-Step ("产物：src-tauri\binaries\{0}（{1:N2} MB）" -f $OutName, ((Get-Item $built).Length / 1MB))

# 手工运行或 tauri build 复用的副本也必须同步，否则新引擎被旧文件盖过。
foreach ($dir in @("release", "debug")) {
    $target = Join-Path $Root "src-tauri\target\$dir"
    if (Test-Path $target) {
        Copy-Item $built (Join-Path $target "padlend-core.exe") -Force
        Write-Step "同步：src-tauri\target\$dir\padlend-core.exe"
    }
}
