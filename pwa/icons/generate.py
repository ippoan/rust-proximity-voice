#!/usr/bin/env python3
"""アイコンを生成する。**ビルドステップではない** — 出力の PNG を repo に置く。

デザインは近接ボイスの見立てそのままで、中心の点 (自分) と、そこから広がる
可聴範囲の輪。60m で切れることを、いちばん外側の輪を薄くして表す。

    python3 pwa/icons/generate.py
"""
import math, struct, zlib, os

BG = (0x14, 0x16, 0x1a)
FG = (0x6e, 0xa8, 0xfe)

def png(path, size, pixels):
    raw = b''.join(b'\x00' + bytes(v for px in row for v in px) for row in pixels)
    def chunk(tag, data):
        c = tag + data
        return struct.pack('>I', len(data)) + c + struct.pack('>I', zlib.crc32(c) & 0xffffffff)
    out = (b'\x89PNG\r\n\x1a\n'
           + chunk(b'IHDR', struct.pack('>IIBBBBB', size, size, 8, 2, 0, 0, 0))
           + chunk(b'IDAT', zlib.compress(raw, 9))
           + chunk(b'IEND', b''))
    with open(path, 'wb') as f:
        f.write(out)

def blend(a, b, t):
    t = max(0.0, min(1.0, t))
    return tuple(int(round(a[i] + (b[i] - a[i]) * t)) for i in range(3))

def render(size):
    c = (size - 1) / 2.0
    # maskable の safe zone (中央 80%) に収める
    unit = size / 2.0 * 0.80
    rings = [(0.30, 1.00), (0.55, 0.55), (0.80, 0.22)]   # (半径比, 濃さ)
    dot_r = unit * 0.13
    ring_w = max(1.0, size * 0.035)
    rows = []
    for y in range(size):
        row = []
        for x in range(size):
            # 4x スーパーサンプリング
            acc, hits = 0.0, 0
            for oy in (-0.25, 0.25):
                for ox in (-0.25, 0.25):
                    dx, dy = x + ox - c, y + oy - c
                    d = math.hypot(dx, dy)
                    v = 1.0 if d <= dot_r else 0.0
                    if v == 0.0:
                        for rr, alpha in rings:
                            if abs(d - unit * rr) <= ring_w / 2:
                                v = alpha
                                break
                    acc += v
                    hits += 1
            row.append(blend(BG, FG, acc / hits))
        rows.append(row)
    return rows

if __name__ == '__main__':
    here = os.path.dirname(os.path.abspath(__file__))
    for n in (192, 512):
        png(os.path.join(here, 'icon-%d.png' % n), n, render(n))
        print('icon-%d.png' % n)
