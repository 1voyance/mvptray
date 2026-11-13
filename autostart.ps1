# Автозапуск Mihomo Tray (Rust) через Планировщик заданий.
#   autostart.ps1          — включить автозапуск нового трея и выключить старый
#   autostart.ps1 -Remove  — убрать автозапуск нового трея и вернуть старый
param([switch]$Remove)
$ErrorActionPreference = 'Stop'
$exe = Join-Path $PSScriptRoot 'mihomo-tray.exe'
$name = 'MihomoTrayRs'
$old = 'MihomoTray'

function Has-Task($n) { $null -ne (Get-ScheduledTask -TaskName $n -ErrorAction SilentlyContinue) }

if ($Remove) {
    if (Has-Task $name) { Unregister-ScheduledTask -TaskName $name -Confirm:$false; Write-Host "Автозапуск нового трея удалён." }
    if (Has-Task $old) { Enable-ScheduledTask -TaskName $old | Out-Null; Write-Host "Старый трей снова будет запускаться при входе." }
    Get-Process -Name 'mihomo-tray' -ErrorAction SilentlyContinue | Stop-Process -Force
    exit 0
}

if (-not (Test-Path $exe)) { throw "Не найден $exe" }

$action    = New-ScheduledTaskAction -Execute $exe -Argument '--hidden' -WorkingDirectory $PSScriptRoot
$trigger   = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Highest
$settings  = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
             -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -StartWhenAvailable
Register-ScheduledTask -TaskName $name -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
Write-Host "Автозапуск включён (задача $name, с правами администратора — без окон UAC)."

if (Has-Task $old) {
    Disable-ScheduledTask -TaskName $old | Out-Null
    Write-Host "Старый трей (задача $old) отключён в автозапуске. Вернуть: uninstall-autostart.bat"
}
Get-Process -Name 'MihomoTray' -ErrorAction SilentlyContinue | Stop-Process -Force
Get-Process -Name 'mihomo-tray' -ErrorAction SilentlyContinue | Stop-Process -Force
Start-ScheduledTask -TaskName $name
Write-Host "Новый трей запущен."
