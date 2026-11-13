# LG UltraFine 5K - Windows state checks (read-only, no admin, changes nothing).
#
#   1  Display state: is the LG active? If not, is that a fault (persisted
#      topology says it should be active -> SetDisplayConfig fix) or the
#      user's choice?
#   2  Light sensor state: which driver owns HID MI_02 (plain HID = readable,
#      sensor driver = needs the "Fix light sensor" swap)?
#   3  Laptop panel: brightness (WMI), Windows adaptive brightness setting,
#      AC/battery, laptop light sensor.
#   4  LG brightness (HID via uf_probe.py) next to the laptop brightness.
#   5  Watch (-WatchSeconds N): laptop brightness + laptop lux every second
#      while uf_probe.py streams LG lux in parallel - data for the sync design.
#
# Writes win_check_<timestamp>.log next to this script (via win_check.cmd).

param([int]$WatchSeconds = 60)

$ErrorActionPreference = 'Continue'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$LG_EDID = 'GSM5B11'

function Section([string]$t) { ''; ('=' * 70); "== $t"; ('=' * 70) }

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class UfTopo {
    [StructLayout(LayoutKind.Sequential)] public struct LUID { public uint Low; public int High; }
    [StructLayout(LayoutKind.Sequential)] public struct RATIONAL { public uint Num; public uint Den; }
    [StructLayout(LayoutKind.Sequential)]
    public struct SOURCE_INFO { public LUID adapterId; public uint id; public uint modeInfoIdx; public uint statusFlags; }
    [StructLayout(LayoutKind.Sequential)]
    public struct TARGET_INFO {
        public LUID adapterId; public uint id; public uint modeInfoIdx; public uint outputTechnology;
        public uint rotation; public uint scaling; public RATIONAL refreshRate; public uint scanLineOrdering;
        public int targetAvailable; public uint statusFlags;
    }
    [StructLayout(LayoutKind.Sequential)]
    public struct PATH_INFO { public SOURCE_INFO sourceInfo; public TARGET_INFO targetInfo; public uint flags; }
    [StructLayout(LayoutKind.Sequential, Size = 64)]
    public struct MODE_INFO { public uint infoType; public uint id; public LUID adapterId; }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct TARGET_NAME {
        public uint type; public uint size; public LUID adapterId; public uint id;
        public uint flags; public uint outputTechnology; public ushort edidManufactureId; public ushort edidProductCodeId;
        public uint connectorInstance;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)] public string monitorFriendlyDeviceName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string monitorDevicePath;
    }

    [DllImport("user32.dll")] static extern int GetDisplayConfigBufferSizes(uint flags, out uint nPath, out uint nMode);
    [DllImport("user32.dll")] static extern int QueryDisplayConfig(uint flags, ref uint nPath, [Out] PATH_INFO[] paths,
        ref uint nMode, [Out] MODE_INFO[] modes, IntPtr topology);
    [DllImport("user32.dll")] static extern int QueryDisplayConfig(uint flags, ref uint nPath, [Out] PATH_INFO[] paths,
        ref uint nMode, [Out] MODE_INFO[] modes, out uint topology);
    [DllImport("user32.dll")] static extern int DisplayConfigGetDeviceInfo(ref TARGET_NAME req);

    public const uint QDC_ALL_PATHS = 1, QDC_ONLY_ACTIVE_PATHS = 2, QDC_DATABASE_CURRENT = 4;
    const uint PATH_ACTIVE = 1;

    public class Path { public string Monitor; public string DevicePath; public bool Active; public bool Available; public uint TargetId; public uint SourceId; }

    static string Name(LUID a, uint id, out string devPath) {
        var n = new TARGET_NAME();
        n.type = 2; n.size = (uint)Marshal.SizeOf(typeof(TARGET_NAME)); n.adapterId = a; n.id = id;
        int r = DisplayConfigGetDeviceInfo(ref n);
        devPath = r == 0 ? n.monitorDevicePath : "";
        return r == 0 ? n.monitorFriendlyDeviceName : ("(name err " + r + ")");
    }

    // Returns null and sets err on failure. topo = topology id for DATABASE_CURRENT.
    public static List<Path> Query(uint flags, out int err, out uint topo) {
        topo = 0; uint np, nm;
        err = GetDisplayConfigBufferSizes(flags, out np, out nm);
        if (err != 0) return null;
        var paths = new PATH_INFO[np]; var modes = new MODE_INFO[nm];
        err = (flags == QDC_DATABASE_CURRENT)
            ? QueryDisplayConfig(flags, ref np, paths, ref nm, modes, out topo)
            : QueryDisplayConfig(flags, ref np, paths, ref nm, modes, IntPtr.Zero);
        if (err != 0) return null;
        var res = new List<Path>();
        for (int i = 0; i < np; i++) {
            string dp;
            var p = paths[i];
            res.Add(new Path {
                Monitor = Name(p.targetInfo.adapterId, p.targetInfo.id, out dp), DevicePath = dp,
                Active = (p.flags & PATH_ACTIVE) != 0, Available = p.targetInfo.targetAvailable != 0,
                TargetId = p.targetInfo.id, SourceId = p.sourceInfo.id });
        }
        return res;
    }
}
'@

"LG UltraFine state check  $(Get-Date -Format s)"

# ---------------------------------------------------------------------------
Section '1. Display state'
$err = 0; $topo = 0
$all    = [UfTopo]::Query([UfTopo]::QDC_ALL_PATHS, [ref]$err, [ref]$topo);         $errAll = $err
$active = [UfTopo]::Query([UfTopo]::QDC_ONLY_ACTIVE_PATHS, [ref]$err, [ref]$topo);  $errAct = $err
$db     = [UfTopo]::Query([UfTopo]::QDC_DATABASE_CURRENT, [ref]$err, [ref]$topo);   $errDb = $err; $dbTopo = $topo

function IsLG($p) { $p.DevicePath -match $LG_EDID }
function ShowPaths($name, $list, $e) {
    if ($null -eq $list) { "${name}: query failed (error $e)"; return }
    "${name}: $($list.Count) paths"
    $list | Where-Object { $_.Active -or ($_.Available -and (IsLG $_)) } | ForEach-Object {
        "  active=$($_.Active) available=$($_.Available) src=$($_.SourceId) tgt=$($_.TargetId) $($_.Monitor)  $($_.DevicePath)"
    }
}
ShowPaths 'ALL (active + available LG)' $all $errAll
ShowPaths 'ACTIVE' $active $errAct
ShowPaths "DATABASE_CURRENT (topology=$dbTopo; 1=internal 2=clone 4=extend 8=external)" $db $errDb

$lgConnected = @($all | Where-Object { (IsLG $_) -and $_.Available }).Count -gt 0
$lgActive    = @($active | Where-Object { IsLG $_ }).Count -gt 0
$lgInDb      = @($db | Where-Object { (IsLG $_) -and $_.Active }).Count -gt 0
''
if (-not $lgConnected)   { 'VERDICT display: LG not connected (no available target with ' + $LG_EDID + ')' }
elseif ($lgActive)       { 'VERDICT display: OK - LG is active' }
elseif ($lgInDb)         { 'VERDICT display: FAULT - LG connected, persisted topology wants it active, live topology has it off -> black screen; fix = SetDisplayConfig(SDC_APPLY|SDC_USE_DATABASE_CURRENT)' }
else                     { 'VERDICT display: LG connected but off in the persisted topology too (user choice, or first connection) - do not auto-fix' }

# ---------------------------------------------------------------------------
Section '2. Light sensor driver'
$alsVerdict = 'not present (display unplugged?)'
$als = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -match '^HID\\VID_043E&PID_9A40&MI_02\\' })
if ($als.Count -eq 0) { 'No HID\VID_043E&PID_9A40&MI_02 device known.'; $alsVerdict = 'missing' }
foreach ($d in $als) {
    $props = @{}
    foreach ($k in 'DEVPKEY_Device_DriverInfPath', 'DEVPKEY_Device_Service', 'DEVPKEY_Device_ProblemCode', 'DEVPKEY_Device_DriverDesc') {
        try { $props[$k] = (Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName $k -ErrorAction Stop).Data } catch { $props[$k] = $null }
    }
    "present=$($d.Present) status=$($d.Status) class=$($d.Class) name='$($d.FriendlyName)'"
    "  inf=$($props.DEVPKEY_Device_DriverInfPath) service=$($props.DEVPKEY_Device_Service) problem=$($props.DEVPKEY_Device_ProblemCode) desc='$($props.DEVPKEY_Device_DriverDesc)'"
    "  id=$($d.InstanceId)"
    if ($d.Present) {
        if ($props.DEVPKEY_Device_DriverInfPath -eq 'input.inf' -and $d.Status -eq 'OK') { $alsVerdict = 'ok' }
        elseif ($props.DEVPKEY_Device_DriverInfPath -match 'sensorshidclassdriver') { $alsVerdict = 'sensor-driver' }
        else { $alsVerdict = "other ($($props.DEVPKEY_Device_DriverInfPath), $($d.Status))" }
    }
}
''
switch -Regex ($alsVerdict) {
    '^ok$'            { 'VERDICT sensor: OK - plain HID driver, readable with hidapi' }
    '^sensor-driver$' { 'VERDICT sensor: NEEDS FIX - Windows sensor driver bound (fails, hides the sensor); fix = swap to "HID-compliant device" (admin)' }
    default           { "VERDICT sensor: $alsVerdict" }
}

# ---------------------------------------------------------------------------
Section '3. Laptop panel'
try {
    $wb = Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorBrightness -ErrorAction Stop
    foreach ($b in $wb) { "panel brightness: $($b.CurrentBrightness) %  active=$($b.Active)  levels=$($b.Level.Count)  $($b.InstanceName)" }
} catch { "WmiMonitorBrightness: $_" }
'Adaptive brightness (power setting ADAPTBRIGHT, 0=off 1=on):'
$ab = powercfg /q SCHEME_CURRENT 7516b95f-f776-4464-8c53-06167f40cc99 fbd9aa66-9553-4097-ba44-ed6e9d65eab8 2>&1
$abIdx = @($ab | Where-Object { $_ -match '0x0000000[0-9a-f]\s*$' })
if ($abIdx.Count -gt 0) { $abIdx | ForEach-Object { "  $($_.Trim())" } } else { $ab | Select-Object -Last 6 | ForEach-Object { "  $_" } }
try {
    $bat = Get-CimInstance Win32_Battery -ErrorAction Stop | Select-Object -First 1
    if ($bat) { "power: $(if ($bat.BatteryStatus -eq 2) {'AC'} else {'battery'}) (BatteryStatus=$($bat.BatteryStatus), $($bat.EstimatedChargeRemaining) %)" }
} catch {}

$sensor = $null
try {
    $null = [Windows.Devices.Sensors.LightSensor, Windows.Devices.Sensors, ContentType = WindowsRuntime]
    $sensor = [Windows.Devices.Sensors.LightSensor]::GetDefault()
    if ($sensor) {
        $r = $sensor.GetCurrentReading()
        "laptop light sensor: $($sensor.DeviceId)"
        "  lux now: $(if ($r) { $r.IlluminanceInLux } else { 'no reading' })"
    } else { 'laptop light sensor: none' }
} catch { "laptop light sensor: $_" }

# ---------------------------------------------------------------------------
Section '4. LG brightness (HID)'
$py = $null
foreach ($c in @(@('py', '-3'), @('python'))) {
    try { $v = & $c[0] $c[1..9] -c 'import hid; print(1)' 2>$null; if ($LASTEXITCODE -eq 0 -and $v) { $py = $c; break } } catch {}
}
$probe = Join-Path $here 'uf_probe.py'
if (-not $py) { 'Python with hidapi not found - skipping HID parts (pip install hidapi)' }
else {
    $pyExe = $py[0]; $pyArgs = @($py[1..9] | Where-Object { $_ })
    & $pyExe @pyArgs -u $probe bright 2>&1 | ForEach-Object { "  $_" }
}

# ---------------------------------------------------------------------------
if ($WatchSeconds -gt 0) {
    Section "5. Watch $WatchSeconds s: laptop brightness + laptop lux, LG lux in parallel"
    'Change the light now and then (cover / uncover the laptop and the LG top bezel, lamp on/off).'
    $lgLog = Join-Path $env:TEMP ("uf_lg_als_{0}.txt" -f $PID)
    $proc = $null
    if ($py -and $alsVerdict -eq 'ok') {
        $argList = @($pyArgs) + @('-u', "`"$probe`"", 'als', "$WatchSeconds")
        $proc = Start-Process -FilePath $pyExe -ArgumentList $argList -NoNewWindow -PassThru -RedirectStandardOutput $lgLog
    } else { '(LG lux not streamed: sensor not readable or no Python)' }
    $last = ''
    $end = (Get-Date).AddSeconds($WatchSeconds)
    while ((Get-Date) -lt $end) {
        $pb = (Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorBrightness -ErrorAction SilentlyContinue | Select-Object -First 1).CurrentBrightness
        $lx = if ($sensor) { $rr = $sensor.GetCurrentReading(); if ($rr) { [math]::Round($rr.IlluminanceInLux, 1) } } else { '' }
        $line = "panel=$pb% laptop_lux=$lx"
        if ($line -ne $last) { '{0:HH:mm:ss} {1}' -f (Get-Date), $line; $last = $line }
        Start-Sleep -Milliseconds 1000
    }
    if ($proc) {
        $null = $proc.WaitForExit(5000)
        ''
        '--- LG lux (uf_probe.py als) ---'
        if (Test-Path $lgLog) { Get-Content $lgLog | Where-Object { $_ -notmatch 'no report in' } ; Remove-Item $lgLog -ErrorAction SilentlyContinue }
    }
}

Section 'done'
