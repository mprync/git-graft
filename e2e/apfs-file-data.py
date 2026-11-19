"""Observe APFS data extents without writing to the measured fixture.

Run with a disposable fixture root. --calibrate creates a separate temporary
directory on the given volume and checks the kernel's extent mapping first.
"""
import argparse
from collections import defaultdict
from contextlib import contextmanager
import ctypes
import json
import os
from pathlib import Path
import stat
import struct
import subprocess
import sys
import tempfile


# Darwin's 4-byte-packed struct log2phys from <sys/fcntl.h>.
F_LOG2PHYS_EXT = 65
LOG2PHYS = struct.Struct("=Iqq")
F_PUNCHHOLE = 99
PUNCHHOLE = struct.Struct("=IIqq")


class Unavailable(Exception):
    pass


def allocation_block(path):
    # df may return success with no mount row when its type filter excludes it.
    mount = subprocess.run(
        ["/bin/df", "-P", "-T", "apfs", path], capture_output=True, check=False
    )
    if mount.returncode or len(mount.stdout.strip().splitlines()) < 2:
        raise Unavailable("not-apfs")
    block = os.statvfs(path).f_frsize
    if block < 512 or block & (block - 1):
        raise Unavailable("invalid-allocation-block-size")
    return block


@contextmanager
def open_file(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        yield fd
    finally:
        os.close(fd)


def extent_at(fd, logical, wanted):
    import fcntl

    try:
        data = fcntl.fcntl(fd, F_LOG2PHYS_EXT, LOG2PHYS.pack(0, wanted, logical))
    except OSError as err:
        raise Unavailable("extent-query-failed") from err
    flags, contiguous, physical = LOG2PHYS.unpack(data)
    if flags or contiguous <= 0 or physical < 0:
        raise Unavailable("unmappable-data-extent")
    return physical, min(wanted, contiguous)


def blocks(fd, info, block):
    size = info.st_size
    if size == 0:
        return
    if info.st_flags & (stat.UF_COMPRESSED | stat.SF_DATALESS):
        raise Unavailable("compressed-or-dataless-file")
    if os.lseek(fd, 0, os.SEEK_HOLE) != size:
        raise Unavailable("sparse-file")
    if info.st_blocks * 512 < (size + block - 1) // block * block:
        raise Unavailable("inline-or-unallocated-file")
    logical = 0
    while logical < size:
        physical, length = extent_at(fd, logical, size - logical)
        logical += length
        if physical % block or (logical < size and logical % block):
            raise Unavailable("unaligned-data-extent")
        yield physical // block, (physical + length + block - 1) // block


def union_size(ranges):
    end = 0
    count = 0
    for start, stop in sorted(ranges):
        if stop > end:
            count += stop - max(start, end)
            end = stop
    return count


def measure(root):
    def fail_walk(err):
        raise err

    root_device = os.stat(root).st_dev
    sizes = {root_device: allocation_block(root)}
    extents = defaultdict(list)
    seen = set()
    for directory, _, files in os.walk(root, onerror=fail_walk, followlinks=False):
        for name in files:
            path = os.path.join(directory, name)
            before = os.lstat(path)
            if not stat.S_ISREG(before.st_mode):
                continue
            key = before.st_dev, before.st_ino
            if key in seen:
                continue
            seen.add(key)
            with open_file(path) as fd:
                info = os.fstat(fd)
                if not stat.S_ISREG(info.st_mode) or (info.st_dev, info.st_ino) != key:
                    raise Unavailable("file-changed-during-scan")
                if info.st_dev not in sizes:
                    sizes[info.st_dev] = allocation_block(path)
                extents[info.st_dev].extend(blocks(fd, info, sizes[info.st_dev]))
                after = os.fstat(fd)
                if (info.st_size, info.st_blocks, info.st_mtime_ns, info.st_ctime_ns) != (
                    after.st_size, after.st_blocks, after.st_mtime_ns, after.st_ctime_ns
                ):
                    raise Unavailable("file-changed-during-scan")
    return sum(union_size(ranges) * sizes[device] for device, ranges in extents.items())


def calibrate(volume):
    import fcntl

    def require(condition, reason):
        if not condition:
            raise Unavailable(reason)

    def write(path, data):
        with path.open("xb") as out:
            out.write(data)
            out.flush()
            os.fsync(out.fileno())

    block = allocation_block(volume)
    with tempfile.TemporaryDirectory(prefix="graft-apfs-calibration-", dir=volume) as temporary:
        root = Path(temporary)
        original, copy, clone = (root / name for name in ("original", "copy", "clone"))
        payload = os.urandom(8 * block + 17)
        expected = 9 * block
        (root / "empty").touch()
        require(measure(root) == 0, "calibration-empty")
        write(original, payload)
        require(measure(root) == expected, "calibration-rounded-length")
        write(copy, payload)
        require(measure(root) == 2 * expected, "calibration-byte-copy")
        clonefile = ctypes.CDLL(None, use_errno=True).clonefile
        clonefile.argtypes = (ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int)
        clonefile.restype = ctypes.c_int
        require(clonefile(os.fsencode(original), os.fsencode(clone), 0) == 0,
                "calibration-clonefile-unavailable")
        require(not os.path.samefile(original, clone), "calibration-clonefile-identity")
        os.link(original, root / "hardlink")
        (root / "symlink").symlink_to(copy)
        require(measure(root) == 2 * expected, "calibration-shared-blocks")
        offset = 2 * block
        with open_file(original) as source, open_file(clone) as target:
            first, _ = extent_at(source, offset, block)
            clone_first, _ = extent_at(target, offset, block)
            subrange, length = extent_at(target, offset + 17, block)
            require(first == clone_first and subrange == first + 17 and 0 < length <= block,
                    "calibration-offset-range")
        with clone.open("r+b") as out:
            out.seek(offset)
            out.write(os.urandom(block))
            out.flush()
            os.fsync(out.fileno())
        with open_file(clone) as fd:
            changed, _ = extent_at(fd, offset, block)
            require(changed != first, "calibration-diverged-block")
        require(2 * expected < measure(root) <= 3 * expected, "calibration-diverged-union")
        sparse = root / "sparse"
        write(sparse, payload)
        with sparse.open("r+b") as out:
            try:
                fcntl.fcntl(out, F_PUNCHHOLE, PUNCHHOLE.pack(0, 0, 2 * block, block))
            except OSError as err:
                raise Unavailable("calibration-punchhole-unavailable") from err
        try:
            measure(root)
        except Unavailable as err:
            require(str(err) == "sparse-file", "calibration-sparse-guard")
        else:
            raise Unavailable("calibration-sparse-guard")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--calibrate", action="store_true")
    args = parser.parse_args()
    try:
        if sys.platform != "darwin":
            raise Unavailable("unsupported-platform")
        if args.calibrate:
            calibrate(args.root)
            result = {"available": True}
        else:
            result = {"available": True, "bytes": str(measure(args.root))}
    except Unavailable as err:
        result = {"available": False, "reason": str(err)}
    except OSError as err:
        result = {"available": False, "reason": "observer-io-error", "errno": err.errno}
    print(json.dumps(result))


if __name__ == "__main__":
    main()
