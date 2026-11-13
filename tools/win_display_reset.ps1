# LG UltraFine 5K - test programmatic "display reset" on Windows (no admin).
#
# Use when the display is connected but the screen stays black (the case that
# Win+Ctrl+Shift+B fixes). Try the methods one at a time, in order, and note
# which one brings the picture back:
#
#   1  ChangeDisplaySettingsEx(CDS_RESET) on the LG output - forced mode set
#   2  SetDisplayConfig(SDC_APPLY | SDC_USE_DATABASE_CURRENT) - re-apply topology
#   3  Injected Win+Ctrl+Shift+B keystroke (graphics driver reset)
#
# Appends to win_display_reset.log next to this script.

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$log  = Join-Path $here 'win_display_reset.log'
function Log([string]$s) { Write-Host $s; Add-Content -Path $log -Value $s -Encoding UTF8 }

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class UfDisplay {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct DISPLAY_DEVICE {
        public int cb;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)]  public string DeviceName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceString;
        public int StateFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceID;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceKey;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct DEVMODE {
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
        public short dmSpecVersion; public short dmDriverVersion; public short dmSize;
        public short dmDriverExtra; public int dmFields;
        public int dmPositionX; public int dmPositionY;
        public int dmDisplayOrientation; public int dmDisplayFixedOutput;
        public short dmColor; public short dmDuplex; public short dmYResolution;
        public short dmTTOption; public short dmCollate;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
        public short dmLogPixels; public int dmBitsPerPel;
        public int dmPelsWidth; public int dmPelsHeight;
        public int dmDisplayFlags; public int dmDisplayFrequency;
        public int dmICMMethod; public int dmICMIntent; public int dmMediaType;
        public int dmDitherType; public int dmReserved1; public int dmReserved2;
        public int dmPanningWidth; public int dmPanningHeight;
    }

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    static extern bool EnumDisplayDevicesW(string lpDevice, uint iDevNum, ref DISPLAY_DEVICE dd, uint flags);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    static extern bool EnumDisplaySettingsW(string deviceName, int modeNum, ref DEVMODE dm);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    static extern int ChangeDisplaySettingsExW(string deviceName, ref DEVMODE dm, IntPtr hwnd, uint flags, IntPtr lParam);
    [DllImport("user32.dll")]
    static extern int SetDisplayConfig(uint numPaths, IntPtr paths, uint numModes, IntPtr modes, uint flags);
    [DllImport("user32.dll")]
    static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);

    const int ENUM_CURRENT_SETTINGS = -1;
    const int DISPLAY_DEVICE_ATTACHED_TO_DESKTOP = 0x1;
    const int DISPLAY_DEVICE_PRIMARY_DEVICE = 0x4;
    const uint CDS_RESET = 0x40000000;
    const uint SDC_USE_DATABASE_CURRENT = 0x0000000F;
    const uint SDC_APPLY = 0x00000080;

    static DISPLAY_DEVICE NewDD() { var d = new DISPLAY_DEVICE(); d.cb = Marshal.SizeOf(typeof(DISPLAY_DEVICE)); return d; }
    static DEVMODE NewDM() { var m = new DEVMODE(); m.dmSize = (short)Marshal.SizeOf(typeof(DEVMODE)); return m; }

    // One line per adapter output, with its monitors and current mode.
    public static List<string> Describe() {
        var lines = new List<string>();
        for (uint i = 0; ; i++) {
            var a = NewDD();
            if (!EnumDisplayDevicesW(null, i, ref a, 0)) break;
            var dm = NewDM();
            string mode = EnumDisplaySettingsW(a.DeviceName, ENUM_CURRENT_SETTINGS, ref dm)
                ? string.Format("{0}x{1}@{2}Hz pos {3},{4}", dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency, dm.dmPositionX, dm.dmPositionY)
                : "no current mode";
            string flags = ((a.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) != 0 ? "desktop " : "")
                         + ((a.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE) != 0 ? "primary " : "");
            for (uint j = 0; ; j++) {
                var m = NewDD();
                if (!EnumDisplayDevicesW(a.DeviceName, j, ref m, 0)) break;
                lines.Add(string.Format("{0} [{1}] {2} | {3} | {4} | {5}",
                    a.DeviceName, flags.Trim(), a.DeviceString, m.DeviceString, m.DeviceID, mode));
            }
        }
        return lines;
    }

    // Adapter output names (\\.\DISPLAYn) whose monitor id contains the pattern.
    public static List<string> FindOutputs(string monitorPattern) {
        var res = new List<string>();
        for (uint i = 0; ; i++) {
            var a = NewDD();
            if (!EnumDisplayDevicesW(null, i, ref a, 0)) break;
            for (uint j = 0; ; j++) {
                var m = NewDD();
                if (!EnumDisplayDevicesW(a.DeviceName, j, ref m, 0)) break;
                if (m.DeviceID != null && m.DeviceID.IndexOf(monitorPattern, StringComparison.OrdinalIgnoreCase) >= 0
                    && !res.Contains(a.DeviceName)) res.Add(a.DeviceName);
            }
        }
        return res;
    }

    // Method 1: forced mode set with the current mode. Returns DISP_CHANGE_* (0 = ok).
    public static string ResetMode(string output) {
        var dm = NewDM();
        if (!EnumDisplaySettingsW(output, ENUM_CURRENT_SETTINGS, ref dm)) return "no current mode on " + output;
        int r = ChangeDisplaySettingsExW(output, ref dm, IntPtr.Zero, CDS_RESET, IntPtr.Zero);
        return string.Format("ChangeDisplaySettingsEx({0}, {1}x{2}@{3}, CDS_RESET) = {4}",
            output, dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency, r);
    }

    // Method 2: re-apply the current topology from the database. Returns Win32 error (0 = ok).
    public static int ReapplyTopology() {
        return SetDisplayConfig(0, IntPtr.Zero, 0, IntPtr.Zero, SDC_APPLY | SDC_USE_DATABASE_CURRENT);
    }

    // Method 3: inject Win+Ctrl+Shift+B.
    public static void DriverResetKeys() {
        const uint EXT = 0x1, UP = 0x2;
        keybd_event(0x5B, 0, EXT, UIntPtr.Zero);      // LWin
        keybd_event(0x11, 0, 0, UIntPtr.Zero);        // Ctrl
        keybd_event(0x10, 0, 0, UIntPtr.Zero);        // Shift
        keybd_event(0x42, 0, 0, UIntPtr.Zero);        // B
        keybd_event(0x42, 0, UP, UIntPtr.Zero);
        keybd_event(0x10, 0, UP, UIntPtr.Zero);
        keybd_event(0x11, 0, UP, UIntPtr.Zero);
        keybd_event(0x5B, 0, EXT | UP, UIntPtr.Zero);
    }
}
'@

$LG_MONITOR = 'GSM5B11'   # LG UltraFine 5K EDID id (from win_diag log)

function Show-State([string]$title) {
    Log "--- $title ---"
    foreach ($l in [UfDisplay]::Describe()) { Log "  $l" }
}

Log ''
Log "=== win_display_reset  $(Get-Date -Format s) ==="
Show-State 'displays now'

while ($true) {
    Write-Host ''
    Write-Host 'Try one method at a time, then look at the LG screen:'
    Write-Host '  1  Forced mode set on the LG output (CDS_RESET)'
    Write-Host '  2  Re-apply display topology (SetDisplayConfig)'
    Write-Host '  3  Inject Win+Ctrl+Shift+B'
    Write-Host '  q  Quit'
    $c = (Read-Host 'Choice').Trim()
    if ($c -eq 'q') { break }
    if ($c -eq '1') {
        $outs = [UfDisplay]::FindOutputs($LG_MONITOR)
        if ($outs.Count -eq 0) { Log "1: no output with monitor $LG_MONITOR found" }
        foreach ($o in $outs) { Log ("1: " + [UfDisplay]::ResetMode($o)) }
    } elseif ($c -eq '2') {
        Log ("2: SetDisplayConfig = " + [UfDisplay]::ReapplyTopology())
    } elseif ($c -eq '3') {
        [UfDisplay]::DriverResetKeys(); Log '3: Win+Ctrl+Shift+B injected'
    } else {
        continue
    }
    Start-Sleep -Seconds 4
    Show-State "after method $c"
    $ok = Read-Host 'Did the LG picture come back? [y/n]'
    Log "method $c -> picture back: $ok"
    if ($ok -match '^(y|k)') { Log "RESULT: method $c works"; break }
}
Log "Log: $log"
