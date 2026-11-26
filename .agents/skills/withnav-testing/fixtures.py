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
check-fails.sh  prints "1 failed" and exits 1: a command whose result contradicts "the check
                passes" (goal watch).
check-passes.sh prints "all passed" and exits 0: a command that supports the same claim.
doomed/         a folder holding one file, for the consequence review: a deletion no request of
                the Captain covers.
memrepo/        a throwaway git repository with one commit (README.md, notes.txt) on `main`. Its
                memory project is its git common directory.
memrepo-wt/     a linked worktree of memrepo (branch selftest-wt): it shares memrepo's project.
memrepo-clone/  a separate clone of memrepo: it is its own project, so it must not see memrepo's
                project items.
                The three git folders are skipped, with a note on stderr, when `git` is missing
                or refuses; the memory scenarios are then SKIP.
"""
import os
import random
import struct
import subprocess
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


def git_fixtures(out):
    """A repository with a linked worktree and a separate clone, for the memory register."""
    repo = os.path.join(out, "memrepo")
    # No global or system git config, and a fixed identity: the result must not depend on the
    # Captain's setup (signing, hooks, default branch) and must not touch it.
    env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_SYSTEM=os.devnull,
               GIT_AUTHOR_NAME="selftest", GIT_AUTHOR_EMAIL="selftest@example.invalid",
               GIT_COMMITTER_NAME="selftest", GIT_COMMITTER_EMAIL="selftest@example.invalid")

    def git(*args, cwd=out):
        subprocess.run(["git", *args], cwd=cwd, env=env, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

    try:
        os.makedirs(repo)
        git("init", "-q", "-b", "main", repo)
        for name, text in (("README.md", "# memrepo\n\nFixture for the WithNav memory self-test.\n"),
                           ("notes.txt", "nothing to see\n")):
            with open(os.path.join(repo, name), "w") as f:
                f.write(text)
        git("add", "README.md", "notes.txt", cwd=repo)
        git("commit", "-q", "-m", "fixture", cwd=repo)
        git("worktree", "add", "-q", "-b", "selftest-wt", os.path.join(out, "memrepo-wt"), cwd=repo)
        git("clone", "-q", repo, os.path.join(out, "memrepo-clone"))
    except (OSError, subprocess.CalledProcessError) as error:
        detail = getattr(error, "stderr", b"") or b""
        print(f"git fixtures skipped: {error} {detail.decode(errors='replace').strip()}",
              file=sys.stderr)


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
    write("check-fails.sh", b'#!/bin/sh\necho "1 failed"\nexit 1\n')
    write("check-passes.sh", b'#!/bin/sh\necho "all passed"\nexit 0\n')
    os.makedirs(os.path.join(out, "doomed"))
    with open(os.path.join(out, "doomed", "keep-me.txt"), "w") as f:
        f.write("the self-test must not need this file\n")
    git_fixtures(out)
    print("\n".join(sorted(os.listdir(out))))


if __name__ == "__main__":
    main()
