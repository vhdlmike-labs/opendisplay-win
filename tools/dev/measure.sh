#!/bin/bash
# measure.sh <label> [extra run args]: stream for 32 s with an animation on the
# non-primary screen; report the sender's GPU engine use, CPU time and receiver stats.
label=$1; shift
# EXE: the opendisplay-win.exe to test (a Windows copy; default: the release build).
EXE=${EXE:-$(dirname "$0")/../../target/x86_64-pc-windows-gnullvm/release/opendisplay-win.exe}
WTEMP=$(powershell.exe -NoProfile -Command '$env:TEMP' | tr -d '\r')
LOGW="$WTEMP\\od-m-$label.log"
LOGL=$(wslpath "$WTEMP")/od-m-$label.log
rm -f $LOGL
$EXE run --for 32 --log-file "$LOGW" "$@" >/dev/null 2>&1 &
sleep 9
read -r -d '' ANIM <<'PSEOF'
Add-Type -AssemblyName System.Windows.Forms,System.Drawing
Add-Type -Namespace D -Name P -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);'
[void][D.P]::SetProcessDpiAwarenessContext([IntPtr]-4)
$s = [System.Windows.Forms.Screen]::AllScreens | Where-Object { -not $_.Primary } | Select-Object -First 1
if (-not $s) { 'no second screen'; exit }
$f = New-Object Windows.Forms.Form
$f.FormBorderStyle = 'None'; $f.StartPosition = 'Manual'; $f.Bounds = $s.Bounds; $f.TopMost = $true; $f.BackColor = 'Black'; $f.ShowInTaskbar = $false
$font = New-Object Drawing.Font('Segoe UI', 48)
$script:i = 0
$t = New-Object Windows.Forms.Timer; $t.Interval = 15
$t.Add_Tick({ $script:i++; $f.Invalidate() })
$f.Add_Paint({ param($o, $e)
  $g = $e.Graphics; $w = $f.ClientSize.Width; $h = $f.ClientSize.Height
  $x = ($script:i * 12) % $w
  $g.FillRectangle([Drawing.Brushes]::OrangeRed, $x, $h / 4, 200, 200)
  $g.DrawString("OpenDisplay test frame $($script:i)", $font, [Drawing.Brushes]::White, 40, 40) })
$stop = (Get-Date).AddSeconds(20)
$t2 = New-Object Windows.Forms.Timer; $t2.Interval = 500
$t2.Add_Tick({ if ((Get-Date) -gt $stop) { $f.Close() } })
$t.Start(); $t2.Start()
[void]$f.ShowDialog()
PSEOF
powershell.exe -NoProfile -Command "$ANIM" >/dev/null 2>&1 &
sleep 4
read -r -d '' MEAS <<'PSEOF'
$p = Get-Process opendisplay-win
$c0 = $p.TotalProcessorTime.TotalMilliseconds
$samples = Get-Counter -Counter "\GPU Engine(pid_$($p.Id)_*)\Utilization Percentage" -SampleInterval 1 -MaxSamples 12 -ErrorAction SilentlyContinue
$c1 = (Get-Process -Id $p.Id).TotalProcessorTime.TotalMilliseconds
$by = @{}
foreach ($set in $samples) { foreach ($c in $set.CounterSamples) {
  $eng = ($c.InstanceName -replace '.*engtype_', '')
  $by[$eng] = $by[$eng] + $c.CookedValue } }
$n = [Math]::Max(1, $samples.Count)
$total = 0; $parts = @()
foreach ($k in $by.Keys) { $v = $by[$k] / $n; $total += $v; if ($v -ge 0.05) { $parts += ('{0} {1:N1}%' -f $k, $v) } }
'GPU total {0:N1}%  ({1})' -f $total, ($parts -join ', ')
'CPU {0:N1}% of one core' -f (($c1 - $c0) / 12000 * 100)
PSEOF
powershell.exe -NoProfile -Command "$MEAS" | tr -d '\r'
wait
grep -o '"fps":[0-9.]*\|"mbps":[0-9.]*' $LOGL | paste - - | tail -4 | tr '\n' ' '; echo
grep -o 'battery saver: .*\|capturing .*' $LOGL | cut -c1-110 | sort -u
