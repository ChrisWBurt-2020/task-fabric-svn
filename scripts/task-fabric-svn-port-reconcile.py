#!/usr/bin/env python3
"""Task Fabric SVN Port Conflict Reconciler.

Safely inspects and clears port conflicts before starting workstation services.
If the listening process belongs to the expected service, it is left untouched.
If it is a stale, defunct, or conflicting process, it is terminated cleanly.
"""

import os
import sys
import time
import signal
import subprocess
import argparse
from pathlib import Path


def find_listener(port: int):
    """Find PID and command listening on the given TCP port."""
    try:
        # Use ss -tulpn to inspect listening sockets
        res = subprocess.run(
            ["ss", "-tulpn", f"sport = :{port}"],
            capture_output=True,
            text=True,
            check=False,
        )
        lines = res.stdout.strip().splitlines()
        for line in lines[1:]:  # skip header
            if f":{port}" in line:
                # Look for pid=... in users:(("...",pid=123,fd=...))
                import re
                m = re.search(r'pid=(\d+)', line)
                if m:
                    pid = int(m.group(1))
                    try:
                        cmdline = Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace").strip()
                    except Exception:
                        cmdline = ""
                    return pid, cmdline
    except Exception:
        pass

    # Fallback to fuser
    try:
        res = subprocess.run(
            ["fuser", f"{port}/tcp"],
            capture_output=True,
            text=True,
            check=False,
        )
        pids = [int(p) for p in res.stdout.strip().split() if p.isdigit()]
        if pids:
            pid = pids[0]
            try:
                cmdline = Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace").strip()
            except Exception:
                cmdline = ""
            return pid, cmdline
    except Exception:
        pass

    return None, None


def reconcile_port(port: int, expected_match: str = None, force: bool = False):
    pid, cmdline = find_listener(port)
    if not pid:
        print(f"Port {port} is free.")
        return 0

    print(f"Port {port} is in use by PID {pid}: {cmdline[:80]}")

    # Check if this process already matches what is expected
    if expected_match and expected_match.lower() in cmdline.lower() and not force:
        print(f"PID {pid} matches expected service '{expected_match}'. Leaving intact.")
        return 0

    # Otherwise, this is an orphan or conflicting process
    print(f"Reconciling conflict: Terminating stale listener PID {pid} on port {port}...")
    try:
        os.kill(pid, signal.SIGTERM)
        for _ in range(10):
            time.sleep(0.1)
            try:
                os.kill(pid, 0)
            except OSError:
                break
        else:
            print(f"PID {pid} did not terminate on SIGTERM; sending SIGKILL...")
            os.kill(pid, signal.SIGKILL)
            time.sleep(0.1)
        print(f"Port {port} successfully cleared.")
        return 0
    except ProcessLookupError:
        print(f"PID {pid} already exited.")
        return 0
    except PermissionError as e:
        print(f"Permission denied killing PID {pid}: {e}", file=sys.stderr)
        return 1


def main():
    parser = argparse.ArgumentParser(description="Reconcile TCP port conflicts for Task Fabric SVN services")
    parser.add_argument("port", type=int, help="TCP port number")
    parser.add_argument("--expected", type=str, default=None, help="Process name/substring expected to own the port")
    parser.add_argument("--force", action="store_true", help="Force termination even if matching expected service")
    args = parser.parse_args()

    sys.exit(reconcile_port(args.port, expected_match=args.expected, force=args.force))


if __name__ == "__main__":
    main()
