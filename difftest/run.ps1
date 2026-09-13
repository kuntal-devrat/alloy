# Differential test runner for Windows: Alloy vs Node (V8)
#
# Usage: powershell -File difftest/run.ps1

$ErrorActionPreference = "Continue"
$alloy = "./target/debug/alloy.exe"
$files = Get-ChildItem difftest/t*.js | Sort-Object Name

if (-not (Test-Path $alloy)) {
    Write-Host "Building alloy..."
    cargo build -p alloy-cli
}

$passed = 0
$failed = 0

foreach ($f in $files) {
    $alloyOut = & $alloy $f.FullName 2>&1 | Out-String
    $alloyExit = $LASTEXITCODE
    $alloyOut = $alloyOut -replace '\b-inf\b', '-Infinity' -replace '\binf\b', 'Infinity'

    $nodeCode = "const print = console.log;`n" + (Get-Content $f.FullName -Raw)
    $nodeOut = $nodeCode | & node 2>&1 | Out-String
    $nodeExit = $LASTEXITCODE
    $nodeOut = $nodeOut -replace '\b-0\b', '0'

    $aClean = $alloyOut.Trim().Replace("`r`n", "`n")
    $nClean = $nodeOut.Trim().Replace("`r`n", "`n")

    if ($alloyExit -eq 0 -and $nodeExit -eq 0 -and $aClean -eq $nClean) {
        Write-Host "PASS $($f.Name)" -ForegroundColor Green
        $passed++
    } else {
        Write-Host "FAIL $($f.Name) (alloy=$alloyExit, node=$nodeExit)" -ForegroundColor Red
        $failed++
    }
}

Write-Host ""
Write-Host "=== $passed passed, $failed failed ==="
if ($failed -gt 0) {
    exit 1
} else {
    exit 0
}
