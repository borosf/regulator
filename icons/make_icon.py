#!/usr/bin/env python3
"""Generate the regulator icon: a white monospaced lowercase r on black."""
import struct, zlib, os

SS = 3  # supersample factor

def render(size):
    """RGBA raster of the icon at `size` px, antialiased by supersampling."""
    px = [[(0, 0, 0, 0)] * size for _ in range(size)]
    # geometry in a normalized 256-unit space, scaled to `size`
    s = size / 256.0

    def rrect(x, y):
        # black rounded square, margin 16, radius 56
        m, r = 16 * s, 56 * s
        dx = max(m - x, x - (size - m), 0)
        dy = max(m - y, y - (size - m), 0)
        return dx * dx + dy * dy <= r * r

    def letter(x, y):
        x, y = x / s, y / s  # work in 256-space
        # monospaced lowercase r: vertical stem, small shoulder bowl
        # baseline y=184, x-height top y=88
        # stem: x 98..130, full x-height to baseline
        if 98 <= x <= 130 and 88 <= y <= 184:
            return True
        # bowl: quarter-annulus centered (102,124), outer r 36, inner r 8,
        # sweeping from the stem top out to the right
        dx, dy = x - 102, y - 124
        if dx >= 0 and dy <= 4 and 8 * 8 <= dx * dx + dy * dy <= 36 * 36:
            return True
        return False

    bg = (0, 0, 0)
    fg = (255, 255, 255)
    for iy in range(size):
        for ix in range(size):
            hit = acc = 0
            lit = lacc = 0
            for sy in range(SS):
                for sx in range(SS):
                    x = ix + (sx + 0.5) / SS
                    y = iy + (sy + 0.5) / SS
                    acc += 1
                    if rrect(x, y):
                        hit += 1
                        if letter(x, y):
                            lacc += 1
            a = hit / acc
            l = lacc / acc
            col = (
                int(bg[0] * (1 - l) + fg[0] * l),
                int(bg[1] * (1 - l) + fg[1] * l),
                int(bg[2] * (1 - l) + fg[2] * l),
                int(255 * a),
            )
            px[iy][ix] = col
    return px

def downscale(px, n, m):
    """Box-sample an n px raster down to m px."""
    out = []
    for y in range(m):
        row = []
        y0, y1 = y * n // m, (y + 1) * n // m
        for x in range(m):
            x0, x1 = x * n // m, (x + 1) * n // m
            r = g = b = a = c = 0
            for yy in range(y0, y1):
                for xx in range(x0, x1):
                    pr, pg, pb, pa = px[yy][xx]
                    r += pr * pa; g += pg * pa; b += pb * pa; a += pa; c += 1
            if a == 0:
                row.append((0, 0, 0, 0))
            else:
                row.append((r // a, g // a, b // a, (a + c // 2) // c))
        out.append(row)
    return out

def png_bytes(px):
    h = len(px); w = len(px[0])
    raw = b""
    for row in px:
        raw += b"\x00" + b"".join(struct.pack("4B", *p) for p in row)

    def chunk(tag, data):
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c))
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))

def bmp_dib(px):
    """32-bit BGRA DIB (bottom-up, double-height AND mask) for small ICO sizes."""
    h = len(px); w = len(px[0])
    xor = b""
    for row in reversed(px):
        for r, g, b, a in row:
            xor += struct.pack("4B", b, g, r, a)
    and_stride = ((w + 31) // 32) * 4
    and_mask = bytes(and_stride * h)
    hdr = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, len(xor) + len(and_mask), 0, 0, 0, 0)
    return hdr + xor + and_mask

def ico(entries):
    """entries: list of (size, payload) where payload is PNG or DIB bytes."""
    n = len(entries)
    off = 6 + 16 * n
    body = b""
    head = struct.pack("<HHH", 0, 1, n)
    for i, (size, data) in enumerate(entries):
        is_png = data[:4] == b"\x89PNG"
        fmt = 14 if is_png else 0
        head += struct.pack("<BBBBHHII", size % 256, 0, 0, 0, 1, 32, len(data), off)
        body += data
        off += len(data)
    return head + body

base = render(256)
open(os.path.join(os.path.dirname(__file__), "regulator-preview.png"), "wb").write(png_bytes(base))
entries = [(256, png_bytes(base))]
for size in (48, 32, 16):
    entries.append((size, bmp_dib(downscale(base, 256, size))))
open(os.path.join(os.path.dirname(__file__), "regulator.ico"), "wb").write(ico(entries))
print("written")
