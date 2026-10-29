"""cutout.py in.png out.png radius x0,y0,x1,y1 [...]: keep only the given rounded
rectangles of a screenshot, transparent elsewhere, cropped to their union."""
import sys, os
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from crop import load, save
src, dst, rad = sys.argv[1], sys.argv[2], float(sys.argv[3])
rects = [tuple(map(int, a.split(','))) for a in sys.argv[4:]]
w, h, bpp, rows = load(src)
X0 = min(r[0] for r in rects); Y0 = min(r[1] for r in rects); X1 = max(r[2] for r in rects); Y1 = max(r[3] for r in rects)
def inside(px, py, r):
    x0, y0, x1, y1 = r
    if px < x0 or px >= x1 or py < y0 or py >= y1: return 0.0
    cx = min(max(px, x0 + rad), x1 - rad); cy = min(max(py, y0 + rad), y1 - rad)
    d = ((px - cx) ** 2 + (py - cy) ** 2) ** 0.5
    return max(0.0, min(1.0, rad + 0.5 - d)) if d > 0 else 1.0
out = []
for y in range(Y0, Y1):
    line = bytearray()
    for x in range(X0, X1):
        a = max(inside(x + 0.5, y + 0.5, r) for r in rects)
        p = rows[y][x * bpp:x * bpp + 3]
        line += bytes(p) + bytes([int(a * 255)])
    out.append(line)
save(dst, X1 - X0, Y1 - Y0, out)
print(dst, X1 - X0, 'x', Y1 - Y0)
