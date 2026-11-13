# Read-only DDC/CI query through Windows' own monitor API (dxva2), i.e. via
# the graphics driver / DisplayPort AUX - not through the display's I2C bridge.
# Only "Capabilities" and "Get VCP feature" requests are sent (no Set VCP).
#
#   win_ddc.ps1                    capabilities + a few VCP values, all monitors
#   win_ddc.ps1 -WatchSeconds 30   also poll VCP codes each second on monitors
#                                  that answer DDC/CI, print changes

param([int]$WatchSeconds = 0)

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class UfDdc {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct PHYSICAL_MONITOR {
        public IntPtr hPhysicalMonitor;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string szPhysicalMonitorDescription;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct MONITORINFOEX {
        public int cbSize; public int l, t, r, b; public int wl, wt, wr, wb; public uint dwFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string szDevice;
    }
    delegate bool MonitorEnumProc(IntPtr hMon, IntPtr hdc, IntPtr rect, IntPtr data);

    [DllImport("user32.dll")] static extern bool EnumDisplayMonitors(IntPtr hdc, IntPtr clip, MonitorEnumProc cb, IntPtr data);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern bool GetMonitorInfoW(IntPtr hMon, ref MONITORINFOEX mi);
    [DllImport("dxva2.dll")] static extern bool GetNumberOfPhysicalMonitorsFromHMONITOR(IntPtr hMon, out uint n);
    [DllImport("dxva2.dll")] static extern bool GetPhysicalMonitorsFromHMONITOR(IntPtr hMon, uint n, [Out] PHYSICAL_MONITOR[] arr);
    [DllImport("dxva2.dll")] static extern bool DestroyPhysicalMonitor(IntPtr h);
    [DllImport("dxva2.dll")] static extern bool GetCapabilitiesStringLength(IntPtr h, out uint len);
    [DllImport("dxva2.dll", CharSet = CharSet.Ansi)] static extern bool CapabilitiesRequestAndCapabilitiesReply(IntPtr h, StringBuilder s, uint len);
    [DllImport("dxva2.dll")] static extern bool GetVCPFeatureAndVCPFeatureReply(IntPtr h, byte code, out uint type, out uint cur, out uint max);

    public class Mon { public string Device; public string Desc; public IntPtr H; }

    public static List<Mon> Open() {
        var res = new List<Mon>();
        var hms = new List<IntPtr>();
        EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, (h, dc, r, d) => { hms.Add(h); return true; }, IntPtr.Zero);
        foreach (var hm in hms) {
            var mi = new MONITORINFOEX(); mi.cbSize = Marshal.SizeOf(typeof(MONITORINFOEX));
            GetMonitorInfoW(hm, ref mi);
            uint n;
            if (!GetNumberOfPhysicalMonitorsFromHMONITOR(hm, out n) || n == 0) continue;
            var arr = new PHYSICAL_MONITOR[n];
            if (!GetPhysicalMonitorsFromHMONITOR(hm, n, arr)) continue;
            foreach (var pm in arr)
                res.Add(new Mon { Device = mi.szDevice, Desc = pm.szPhysicalMonitorDescription, H = pm.hPhysicalMonitor });
        }
        return res;
    }

    public static void Close(List<Mon> mons) { foreach (var m in mons) DestroyPhysicalMonitor(m.H); }

    public static string Caps(IntPtr h) {
        uint len;
        if (!GetCapabilitiesStringLength(h, out len) || len == 0) return null;
        var sb = new StringBuilder((int)len);
        return CapabilitiesRequestAndCapabilitiesReply(h, sb, len) ? sb.ToString() : null;
    }

    // Returns "cur/max (type t)" or null. Retries: DDC/CI replies are flaky.
    public static string Get(IntPtr h, byte code, int tries) {
        for (int i = 0; i < tries; i++) {
            uint t, cur, max;
            if (GetVCPFeatureAndVCPFeatureReply(h, code, out t, out cur, out max))
                return string.Format("{0}/{1} (type {2})", cur, max, t);
            System.Threading.Thread.Sleep(50);
        }
        return null;
    }
}
'@

# Standard codes from the LG caps string plus the vendor range (0xE0-0xFF).
$listed = 0x02, 0x04, 0x05, 0x06, 0x08, 0x0B, 0x0C, 0x10, 0x12, 0x14, 0x16, 0x18, 0x1A, 0x52, 0x60,
          0x87, 0xAC, 0xAE, 0xB2, 0xB6, 0xC6, 0xC8, 0xC9, 0xCA, 0xCC, 0xD6, 0xDF, 0xFD, 0xFF
$vendor = 0xE0..0xFF

$mons = [UfDdc]::Open()
try {
    $ddc = @()
    foreach ($m in $mons) {
        "== $($m.Device) | $($m.Desc)"
        $caps = [UfDdc]::Caps($m.H)
        if (-not $caps) { 'caps: not available (no DDC/CI on this path?)'; continue }
        "caps: $caps"
        $ddc += $m
        foreach ($c in ($listed + $vendor | Sort-Object -Unique)) {
            $v = [UfDdc]::Get($m.H, [byte]$c, 3)
            if ($v) { 'VCP 0x{0:X2}: {1}' -f $c, $v }
            elseif ($listed -contains $c) { 'VCP 0x{0:X2}: failed' -f $c }
        }
    }

    if ($WatchSeconds -gt 0 -and $ddc.Count -gt 0) {
        ''
        "Watching VCP values for $WatchSeconds s - cover the top bezel ~5 s, then shine a light."
        $watch = @(0x10, 0x52) + $vendor
        $last = @{}
        $end = (Get-Date).AddSeconds($WatchSeconds)
        while ((Get-Date) -lt $end) {
            foreach ($m in $ddc) {
                foreach ($c in $watch) {
                    $v = [UfDdc]::Get($m.H, [byte]$c, 1)
                    if (-not $v) { continue }
                    $k = "$($m.Device)/$c"
                    if ($last[$k] -ne $v) {
                        '{0:HH:mm:ss} {1} VCP 0x{2:X2}: {3}' -f (Get-Date), $m.Device, $c, $v
                        $last[$k] = $v
                    }
                }
            }
            Start-Sleep -Milliseconds 300
        }
        'watch done'
    }
} finally {
    [UfDdc]::Close($mons)
}
