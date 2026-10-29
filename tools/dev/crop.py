"""crop.py in.png out.png x0 y0 x1 y1 [scale]: crop (and optionally upscale) a PNG. Pure Python."""
import sys, zlib, struct
def load(p):
    d = open(p, 'rb').read(); i = 8; idat = b''
    while i < len(d):
        l = struct.unpack('>I', d[i:i+4])[0]; t = d[i+4:i+8]
        if t == b'IHDR': w, h, bd, ct = struct.unpack('>IIBB', d[i+8:i+18])
        if t == b'IDAT': idat += d[i+8:i+8+l]
        i += 12 + l
    bpp = 4 if ct == 6 else 3
    raw = zlib.decompress(idat); stride = w * bpp; rows = []; prev = bytearray(stride); pos = 0
    for _ in range(h):
        f = raw[pos]; line = bytearray(raw[pos+1:pos+1+stride]); pos += 1 + stride
        for x in range(stride):
            a = line[x-bpp] if x >= bpp else 0; b = prev[x]; c = prev[x-bpp] if x >= bpp else 0
            if f == 1: line[x] = (line[x] + a) & 255
            elif f == 2: line[x] = (line[x] + b) & 255
            elif f == 3: line[x] = (line[x] + (a + b) // 2) & 255
            elif f == 4:
                pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2*c)
                line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        rows.append(line); prev = line
    return w, h, bpp, rows
def save(p, w, h, rows):
    def chunk(t, d): return struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d) & 0xffffffff)
    raw = b''.join(b'\0' + bytes(r) for r in rows)
    open(p, 'wb').write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 6, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b''))


if __name__ == "__main__":
    src, dst = sys.argv[1], sys.argv[2]
    x0, y0, x1, y1 = map(int, sys.argv[3:7]); k = int(sys.argv[7]) if len(sys.argv) > 7 else 1
    w, h, bpp, rows = load(src)
    x0, y0, x1, y1 = max(0, x0), max(0, y0), min(w, x1), min(h, y1)
    out = []
    for y in range(y0, y1):
        r = rows[y]; line = bytearray()
        for x in range(x0, x1):
            px = bytes(r[x*bpp:x*bpp+3]) + b'\xff'
            line += px * k
        for _ in range(k): out.append(line)
    save(dst, (x1 - x0) * k, (y1 - y0) * k, out)
    print(dst, (x1 - x0) * k, 'x', (y1 - y0) * k)
