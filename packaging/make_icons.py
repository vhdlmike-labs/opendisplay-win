"""Draw the OpenDisplay icons (assets/*.ico): a landscape tablet whose screen
colour shows the state. Pure Python (zlib + struct), no image libraries.
  opendisplay.ico  grey screen   - app icon, waiting for a device
  active.ico       blue screen   - streaming (second screen)
  warning.ico      amber screen  - mirroring / problem"""
import struct, sys, zlib, os

SIZES = [16, 20, 24, 32, 40, 48, 64, 256]
BODY = (38, 40, 44)
OUTLINE = (232, 234, 237)
SCREENS = {"opendisplay": (154, 160, 166), "active": (47, 140, 255), "warning": (242, 165, 22)}

def rrect(px, py, x0, y0, x1, y1, r):
    """Inside test for a rounded rectangle."""
    if px < x0 or px > x1 or py < y0 or py > y1:
        return False
    cx = min(max(px, x0 + r), x1 - r)
    cy = min(max(py, y0 + r), y1 - r)
    return (px - cx) ** 2 + (py - cy) ** 2 <= r * r

def render(n, screen):
    ss = 4
    out = bytearray()
    o = max(1.0, n / 24)          # outline width
    bx0, by0, bx1, by1 = 0.04 * n, 0.17 * n, 0.96 * n, 0.83 * n
    br = 0.13 * n
    inset = 0.10 * n
    sx0, sy0, sx1, sy1 = bx0 + inset, by0 + inset, bx1 - inset, by1 - inset
    for y in range(n):
        out.append(0)  # PNG filter byte
        for x in range(n):
            acc = [0, 0, 0, 0]
            for j in range(ss):
                for i in range(ss):
                    px, py = x + (i + 0.5) / ss, y + (j + 0.5) / ss
                    if not rrect(px, py, bx0, by0, bx1, by1, br):
                        continue
                    if rrect(px, py, sx0, sy0, sx1, sy1, br * 0.35):
                        c = screen
                    elif rrect(px, py, bx0 + o, by0 + o, bx1 - o, by1 - o, max(br - o, 0)):
                        c = BODY
                    else:
                        c = OUTLINE
                    acc[0] += c[0]; acc[1] += c[1]; acc[2] += c[2]; acc[3] += 255
            k = ss * ss
            a = acc[3] // k
            if a:
                cov = acc[3] / 255
                out += bytes([int(acc[0] / cov), int(acc[1] / cov), int(acc[2] / cov), a])
            else:
                out += b"\0\0\0\0"
    return bytes(out)

def png(n, rgba):
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    ihdr = struct.pack(">IIBBBBB", n, n, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(rgba, 9)) + chunk(b"IEND", b"")

def ico(images):
    head = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries, data = b"", b""
    for n, blob in images:
        entries += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(blob), offset + len(data))
        data += blob
    return head + entries + data

outdir = sys.argv[1]
os.makedirs(outdir, exist_ok=True)
for name, screen in SCREENS.items():
    imgs = [(n, png(n, render(n, screen))) for n in SIZES]
    open(os.path.join(outdir, name + ".ico"), "wb").write(ico(imgs))
    open(os.path.join(outdir, name + "-32.png"), "wb").write(imgs[3][1])
print("icons written to", outdir)
