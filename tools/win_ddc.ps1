# Read-only DDC/CI query through Windows' own monitor API (dxva2), i.e. via
# the graphics driver / DisplayPort AUX - not through the display's I2C bridge.
# For each monitor: capabilities string and a few VCP values (Get VCP only).

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
    [DllImport("dxva2.dll")] static extern bool DestroyPhysicalMonitors(uint n, PHYSICAL_MONITOR[] arr);
    [DllImport("dxva2.dll")] static extern bool GetCapabilitiesStringLength(IntPtr h, out uint len);
    [DllImport("dxva2.dll", CharSet = CharSet.Ansi)] static extern bool CapabilitiesRequestAndCapabilitiesReply(IntPtr h, StringBuilder s, uint len);
    [DllImport("dxva2.dll")] static extern bool GetVCPFeatureAndVCPFeatureReply(IntPtr h, byte code, out uint type, out uint cur, out uint max);

    public static List<string> Run(byte[] codes) {
        var o = new List<string>();
        var mons = new List<IntPtr>();
        EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, (h, dc, r, d) => { mons.Add(h); return true; }, IntPtr.Zero);
        foreach (var hm in mons) {
            var mi = new MONITORINFOEX(); mi.cbSize = Marshal.SizeOf(typeof(MONITORINFOEX));
            GetMonitorInfoW(hm, ref mi);
            uint n;
            if (!GetNumberOfPhysicalMonitorsFromHMONITOR(hm, out n) || n == 0) {
                o.Add(mi.szDevice + ": no physical monitors (err " + Marshal.GetLastWin32Error() + ")"); continue;
            }
            var arr = new PHYSICAL_MONITOR[n];
            if (!GetPhysicalMonitorsFromHMONITOR(hm, n, arr)) { o.Add(mi.szDevice + ": GetPhysicalMonitors failed"); continue; }
            foreach (var pm in arr) {
                o.Add("== " + mi.szDevice + " | " + pm.szPhysicalMonitorDescription);
                uint len;
                if (GetCapabilitiesStringLength(pm.hPhysicalMonitor, out len) && len > 0) {
                    var sb = new StringBuilder((int)len);
                    o.Add(CapabilitiesRequestAndCapabilitiesReply(pm.hPhysicalMonitor, sb, len)
                        ? "caps: " + sb.ToString() : "caps: request failed");
                } else {
                    o.Add("caps: not available (no DDC/CI on this path?)");
                }
                foreach (var c in codes) {
                    uint t, cur, max;
                    if (GetVCPFeatureAndVCPFeatureReply(pm.hPhysicalMonitor, c, out t, out cur, out max))
                        o.Add(string.Format("VCP 0x{0:X2}: cur={1} max={2} type={3}", c, cur, max, t));
                    else
                        o.Add(string.Format("VCP 0x{0:X2}: failed", c));
                }
            }
            DestroyPhysicalMonitors(n, arr);
        }
        return o;
    }
}
'@

# 0x10 brightness, 0x12 contrast, 0x60 input, 0xC9 firmware level, 0xDF VCP version
foreach ($l in [UfDdc]::Run([byte[]](0x10, 0x12, 0x60, 0xC9, 0xDF))) { $l }
