"""check_h264.py in.h264 first-frame.png: decode an Annex B H.264 file (e.g. from
`opendisplay-win probe`) with PyAV, save the first frame, print frame/keyframe counts,
profile and colour range. Needs `pip install --user av`; run without `python3 -I`."""
import sys
import av

path, png = sys.argv[1], sys.argv[2]
c = av.open(path, format="h264")
n = keys = 0
for f in c.decode(video=0):
    n += 1
    keys += f.key_frame
    if n == 1:
        f.to_image().save(png)
        print("first frame", f.width, "x", f.height, f.format.name)
print("decoded frames", n, "keyframes", keys)
s = c.streams.video[0].codec_context
print("profile", s.profile, "color range", s.color_range, "colorspace", s.colorspace)
