"""The two files every fixture is made from: 4 KiB of seeded noise, in which the branch filters
find calls to rewrite, and some text. Run in a fixture directory to write them there, dated
2025-12-31 23:00 UTC; `manifest.py` imports them."""

import os
import random

MODIFIED = 1767222000


def binary():
    rng = random.Random(7)
    return bytes(rng.getrandbits(8) for _ in range(4096))


def text():
    return "".join(f"line {i}: the quick brown fox jumps over the lazy dog\n" for i in range(40)).encode()


if __name__ == "__main__":
    for name, data in (("bin.dat", binary()), ("text.txt", text())):
        with open(name, "wb") as out:
            out.write(data)
        os.utime(name, (MODIFIED, MODIFIED))
