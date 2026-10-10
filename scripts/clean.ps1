<#
.SYNOPSIS
    清理 desktop-pet 的构建产物，回收磁盘空间。

.DESCRIPTION
    Rust/Tauri 项目的 target/ 目录会随反复构建持续膨胀且从不自动回收。
    实测该项目曾累积到 17.2 GB，其中 target/debug/incremental 一项就占 6.5 GB
    （incremental 的会话目录 s-<hash> 只增不删）。

    本脚本提供分级清理，默认是最温和的一档。

.PARAMETER IncrementalOnly
    只删除 target/debug/incremental。
    回收最多空间的同时保留全部依赖编译结果 —— 下次构建只会重编你自己的代码，
    依赖无需重编。这是日常最推荐的清理方式。

.PARAMETER Target
    删除整个 src-tauri/target。
    回收最彻底，但下次构建需要完整重编 551 个依赖（约十几分钟）。

.PARAMETER All
    额外删除 dist/ 和 node_modules/。相当于回到刚 clone 的状态。

.PARAMETER WhatIf
    只报告各目录体积，不实际删除。

.EXAMPLE
    .\scripts\clean.ps1 -WhatIf
    看看能回收多少空间。

.EXAMPLE
    .\scripts\clean.ps1 -IncrementalOnly
    日常清理，保留依赖缓存。

.EXAMPLE
    .\scripts\clean.ps1 -All
    彻底重置。
#>
[CmdletBinding()]
param(
    [switch]$IncrementalOnly,
    [switch]$Target,
    [switch]$All,
    [switch]$WhatIf
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)

function Get-DirSize {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return 0 }
    $sum = (Get-ChildItem -LiteralPath $Path -Recurse -File -Force -ErrorAction SilentlyContinue |
            Measure-Object -Property Length -Sum).Sum
    if ($null -eq $sum) { return 0 }
    return $sum
}

function Format-Size {
    param([double]$Bytes)
    if ($Bytes -ge 1GB) { return ('{0:N2} GB' -f ($Bytes / 1GB)) }
    if ($Bytes -ge 1MB) { return ('{0:N2} MB' -f ($Bytes / 1MB)) }
    return ('{0:N0} KB' -f ($Bytes / 1KB))
}

# 目标目录（按清理档位选择）
$targets = @()

if ($All -or $Target) {
    $targets += Join-Path $Root 'src-tauri\target'
} elseif ($IncrementalOnly) {
    $targets += Join-Path $Root 'src-tauri\target\debug\incremental'
    $targets += Join-Path $Root 'src-tauri\target\release\incremental'
} else {
    # 默认：最温和的一档
    $targets += Join-Path $Root 'src-tauri\target\debug\incremental'
    $targets += Join-Path $Root 'src-tauri\target\release\incremental'
}

if ($All) {
    $targets += Join-Path $Root 'dist'
    $targets += Join-Path $Root 'node_modules'
}

# 检查是否有进程占用 target
$busy = Get-Process -ErrorAction SilentlyContinue |
        Where-Object { $_.ProcessName -match '^(desktop-pet|cargo|rustc)$' }
if ($busy -and -not $WhatIf) {
    Write-Warning "检测到可能占用 target 的进程："
    $busy | ForEach-Object { Write-Warning ("  {0} (PID {1})" -f $_.ProcessName, $_.Id) }
    Write-Warning "请先关闭 tauri dev / 编辑器里的 cargo check，再运行本脚本。"
    exit 1
}

$total = 0
$plan = @()
foreach ($t in $targets) {
    $size = Get-DirSize $t
    if ($size -gt 0) {
        $plan += [PSCustomObject]@{ Path = $t; Size = $size }
        $total += $size
    }
}

if ($plan.Count -eq 0) {
    Write-Host "没有需要清理的内容，项目已经是干净状态。" -ForegroundColor Green
    exit 0
}

Write-Host ""
Write-Host "待清理：" -ForegroundColor Cyan
foreach ($p in $plan) {
    $rel = $p.Path.Replace("$Root\", '')
    Write-Host ("  {0,-12} {1}" -f (Format-Size $p.Size), $rel)
}
Write-Host ("  {0,-12} {1}" -f ('-' * 10), '')
Write-Host ("  {0,-12} 合计" -f (Format-Size $total)) -ForegroundColor Yellow
Write-Host ""

if ($WhatIf) {
    Write-Host "（-WhatIf 模式，未删除任何文件）" -ForegroundColor DarkGray
    exit 0
}

foreach ($p in $plan) {
    try {
        Remove-Item -LiteralPath $p.Path -Recurse -Force -ErrorAction Stop
        Write-Host ("已删除 {0}" -f $p.Path.Replace("$Root\", '')) -ForegroundColor Green
    } catch {
        Write-Warning ("删除失败 {0}: {1}" -f $p.Path, $_.Exception.Message)
    }
}

Write-Host ""
Write-Host ("完成，共回收 {0}" -f (Format-Size $total)) -ForegroundColor Green
