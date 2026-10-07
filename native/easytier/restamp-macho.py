#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Sets the deployment target of 64-bit Mach-O objects in place.

Rust's prebuilt standard-library objects carry older deployment targets than
the crates compiled for Heeler, some as a 16-byte LC_VERSION_MIN_IPHONEOS that
vtool cannot widen into LC_BUILD_VERSION inside an object file. This rewrites
only the version fields: LC_BUILD_VERSION.minos (the platform must already
match) and LC_VERSION_MIN_IPHONEOS.version (device objects only).

    restamp-macho.py <platform> <major.minor> <object>...
"""

import struct
import sys

MH_MAGIC_64 = 0xFEEDFACF
LC_VERSION_MIN_IPHONEOS = 0x25
LC_BUILD_VERSION = 0x32
PLATFORM_IOS = 2


def restamp(path: str, platform: int, version: int) -> bool:
    with open(path, "r+b") as handle:
        data = bytearray(handle.read())
        magic, _, _, _, ncmds, _ = struct.unpack_from("<IiiIII", data, 0)
        if magic != MH_MAGIC_64:
            raise SystemExit(f"error: {path} is not a 64-bit little-endian Mach-O object")
        offset = 32
        changed = False
        for _ in range(ncmds):
            cmd, cmdsize = struct.unpack_from("<II", data, offset)
            if cmd == LC_BUILD_VERSION:
                found_platform, minos = struct.unpack_from("<II", data, offset + 8)
                if found_platform != platform:
                    raise SystemExit(f"error: {path} is for platform {found_platform}, expected {platform}")
                if minos != version:
                    struct.pack_into("<I", data, offset + 12, version)
                    changed = True
            elif cmd == LC_VERSION_MIN_IPHONEOS:
                if platform != PLATFORM_IOS:
                    raise SystemExit(f"error: {path} has LC_VERSION_MIN_IPHONEOS in a non-device slice")
                (found,) = struct.unpack_from("<I", data, offset + 8)
                if found != version:
                    struct.pack_into("<I", data, offset + 8, version)
                    changed = True
            offset += cmdsize
        if changed:
            handle.seek(0)
            handle.write(data)
        return changed


def main() -> int:
    platform = int(sys.argv[1])
    major, minor = (int(part) for part in sys.argv[2].split("."))
    version = (major << 16) | (minor << 8)
    count = sum(restamp(path, platform, version) for path in sys.argv[3:])
    print(f"    restamped {count} of {len(sys.argv) - 3} objects to {sys.argv[2]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
