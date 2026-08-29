$ErrorActionPreference = "Continue"
$exe = "c:\Users\adou0002\space_invader\target\release\space_invader.exe"
$sp = "C:\Users\adou0002\AppData\Local\Temp\claude\c--Users-adou0002-space-invader\5196260e-4dbe-48c3-b3cc-aed06163de11\scratchpad"
$log = "$sp\elevated_bench.log"

function Log($msg) { $msg | Out-File $log -Append -Encoding utf8 }

"=== ELEVATED MFT BENCH $(Get-Date -Format s) ===" | Out-File $log -Encoding utf8
$elev = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Log "elevated: $elev"

# Warm-up MFT run
$null = & $exe C:\ --mft --no-dupes --no-report --top 1 2>$null

Log "--- MFT scan-only x5 (C:\) ---"
for ($i = 1; $i -le 5; $i++) {
    $t = Measure-Command { $script:out = & $exe C:\ --mft --no-dupes --no-report --top 1 2>$null }
    $line = ($script:out | Select-String "files," | Select-Object -First 1).Line
    $fast = ($script:out | Select-String "MFT fast path" | Measure-Object).Count -gt 0
    Log "run $i wall=$([math]::Round($t.TotalSeconds,2))s mftPathUsed=$fast :: $($line.Trim())"
}

Log "--- MFT full (scan+dupes+report) x2 (C:\) ---"
for ($i = 1; $i -le 2; $i++) {
    $t = Measure-Command { $script:out = & $exe C:\ --mft --report "$sp\mft_bench.html" --top 1 2>$null }
    $line = ($script:out | Select-String "files," | Select-Object -First 1).Line
    Log "run $i wall=$([math]::Round($t.TotalSeconds,2))s :: $($line.Trim())"
}

Log "--- walker scan-only x2 (C:\, elevated, for comparison) ---"
for ($i = 1; $i -le 2; $i++) {
    $t = Measure-Command { $script:out = & $exe C:\ --no-dupes --no-report --top 1 2>$null }
    $line = ($script:out | Select-String "files," | Select-Object -First 1).Line
    Log "run $i wall=$([math]::Round($t.TotalSeconds,2))s :: $($line.Trim())"
}

Log "--- MFT subtree test (AppData\Local) ---"
$t = Measure-Command { $script:out = & $exe "C:\Users\adou0002\AppData\Local" --mft --no-dupes --no-report --top 3 2>$null }
Log "wall=$([math]::Round($t.TotalSeconds,2))s"
$script:out | ForEach-Object { Log "  $_" }

Log "ELEVATED BENCH DONE"
