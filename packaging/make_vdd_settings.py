"""Write vdd_settings.xml for the bundled MTT Virtual Display Driver: one
virtual monitor whose modes are the panels of current iPads and iPhones in
both orientations (opendisplay-win sizes the monitor to the receiver's panel),
plus a few desktop sizes.

Keep the mode count low: the driver silently creates no monitor at all with
more than ~85 modes (measured: 84 worked, 88 did not). Every resolution is
one mode at 60 Hz and there are no global refresh rates (each would add one
mode per resolution), so this file has len(modes) modes."""
import sys

# Physical panel pixels (landscape).
PANELS = [
    (2752, 2064),  # iPad Pro 13" (M4)
    (2732, 2048),  # iPad Pro 12.9", iPad Air 13"
    (2420, 1668),  # iPad Pro 11" (M4)
    (2388, 1668),  # iPad Pro 11" (1st-4th gen), iPad Air 11"
    (2360, 1640),  # iPad Air (4th/5th gen), iPad (10th/11th gen)
    (2266, 1488),  # iPad mini (6th/7th gen)
    (2224, 1668),  # iPad Pro 10.5", iPad Air (3rd gen)
    (2160, 1620),  # iPad (7th-9th gen)
    (2048, 1536),  # iPad mini 5, older iPads
    (2868, 1320),  # iPhone 16 Pro Max
    (2796, 1290),  # iPhone 14 Pro Max, 15 Plus/Pro Max, 16 Plus
    (2778, 1284),  # iPhone 12/13 Pro Max, 14 Plus
    (2622, 1206),  # iPhone 16 Pro
    (2556, 1179),  # iPhone 14 Pro, 15, 15 Pro, 16
    (2532, 1170),  # iPhone 12, 13, 14
    (2436, 1125),  # iPhone X, XS, 11 Pro
    (2340, 1080),  # iPhone 12/13 mini
    (1792, 828),   # iPhone XR, 11
    (1334, 750),   # iPhone SE
]
DESKTOP = [(1920, 1080), (2560, 1440), (3840, 2160)]

modes = []
for w, h in PANELS:
    modes += [(w, h), (h, w)]
modes += DESKTOP
res = "".join(
    f"""        <resolution>
            <width>{w}</width>
            <height>{h}</height>
            <refresh_rate>60</refresh_rate>
        </resolution>
""" for w, h in modes)

xml = f"""<?xml version='1.0' encoding='utf-8'?>
<vdd_settings>
    <monitors>
        <count>1</count>
    </monitors>
    <gpu>
        <friendlyname>default</friendlyname>
    </gpu>
    <global>
        <!-- No global refresh rates: each one adds a mode per resolution, and
             the driver creates no monitor beyond ~85 modes. -->
    </global>
    <resolutions>
{res}    </resolutions>
    <options>
        <CustomEdid>false</CustomEdid>
        <PreventSpoof>false</PreventSpoof>
        <EdidCeaOverride>false</EdidCeaOverride>
        <HardwareCursor>true</HardwareCursor>
        <SDR10bit>false</SDR10bit>
        <HDRPlus>false</HDRPlus>
        <logging>false</logging>
        <debuglogging>false</debuglogging>
    </options>
</vdd_settings>
"""
assert len(modes) <= 60, f"{len(modes)} modes: the driver fails beyond ~85"
open(sys.argv[1], "w", encoding="utf-8", newline="\r\n").write(xml)
