# MisebanAI Camera Agent — Windows Installer
# Usage: irm https://misebanai.com/install.ps1 | iex
#Requires -Version 5.0

$ErrorActionPreference = "Stop"
$REPO = "yukihamada/miseban-ai"
$InstallDir = "$env:LOCALAPPDATA\MisebanAI"
$ServiceName = "miseban-agent"

Write-Host "=== MisebanAI Camera Agent Installer ===" -ForegroundColor Cyan
Write-Host ""

# Detect architecture
$Arch = if ([System.Environment]::Is64BitOperatingSystem) {
    if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x86_64" }
} else {
    Write-Host "32-bit Windows is not supported." -ForegroundColor Red; exit 1
}

$Artifact = "miseban-agent-windows-$Arch.zip"

# Get latest version
Write-Host "[1/4] Fetching latest version..."
try {
    $Release = Invoke-RestMethod "https://api.github.com/repos/$REPO/releases/latest"
    $Version = $Release.tag_name
} catch {
    $Version = $env:MISEBAN_VERSION ?? "v1.0.0"
}
Write-Host "    Version: $Version"

# Check ffmpeg
Write-Host "[2/4] Checking dependencies..."
if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) {
    Write-Host "    ffmpeg not found. Installing via winget..."
    try {
        winget install --id Gyan.FFmpeg -e --silent 2>$null
        Write-Host "    ffmpeg installed OK"
    } catch {
        Write-Host "    WARNING: Could not auto-install ffmpeg." -ForegroundColor Yellow
        Write-Host "    Download manually: https://ffmpeg.org/download.html" -ForegroundColor Yellow
        Write-Host "    Add ffmpeg.exe to your PATH, then re-run this installer."
    }
} else {
    Write-Host "    ffmpeg: OK"
}

# Download binary
Write-Host "[3/4] Downloading $Artifact..."
$Url = "https://github.com/$REPO/releases/download/$Version/$Artifact"
$TmpDir = New-TemporaryFile | ForEach-Object { Remove-Item $_; New-Item -Type Directory $_ }
$ZipPath = "$TmpDir\$Artifact"
Invoke-WebRequest $Url -OutFile $ZipPath -UseBasicParsing
Expand-Archive $ZipPath -DestinationPath $TmpDir

# Install binary
Write-Host "[4/4] Installing to $InstallDir..."
New-Item -ItemType Directory -Force $InstallDir | Out-Null
Copy-Item "$TmpDir\miseban-agent.exe" "$InstallDir\miseban-agent.exe" -Force
Remove-Item $TmpDir -Recurse -Force

# Add to PATH
$CurrentPath = [System.Environment]::GetEnvironmentVariable("PATH", "User")
if ($CurrentPath -notlike "*$InstallDir*") {
    [System.Environment]::SetEnvironmentVariable("PATH", "$CurrentPath;$InstallDir", "User")
    Write-Host "    Added $InstallDir to PATH"
}

# Register as Windows Service (optional, requires admin)
$IsAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]"Administrator")
if ($IsAdmin) {
    Write-Host "    Registering Windows service..."
    $ExePath = "$InstallDir\miseban-agent.exe"
    if (Get-Service $ServiceName -ErrorAction SilentlyContinue) {
        Stop-Service $ServiceName -ErrorAction SilentlyContinue
        sc.exe delete $ServiceName | Out-Null
    }
    New-Service -Name $ServiceName -BinaryPathName $ExePath -DisplayName "MisebanAI Camera Agent" -StartupType Automatic | Out-Null
    Start-Service $ServiceName
    Write-Host "    Service registered and started"
} else {
    Write-Host "    Tip: Run as Administrator to install as a Windows Service (auto-start)"
    Write-Host "    Starting manually..."
    Start-Process "$InstallDir\miseban-agent.exe" -WindowStyle Hidden
}

Write-Host ""
Write-Host "=== インストール完了 / Installation complete ===" -ForegroundColor Green
Write-Host ""
Write-Host "  セットアップ画面 / Setup wizard:"
Write-Host "    http://localhost:3939" -ForegroundColor Cyan
Write-Host ""
Write-Host "  ブラウザでセットアップ画面を開いてカメラを接続してください。"
Write-Host ""
