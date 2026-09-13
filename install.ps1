# Alloy Installer for Windows (PowerShell)
# Usage: irm https://raw.githubusercontent.com/alloy-runtime/alloy/main/install.ps1 | iex

$ErrorActionPreference = "Stop"

$Repo = "alloy-runtime/alloy"
$InstallDir = Join-Path $HOME ".alloy\bin"
$ExePath = Join-Path $InstallDir "alloy.exe"

Write-Host @"
     ___       __   __                 
    /   |     / /  / /____  __  __     
   / /| |    / /  / // __ \/ / / /     
  / ___ |   / /__/ // /_/ / /_/ /      
 /_/  |_|  /_____/____/\____/\__, /       
                            /____/         
"@ -ForegroundColor Cyan

Write-Host "`nInstalling Alloy Systems Runtime for Windows...`n" -ForegroundColor White

# 1. Detect Architecture
$Arch = [System.Environment]::GetEnvironmentVariable("PROCESSOR_ARCHITECTURE")
switch ($Arch) {
    "AMD64" { $Target = "x86_64-pc-windows-msvc" }
    "ARM64" { $Target = "aarch64-pc-windows-msvc" }
    default {
        Write-Error "Unsupported architecture: $Arch"
        exit 1
    }
}

$ArchiveName = "alloy-$Target.zip"
$DownloadUrl = "https://github.com/$Repo/releases/latest/download/$ArchiveName"

Write-Host "  Target:       $Target"
Write-Host "  Destination:  $ExePath`n"

# 2. Prepare Temp Directory
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("alloy-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $TempDir -Force | Out-Null
$ZipPath = Join-Path $TempDir $ArchiveName

try {
    Write-Host "Downloading $ArchiveName..." -ForegroundColor Cyan
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $ZipPath -UseBasicParsing

    Write-Host "Extracting archive..."
    Expand-Archive -Path $ZipPath -DestinationPath $TempDir -Force

    # Locate binary
    $SourceExe = Join-Path $TempDir "alloy.exe"
    if (-not (Test-Path $SourceExe)) {
        $Found = Get-ChildItem -Path $TempDir -Filter "alloy.exe" -Recurse | Select-Object -First 1
        if ($Found) {
            $SourceExe = $Found.FullName
        } else {
            Write-Error "alloy.exe not found inside archive."
            exit 1
        }
    }

    if (-not (Test-Path $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }

    Copy-Item -Path $SourceExe -Destination $ExePath -Force
    Write-Host "`nAlloy was installed successfully to $ExePath`n" -ForegroundColor Green

    # 3. Add to User PATH if not present
    $UserPath = [System.Environment]::GetEnvironmentVariable("PATH", "User")
    if ($UserPath -notlike "*$InstallDir*") {
        Write-Host "Adding $InstallDir to User PATH environment variable..." -ForegroundColor Yellow
        $NewPath = if ($UserPath) { "$UserPath;$InstallDir" } else { $InstallDir }
        [System.Environment]::SetEnvironmentVariable("PATH", $NewPath, "User")
        $env:PATH = "$env:PATH;$InstallDir"
        Write-Host "Added to User PATH." -ForegroundColor Green
    }

    Write-Host "Verify installation in a new terminal with:" -ForegroundColor White
    Write-Host "  alloy --version`n" -ForegroundColor Cyan
}
finally {
    if (Test-Path $TempDir) {
        Remove-Item -Path $TempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
