# LG UltraFine 5K - Windows diagnostics.
# Run via win_diag.cmd as a normal user (read-only, no admin, changes nothing).
# Writes win_diag_<timestamp>.log next to this script. Must run in Windows PowerShell 5.1 (WinRT interop), not pwsh 7.
#
# Sections:
#   1  system, Thunderbolt/USB controller drivers
#   2  monitors seen by Windows (is the picture path working?)
#   3  failed / problem USB devices (+ parent chain)
#   4  LG devices VID_043E (+ parent chain), HID / Sensors classes
#   5  power settings that commonly break USB enumeration
#   6  event logs (last 3 h): Kernel-PnP, USB, Thunderbolt
#   7  Windows.Devices.Sensors.LightSensor enumeration + readings
#   8  hidapi (python uf_probe.py): list, brightness, ALS stream

$ErrorActionPreference = 'Continue'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$log  = Join-Path $here ("win_diag_{0}.log" -f (Get-Date -Format 'yyyyMMdd_HHmmss'))
$LG_VID = 'VID_043E'

function Log([string]$s) { Write-Host $s; Add-Content -Path $log -Value $s -Encoding UTF8 }
function LogObj($o) { if ($null -ne $o) { Log (($o | Out-String -Width 300).TrimEnd()) } else { Log '(none)' } }
function Section([string]$t) { Log ''; Log ('=' * 78); Log "== $t"; Log ('=' * 78) }

function Get-Parent($id) {
    try { (Get-PnpDeviceProperty -InstanceId $id -KeyName 'DEVPKEY_Device_Parent' -ErrorAction Stop).Data } catch { $null }
}

function Get-Chain($id) {
    $out = @(); $cur = $id; $n = 0
    while ($cur -and $n -lt 12) {
        $d = Get-PnpDevice -InstanceId $cur -ErrorAction SilentlyContinue
        $name = if ($d) { $d.FriendlyName } else { '?' }
        $st   = if ($d) { $d.Status } else { '?' }
        $out += ('  ' * $n) + "<- [$st] $name  ($cur)"
        $cur = Get-Parent $cur; $n++
    }
    $out -join "`n"
}

function Get-ProblemUsb {
    Get-CimInstance Win32_PnPEntity |
        Where-Object { $_.PNPDeviceID -match '^USB\\' -and $_.ConfigManagerErrorCode -ne 0 }
}

function Show-ProblemUsb {
    $bad = @(Get-ProblemUsb)
    if ($bad.Count -eq 0) { Log 'No USB devices with a problem code.'; return }
    foreach ($b in $bad) {
        Log ("PROBLEM code={0}  {1}" -f $b.ConfigManagerErrorCode, $b.Name)
        Log (Get-Chain $b.PNPDeviceID)
    }
    Log '(code 43 = device reported a problem / descriptor request failed; 28 = no driver; 10 = cannot start)'
    return $bad
}

function Show-LgDevices {
    $lg = @(Get-PnpDevice | Where-Object { $_.InstanceId -match $LG_VID })
    if ($lg.Count -eq 0) { Log "No $LG_VID devices known to Windows (present or past)."; return }
    LogObj ($lg | Sort-Object InstanceId | Format-Table Status, Present, Class, FriendlyName, InstanceId -AutoSize)
    $present = $lg | Where-Object { $_.Present -and $_.InstanceId -match '^USB\\' } | Select-Object -First 1
    if ($present) { Log 'Parent chain of first present LG USB device:'; Log (Get-Chain $present.InstanceId) }
}

Log "LG UltraFine Windows diagnostics  $(Get-Date -Format s)"
Log "PowerShell $($PSVersionTable.PSVersion)"

# ---------------------------------------------------------------------------
Section '1. System, Thunderbolt / USB controllers'
$cs = Get-CimInstance Win32_ComputerSystem
$os = Get-CimInstance Win32_OperatingSystem
$bios = Get-CimInstance Win32_BIOS
Log "Computer : $($cs.Manufacturer) $($cs.Model)"
Log "OS       : $($os.Caption) $($os.Version) build $($os.BuildNumber)"
Log "BIOS     : $($bios.SMBIOSBIOSVersion) ($($bios.ReleaseDate))"
try { $bb = Get-CimInstance Win32_BaseBoard; Log "Board    : $($bb.Manufacturer) $($bb.Product)" } catch {}

LogObj (Get-CimInstance Win32_PnPSignedDriver |
    Where-Object { $_.DeviceName -match 'Thunderbolt|USB4|xHCI|eXtensible|USB.*Host Controller|USB Root Hub' } |
    Sort-Object DeviceName |
    Format-Table DeviceName, DriverVersion, @{n='Date';e={ if ($_.DriverDate) { $_.DriverDate.ToString('yyyy-MM-dd') } }}, Manufacturer -AutoSize)

Log 'Thunderbolt service / app:'
LogObj (Get-Service -Name '*thunderbolt*' -ErrorAction SilentlyContinue | Format-Table Name, Status, StartType -AutoSize)
LogObj (Get-AppxPackage -Name '*Thunderbolt*' -ErrorAction SilentlyContinue | Format-Table Name, Version -AutoSize)

# ---------------------------------------------------------------------------
Section '2. Monitors seen by Windows'
try {
    $mons = Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorID -ErrorAction Stop
    foreach ($m in $mons) {
        $dec = { param($a) if ($a) { -join ($a | Where-Object { $_ -ne 0 } | ForEach-Object { [char]$_ }) } }
        Log ("{0} {1}  serial={2}  active={3}  {4}" -f (& $dec $m.ManufacturerName), (& $dec $m.UserFriendlyName),
             (& $dec $m.SerialNumberID), $m.Active, $m.InstanceName)
    }
} catch { Log "WmiMonitorID failed: $_" }
LogObj (Get-PnpDevice -Class Monitor -PresentOnly -ErrorAction SilentlyContinue | Format-Table Status, FriendlyName, InstanceId -AutoSize)

# ---------------------------------------------------------------------------
Section '3. USB devices with problems (failed enumeration etc.)'
$null = Show-ProblemUsb

# ---------------------------------------------------------------------------
Section "4. LG devices ($LG_VID), HID and Sensors"
Show-LgDevices
Log ''
Log 'Sensors class:'
LogObj (Get-PnpDevice -Class Sensor -ErrorAction SilentlyContinue | Format-Table Status, Present, FriendlyName, InstanceId -AutoSize)
Log 'HID devices from LG:'
LogObj (Get-PnpDevice -Class HIDClass -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -match $LG_VID } |
    Format-Table Status, Present, FriendlyName, InstanceId -AutoSize)

# ---------------------------------------------------------------------------
Section '5. Power settings'
Log 'USB selective suspend (current scheme; 0=disabled 1=enabled):'
$ps = powercfg /q SCHEME_CURRENT 2a737441-1930-4402-8d77-b2bebba308a3 48e6b7a6-50f5-4782-a5d4-53bb8f07e226 2>&1
LogObj ($ps | Where-Object { $_ -and $_ -notmatch '^\s*$' } | Select-Object -Last 8)
try {
    $hb = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -ErrorAction Stop).HiberbootEnabled
    Log "Fast startup (HiberbootEnabled): $hb"
} catch { Log 'Fast startup: unknown' }
Log '"Allow the computer to turn off this device" on USB hubs / controllers (Enable=True means allowed):'
try {
    $pm = Get-CimInstance -Namespace root\wmi -ClassName MSPower_DeviceEnable -ErrorAction Stop |
        Where-Object { $_.InstanceName -match '^(USB\\ROOT_HUB|USB\\VID_[0-9A-F]+&PID_[0-9A-F]+\\|PCI\\)' }
    LogObj ($pm | ForEach-Object {
        $id = $_.InstanceName -replace '_0$', ''
        $d = Get-PnpDevice -InstanceId $id -ErrorAction SilentlyContinue
        if ($d -and $d.Class -eq 'USB') { [pscustomobject]@{ Enable = $_.Enable; Name = $d.FriendlyName; Id = $id } }
    } | Format-Table -AutoSize)
} catch { Log "MSPower_DeviceEnable failed: $_" }

# ---------------------------------------------------------------------------
Section '6. Event logs, last 3 h'
$since = (Get-Date).AddHours(-3)
try {
    $ev = Get-WinEvent -FilterHashtable @{ LogName = 'Microsoft-Windows-Kernel-PnP/Configuration'; StartTime = $since } -ErrorAction Stop |
        Where-Object { $_.Message -match 'USB|VID_|Thunderbolt|HID|SENSOR' } | Select-Object -First 150
    Log "Kernel-PnP/Configuration ($(@($ev).Count) matching):"
    foreach ($e in $ev) { Log ("{0:HH:mm:ss} id={1} {2}" -f $e.TimeCreated, $e.Id, (($e.Message -split "`r?`n" | Where-Object { $_ }) -join ' | ')) }
} catch { Log "Kernel-PnP/Configuration: $_" }
Log ''
try {
    $ev = Get-WinEvent -FilterHashtable @{ LogName = 'System'; StartTime = $since; Level = 1, 2, 3 } -ErrorAction Stop |
        Where-Object { $_.ProviderName -match 'USB|PnP|Thunderbolt|ACPI|pci|Kernel-Power' } | Select-Object -First 100
    Log "System log errors/warnings ($(@($ev).Count) matching):"
    foreach ($e in $ev) { Log ("{0:HH:mm:ss} {1} id={2} {3}" -f $e.TimeCreated, $e.ProviderName, $e.Id, (($e.Message -split "`r?`n" | Where-Object { $_ }) -join ' | ')) }
} catch { Log "System log: $_" }

# ---------------------------------------------------------------------------
Section '7. WinRT LightSensor (fallback path)'
try {
    Add-Type -AssemblyName System.Runtime.WindowsRuntime
    $asTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
        $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
        $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' })[0]
    function Await($op, [Type]$t) {
        $task = $asTaskGeneric.MakeGenericMethod($t).Invoke($null, @($op))
        $null = $task.Wait(-1); $task.Result
    }
    $null = [Windows.Devices.Sensors.LightSensor, Windows.Devices.Sensors, ContentType = WindowsRuntime]
    $null = [Windows.Devices.Enumeration.DeviceInformation, Windows.Devices.Enumeration, ContentType = WindowsRuntime]
    $sel = [Windows.Devices.Sensors.LightSensor]::GetDeviceSelector()
    $infos = Await ([Windows.Devices.Enumeration.DeviceInformation]::FindAllAsync($sel)) ([Windows.Devices.Enumeration.DeviceInformationCollection])
    Log "Light sensors found: $($infos.Count)"
    foreach ($i in $infos) {
        Log "- $($i.Name)  enabled=$($i.IsEnabled)"
        Log "  id: $($i.Id)"
        $s = Await ([Windows.Devices.Sensors.LightSensor]::FromIdAsync($i.Id)) ([Windows.Devices.Sensors.LightSensor])
        if (-not $s) { Log '  FromIdAsync returned null (access denied?)'; continue }
        Log "  min report interval: $($s.MinimumReportInterval) ms"
        Log '  Reading 15 s - cover the top bezel, then shine a light:'
        for ($k = 0; $k -lt 15; $k++) {
            $r = $s.GetCurrentReading()
            if ($r) { Log ("  {0:HH:mm:ss} lux={1}" -f (Get-Date), $r.IlluminanceInLux) } else { Log '  (no reading)' }
            Start-Sleep -Seconds 1
        }
    }
    $def = [Windows.Devices.Sensors.LightSensor]::GetDefault()
    Log "GetDefault(): $(if ($def) { $def.DeviceId } else { 'null' })"
} catch { Log "LightSensor test failed: $_" }

# ---------------------------------------------------------------------------
Section '8. hidapi (uf_probe.py)'
$py = $null
foreach ($c in @(@('py', '-3'), @('python'))) {
    try {
        $v = & $c[0] $c[1..9] -c 'import sys; print(sys.version)' 2>$null
        if ($LASTEXITCODE -eq 0 -and $v) { $py = $c; Log "Python: $v"; break }
    } catch {}
}
if (-not $py) {
    Log 'Python not found - skipping. Install from python.org (tick "Add to PATH") and re-run.'
} else {
    $pyExe = $py[0]; $pyArgs = @($py[1..9] | Where-Object { $_ })
    $probe = Join-Path $here 'uf_probe.py'
    & $pyExe @pyArgs -c 'import hid' 2>$null
    if ($LASTEXITCODE -ne 0) {
        Log 'Installing hidapi (pip --user)...'
        & $pyExe @pyArgs -m pip install --user --quiet hidapi 2>&1 | ForEach-Object { Log "$_" }
    }
    Log '--- list ---'
    & $pyExe @pyArgs -u $probe list 2>&1 | ForEach-Object { Log "$_" }
    Log '--- bright ---'
    & $pyExe @pyArgs -u $probe bright 2>&1 | ForEach-Object { Log "$_" }
    Log '--- als 25 s: cover the top bezel ~5 s, then shine a light ---'
    & $pyExe @pyArgs -u $probe als 25 2>&1 | ForEach-Object { Log "$_" }
}

Section 'done'
Log "Log written to: $log"
