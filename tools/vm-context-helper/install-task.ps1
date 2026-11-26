# screenpipe — AI that knows everything you've seen, said, or heard
# https://screenpipe.com
#
# Registers a per-user logon task that runs the VM context helper.
# Run once inside the VM, as the user who will be logged in:
#   dotnet publish -c Release -o $env:LOCALAPPDATA\vm-context-helper\bin
#   .\install-task.ps1
# The API key goes in %APPDATA%\vm-context-helper\api-key (this script never touches it).
param(
    [string]$ExePath = (Join-Path $env:LOCALAPPDATA 'vm-context-helper\bin\VmContextHelper.exe')
)

if (-not (Test-Path $ExePath)) { throw "Helper not found at $ExePath. Publish it first." }

$action = New-ScheduledTaskAction -Execute $ExePath
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
# Interactive, non-elevated: the helper must run in the user's desktop session to see the foreground window.
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)

Register-ScheduledTask -TaskName 'VmContextHelper' -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings -Force | Out-Null
Write-Output "Registered task VmContextHelper (runs at next logon)."
