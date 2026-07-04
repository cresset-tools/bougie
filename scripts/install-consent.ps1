
# ---------------------------------------------------------------------
# bougie telemetry consent block (PowerShell).
#
# Appended to the dist-generated installer.ps1 by publish-mirror.yml
# when promoting to installers/bougie/latest/. The dist script's
# entrypoint runs inside try/catch with `exit 1` on failure, so this
# block only executes after a successful install. `irm | iex` leaves
# stdin attached, so Read-Host works directly.
#
# Contract: writes the same mode file bougie reads
# (`%APPDATA%\bougie\telemetry`, single line
# `<mode> <yyyy-MM-dd> <consent-version>`); see TELEMETRY.md. Must
# never affect the installer's exit status.
# ---------------------------------------------------------------------
function Invoke-BougieTelemetryConsent {
    if ($env:BOUGIE_TELEMETRY) { return }
    if ($env:CI) { return }
    $cfgDir = Join-Path $env:APPDATA 'bougie'
    $modeFile = Join-Path $cfgDir 'telemetry'
    if (Test-Path $modeFile) { return }
    $date = (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')

    if ($env:DO_NOT_TRACK -and $env:DO_NOT_TRACK -ne '0') {
        New-Item -ItemType Directory -Force -Path $cfgDir | Out-Null
        Set-Content -Path $modeFile -Value "off $date 1"
        return
    }
    if (-not [Environment]::UserInteractive) { return }

    Write-Host ''
    Write-Host 'bougie can send anonymous usage statistics and crash reports to the'
    Write-Host 'bougie developers. This never includes project names, package names,'
    Write-Host 'paths, or IP addresses, and nothing is sent without your consent.'
    Write-Host 'Details + full field list: https://bougie.tools/telemetry'
    Write-Host ''
    $answer = Read-Host '  Enable anonymous telemetry? [Y/n]'

    if ($answer -match '^[nN]') {
        New-Item -ItemType Directory -Force -Path $cfgDir | Out-Null
        Set-Content -Path $modeFile -Value "off $date 1"
        Write-Host 'ok — telemetry is off. Enable later with: bougie telemetry on'
    } elseif ($answer -eq '' -or $answer -match '^[yY]') {
        New-Item -ItemType Directory -Force -Path $cfgDir | Out-Null
        Set-Content -Path $modeFile -Value "on $date 1"
        Write-Host 'telemetry enabled — inspect events anytime with: bougie telemetry log'
    }
    # Unclassifiable reply: record nothing; bougie may ask on first run.
}
try { Invoke-BougieTelemetryConsent } catch {}
# ------------------------- end consent block -------------------------
