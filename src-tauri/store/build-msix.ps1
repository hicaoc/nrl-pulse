# =============================================================================
# NRL Pulse - Microsoft Store (MSIX) 打包脚本
# 用法（在项目根目录）:
#   powershell -ExecutionPolicy Bypass -File src-tauri/store/build-msix.ps1
#
# 可选参数:
#   -IdentityName  商店 Package/Identity/Name（Partner Center 预留名，如 12345Dev.NRLPulse）
#   -Publisher     证书主体/发布者（如 "CN=ABCD1234-..."，必须与商店和签名证书一致）
#   -Version       4 段版本号（默认取 tauri.conf.json + .0）
#   -Cert          签名证书 .pfx 路径（留空则不签名，由商店上传时代签）
#   -CertPassword  .pfx 密码
#   -SkipBuild     跳过 cargo/tauri 编译，直接用已有 release 产物
# =============================================================================
param(
  [string]$IdentityName = $env:MSIX_IDENTITY_NAME,
  [string]$Publisher    = $env:MSIX_PUBLISHER,
  [string]$PublisherDisplayName = $env:MSIX_PUBLISHER_DISPLAY_NAME,
  [string]$Version      = "",
  [string]$Cert         = $env:MSIX_CERT,
  [string]$CertPassword = $env:MSIX_CERT_PASSWORD,
  [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$root      = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$srcTauri  = Join-Path $root "src-tauri"
$releaseDir = Join-Path $srcTauri "target\release"
$stage     = Join-Path $srcTauri "target\msix-stage"
$outDir    = Join-Path $srcTauri "target\msix"

# ---- 定位 Windows SDK 的 makeappx / signtool ----
$sdkBin = "C:\Program Files (x86)\Windows Kits\10\bin"
function Find-Tool($name) {
  $c = Get-Command $name -ErrorAction SilentlyContinue
  if ($c) { return $c.Source }
  $cands = Get-ChildItem "$sdkBin\*\x64\$name" -ErrorAction SilentlyContinue |
           Sort-Object FullName -Descending | Select-Object -First 1
  if ($cands) { return $cands.FullName }
  throw "找不到 $name，请确认已安装 Windows 10/11 SDK"
}
$makeappx = Find-Tool "makeappx.exe"
$signtool = $null
if ($Cert) { $signtool = Find-Tool "signtool.exe" }

Write-Host "[msix] makeappx: $makeappx" -ForegroundColor Cyan

# ---- 版本号（MSIX 需要 4 段）----
if (-not $Version) {
  $conf = Get-Content (Join-Path $srcTauri "tauri.conf.json") -Raw | ConvertFrom-Json
  $v = $conf.version
  if ($v.Split('.').Count -lt 4) { $v = "$v.0" }
  $Version = $v
}

# ---- Identity / Publisher 校验 ----
if (-not $IdentityName) {
  $IdentityName = "NRLPulse"
  Write-Warning "未提供 -IdentityName，使用占位符 'NRLPulse'。上架前必须替换为 Partner Center 的 Package/Identity/Name。"
}
if (-not $Publisher) {
  $Publisher = "CN=NRL Pulse"
  Write-Warning "未提供 -Publisher，使用占位符 'CN=NRL Pulse'。上架前必须替换为开发者证书的发布者主题。"
}
if (-not $PublisherDisplayName) {
  $PublisherDisplayName = "NRL Pulse"
  Write-Warning "未提供 -PublisherDisplayName，使用默认 'NRL Pulse'。"
}

# ---- 编译（禁用 updater，由商店托管更新）----
if (-not $SkipBuild) {
  Write-Host "[msix] 编译 release（商店配置，禁用自动更新）..." -ForegroundColor Cyan
  Push-Location $root
  npm run tauri build -- --config src-tauri/tauri.store.json --no-bundle
  Pop-Location
}

$exe = Join-Path $releaseDir "nrl-pulse.exe"
if (-not (Test-Path $exe)) { throw "缺少 $exe，请先编译或去掉 -SkipBuild" }

# ---- 组装 staging 目录 ----
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage | Out-Null
New-Item -ItemType Directory -Path (Join-Path $stage "Assets") | Out-Null

# 主程序 + 运行时 dll
Copy-Item $exe $stage -Force
Get-ChildItem $releaseDir -Filter "*.dll" | ForEach-Object { Copy-Item $_.FullName $stage -Force }

# 图标 -> Assets
$icons = @{
  "Square44x44Logo.png"   = "Assets\Square44x44Logo.png"
  "Square150x150Logo.png" = "Assets\Square150x150Logo.png"
  "Square310x310Logo.png" = "Assets\Square310x310Logo.png"
  "StoreLogo.png"         = "Assets\StoreLogo.png"
}
$iconsDir = Join-Path $srcTauri "icons"
foreach ($k in $icons.Keys) {
  Copy-Item (Join-Path $iconsDir $k) (Join-Path $stage $icons[$k]) -Force
}

# AppxManifest.xml（替换占位符）
$manifest = Get-Content (Join-Path $srcTauri "store\AppxManifest.xml") -Raw -Encoding UTF8
$manifest = $manifest.Replace("__IDENTITY_NAME__", $IdentityName)
$manifest = $manifest.Replace("__PUBLISHER__", $Publisher)
$manifest = $manifest.Replace("__PUBLISHER_DISPLAY_NAME__", $PublisherDisplayName)
$manifest = $manifest.Replace("__VERSION__", $Version)
Set-Content -Path (Join-Path $stage "AppxManifest.xml") -Value $manifest -Encoding UTF8

# ---- makeappx pack ----
New-Item -ItemType Directory -Path $outDir -Force | Out-Null
$msixPath = Join-Path $outDir "NRL-Pulse_$($Version)_x64.msix"
if (Test-Path $msixPath) { Remove-Item $msixPath -Force }
& $makeappx pack /d $stage /p $msixPath /o
if ($LASTEXITCODE -ne 0) { throw "makeappx 打包失败" }
Write-Host "[msix] 已生成: $msixPath" -ForegroundColor Green

# ---- 可选签名 ----
if ($Cert -and $signtool) {
  if (Test-Path $Cert) {
    $sargs = @("sign","/fd","SHA256","/f",$Cert,"/tr","http://timestamp.digicert.com","/td","SHA256")
    if ($CertPassword) { $sargs += @("/p", $CertPassword) }
    $sargs += $msixPath
    & $signtool @sargs
    if ($LASTEXITCODE -ne 0) { throw "signtool 签名失败" }
    Write-Host "[msix] 已签名" -ForegroundColor Green
  } else {
    Write-Warning "证书不存在，跳过签名: $Cert"
  }
} else {
  Write-Warning "[msix] 未签名。上传到 Partner Center 时可让商店代签，或提供 -Cert 本地签名。"
}

Write-Host ""
Write-Host "============================================" -ForegroundColor Yellow
Write-Host " Identity Name : $IdentityName"
Write-Host " Publisher     : $Publisher"
Write-Host " PublisherDisp : $PublisherDisplayName"
Write-Host " Version       : $Version"
Write-Host " 输出          : $msixPath"
Write-Host "============================================" -ForegroundColor Yellow
