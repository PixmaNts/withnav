#!/usr/bin/env python3
"""Writes the self-test images into a NEW folder given as the only argument (standard library
only). It refuses a path that already exists, so it never overwrites anything: the run owns the
folder and may delete it whole at the end.

ok.png          64x48, red background, blue square in the top-left quarter: a model that sees it
                can say so, which proves the image reached it.
truncated.png   ok.png cut at three quarters: the attachment store must refuse it.
corrupt.png     ok.png with four bytes flipped inside its compressed data: refused too.
big.png         about 4.3 MB of noise, past the 3.75 MiB (3,932,160 bytes) upload limit.
img1..img9.png  nine small images of different colours, one more than a message may carry.
drawing.svg     a well-formed SVG: yellow background, blue circle.
empty.svg       a self-closing <svg/>: still an SVG document.
fake.svg        prose that mentions <svg> but is not an SVG document: `read` returns it as text.
empty.png       zero bytes: refused at attach or upload.
notimage.png    text named .png: not an image, refused when uploaded.
"""
import os
import random
import struct
import sys
import zlib


def png(width, height, pixel):
    rows = b"".join(
        b"\x00" + b"".join(bytes(pixel(x, y)) for x in range(width)) for y in range(height)
    )

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(rows, 9))
        + chunk(b"IEND", b"")
    )


def main():
    out = sys.argv[1]
    try:
        os.makedirs(out)
    except FileExistsError:
        sys.exit(f"{out} already exists: choose a new folder name")

    def write(name, data):
        with open(os.path.join(out, name), "wb") as f:
            f.write(data)

    ok = png(64, 48, lambda x, y: (40, 60, 220) if x < 32 and y < 24 else (220, 30, 30))
    write("ok.png", ok)
    write("truncated.png", ok[: len(ok) * 3 // 4])
    corrupt = bytearray(ok)
    middle = len(corrupt) // 2
    for i in range(middle, middle + 4):
        corrupt[i] ^= 0xFF
    write("corrupt.png", bytes(corrupt))
    noise = random.Random(7)
    write("big.png", png(1200, 1200, lambda x, y: (noise.randrange(256), noise.randrange(256), noise.randrange(256))))
    colours = [(255, 0, 0), (0, 160, 0), (0, 0, 255), (255, 200, 0), (160, 0, 160),
               (0, 180, 180), (255, 120, 0), (90, 90, 90), (0, 0, 0)]
    for i, colour in enumerate(colours, 1):
        write(f"img{i}.png", png(16, 16, lambda x, y, c=colour: c))
    write("drawing.svg", b'<svg xmlns="http://www.w3.org/2000/svg" width="120" height="80">'
          b'<rect width="120" height="80" fill="#ffd400"/>'
          b'<circle cx="60" cy="40" r="28" fill="#1e50dc"/></svg>\n')
    write("empty.svg", b'<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>\n')
    write("fake.svg", b"Notes: the logo is an <svg> element, see design.md.\n")
    write("empty.png", b"")
    write("notimage.png", b"This is a text file with an image name.\n")
    print("\n".join(sorted(os.listdir(out))))


if __name__ == "__main__":
    main()
