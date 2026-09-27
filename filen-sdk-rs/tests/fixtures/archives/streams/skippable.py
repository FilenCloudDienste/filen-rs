# a skippable frame (magic 0x184D2A5?, little-endian length, then that many bytes), which lz4 and
# zstd share and neither CLI writes: `python3 skippable.py 0x184D2A50 metadata > frame.bin`
import struct, sys
data = sys.argv[2].encode()
sys.stdout.buffer.write(struct.pack("<II", int(sys.argv[1], 16), len(data)) + data)
