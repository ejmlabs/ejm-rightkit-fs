#!/usr/bin/env python3
"""Native, two-principal macOS probe. Exit 77 means the test was not run."""

import errno
import fcntl
import os
import selectors
import subprocess
import sys
import tempfile
from pathlib import Path


def line(process: subprocess.Popen[bytes], expected: bytes) -> None:
    assert process.stdout is not None
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        if not selector.select(10):
            raise RuntimeError(f"timed out waiting for {expected!r}")
        actual = process.stdout.readline().strip()
    if actual != expected:
        raise RuntimeError(f"expected {expected!r}, got {actual!r}")


def run() -> int:
    if sys.platform != "darwin":
        print("not run: native macOS required")
        return 77
    principal = "nobody"
    try:
        other_uid = int(subprocess.check_output(["id", "-u", principal], timeout=5))
        if other_uid == os.getuid():
            raise RuntimeError("probe principal must differ from the owner")
        subprocess.run(["sudo", "-n", "-u", principal, "/usr/bin/true"], check=True, timeout=5)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exc:
        print(f"not run: a distinct sudo-accessible principal is required: {exc}")
        return 77
    source = Path(__file__).with_name("guard-open-probe.c")
    with tempfile.TemporaryDirectory(prefix="rightkit-acl-probe-", dir="/tmp") as folder:
        root = Path(folder)
        root.chmod(0o711)
        binary = root / "guard-open-probe"
        subprocess.run(["cc", "-Wall", "-Wextra", "-Werror", str(source), "-o", str(binary)], check=True, timeout=30)
        acl = f"user:{principal} allow read,file_inherit,only_inherit"
        result = subprocess.run(["chmod", "+a", acl, str(root)], capture_output=True, timeout=5)
        if result.returncode:
            print("not run: unable to create inherited ACL:", result.stderr.decode(errors="replace"))
            return 77
        for mode in ("control", "private"):
            temporary, published = root / f"{mode}.tmp", root / f"{mode}.takeover"
            process = subprocess.Popen([str(binary), mode, str(temporary), str(published)],
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE)
            assert process.stdin is not None
            try:
                line(process, b"created")
                access = subprocess.run(["sudo", "-n", "-u", principal, "/bin/cat", str(temporary)],
                                        capture_output=True, timeout=5)
                if mode == "control" and access.returncode != 0:
                    raise RuntimeError("control did not demonstrate inherited read access")
                if mode == "private" and access.returncode == 0:
                    raise RuntimeError("private file was readable before initialization")
                if mode == "private" and b"permission denied" not in access.stderr.lower():
                    raise RuntimeError("private open failed for a reason other than permission denial")
                process.stdin.write(b"x")
                process.stdin.flush()
                line(process, b"published")
                if temporary.stat().st_ino != published.stat().st_ino:
                    raise RuntimeError("published path is not the created inode")
                with published.open("rb") as guard:
                    if guard.read() != b"rightkit-lease-guard v2\n":
                        raise RuntimeError("published guard was not initialized")
                    try:
                        fcntl.flock(guard.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except OSError as exc:
                        if exc.errno not in (errno.EAGAIN, errno.EACCES):
                            raise
                    else:
                        raise RuntimeError("published guard was not locked")
                access = subprocess.run(["sudo", "-n", "-u", principal, "/bin/cat", str(published)],
                                        capture_output=True, timeout=5)
                if (access.returncode == 0) != (mode == "control"):
                    raise RuntimeError(f"unexpected access after publication for {mode}")
                if mode == "private" and b"permission denied" not in access.stderr.lower():
                    raise RuntimeError("published open failed for a reason other than permission denial")
                process.stdin.write(b"y")
                process.stdin.flush()
                if process.wait(timeout=5) != 0:
                    raise RuntimeError("probe process failed")
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
                process.stdin.close()
                if process.stdout is not None:
                    process.stdout.close()
        print("PASS: inherited-access control and private initialized locked publication")
    return 0


if __name__ == "__main__":
    sys.exit(run())
