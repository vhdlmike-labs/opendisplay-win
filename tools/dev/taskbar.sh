#!/bin/bash
# taskbar.sh show|autohide: switch the taskbar's auto-hide off (show) or on, e.g.
# around screenshots of the tray icon. ABM_SETSTATE: 2 = always on top, 1 = auto-hide.
case "$1" in show) STATE=2 ;; autohide) STATE=1 ;; *) echo "usage: $0 show|autohide"; exit 2 ;; esac
read -r -d '' PS <<'PSEOF'
Add-Type -Namespace W -Name A -MemberDefinition '[StructLayout(LayoutKind.Sequential)] public struct APPBARDATA { public int cbSize; public IntPtr hWnd; public uint uCallbackMessage; public uint uEdge; public int l, t, r, b; public IntPtr lParam; } [DllImport("shell32.dll")] public static extern UIntPtr SHAppBarMessage(uint msg, ref APPBARDATA d); [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string t);'
$d = New-Object W.A+APPBARDATA; $d.cbSize = [Runtime.InteropServices.Marshal]::SizeOf($d); $d.hWnd = [W.A]::FindWindow('Shell_TrayWnd', [NullString]::Value)
$d.lParam = [IntPtr]STATE
[void][W.A]::SHAppBarMessage(10, [ref]$d)
'taskbar state set to STATE'
PSEOF
powershell.exe -NoProfile -Command "${PS//STATE/$STATE}" | tr -d '\r'
