# Compile check of kotost_aura alone, without the launcher's build: horizon-mixins apply onto the pinned
# Horizon source, then `cargo check` of the client at idle priority. Its own stage (mod-check\stage-aura),
# the shared target dir (dependencies reused). Logs: E:\Games\Horizon\mod-check\aura_*.
#   powershell -File tools\check.ps1 [-Jobs 4]
param([int]$Jobs = 4)
$ErrorActionPreference = "Continue"
$L = "E:\Games\Horizon\launcher-data"
$C = "E:\Games\Horizon\mod-check"
$M = Split-Path $PSScriptRoot -Parent
$commit = "fdc87aecdbd116bf1e08adcd062fcbaea8c2e2ca"
$rust = Get-ChildItem "$L\rust" -Directory | Select-Object -First 1
$env:PATH = "$($rust.FullName)\bin;$($rust.FullName)\mingw64\bin;$env:PATH"
$env:CARGO_HOME = "$L\cargo"
$env:CARGO_TARGET_DIR = "$C\target"
$mods = "$C\mods-aura"
if (Test-Path $mods) { Remove-Item -Recurse -Force $mods }
New-Item -ItemType Directory -Force "$mods\kotost_aura" | Out-Null
foreach ($d in "hooks") { Copy-Item -Recurse "$M\$d" "$mods\kotost_aura\$d" }
Copy-Item "$M\*.rs", "$M\horizon-mixin.toml" "$mods\kotost_aura\"
$mixins = Get-ChildItem "$L\native\tools" -Directory | Select-Object -First 1
if (Test-Path "$C\stage-aura.new") { Remove-Item -Recurse -Force "$C\stage-aura.new" }
$mx = Start-Process -FilePath "$($mixins.FullName)\bin\horizon-mixins.exe" `
  -ArgumentList @("apply", "--workspace", "$L\native\source\$commit", "--mods", $mods, "--out", "$C\stage-aura.new") `
  -NoNewWindow -PassThru -Wait -RedirectStandardOutput "$C\aura_mixins.out" -RedirectStandardError "$C\aura_mixins.err"
if (-not (Test-Path "$C\stage-aura.new\Cargo.toml")) {
  "mixin apply failed (exit $($mx.ExitCode))"
  Get-Content "$C\aura_mixins.out" -Tail 40
  Get-Content "$C\aura_mixins.err" -Tail 40
  exit 2
}
python -I "E:\Games\Horizon\ports\tools\sync_stage.py" "$C\stage-aura.new" "$C\stage-aura"
$cargoArgs = @("check", "--manifest-path", "$C\stage-aura\Cargo.toml", "--locked", "--offline", "--profile", "fast",
  "-p", "horizon-client", "--bin", "horizon-client", "-j", "$Jobs", "--message-format", "short")
$p = Start-Process -FilePath "cargo.exe" -ArgumentList $cargoArgs -NoNewWindow -PassThru `
  -RedirectStandardError "$C\aura_cargo.err" -RedirectStandardOutput "$C\aura_cargo.out"
try { $p.PriorityClass = "Idle" } catch {}
$t0 = Get-Date
while (-not $p.HasExited) {
  Get-Process rustc -ErrorAction SilentlyContinue | ForEach-Object { try { if ($_.PriorityClass -ne "Idle") { $_.PriorityClass = "Idle" } } catch {} }
  Start-Sleep -Seconds 3
}
$p.WaitForExit()
"cargo exit $($p.ExitCode) after $([int]((Get-Date) - $t0).TotalMinutes) min"
Get-Content "$C\aura_cargo.err" | Select-String -Pattern "kotost_aura.*(error|warning: unused)|^error" | Select-Object -First 80 | ForEach-Object { $_.Line }
Get-Content "$C\aura_cargo.err" -Tail 3
