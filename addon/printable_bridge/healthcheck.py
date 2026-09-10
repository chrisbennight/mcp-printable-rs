"""Container health probe for the bridge listener and heartbeat."""

from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import time


def main() -> int:
    state_dir = Path(
        os.environ.get("PRINTABLE_BLENDER_STATE_DIR", "/run/printable-blender")
    )
    try:
        ready = json.loads((state_dir / "ready.json").read_text(encoding="utf-8"))
        live = json.loads((state_dir / "live.json").read_text(encoding="utf-8"))
        heartbeat = float(os.environ.get("PRINTABLE_BLENDER_HEARTBEAT_SECONDS", "1.0"))
        ready_pid = ready["pid"]
        live_pid = live["pid"]
        server_alive = live["server_alive"]
        main_thread_alive = live["main_thread_alive"]
        observed = float(live["monotonic_seconds"])
    except (FileNotFoundError, ValueError, TypeError, json.JSONDecodeError, KeyError):
        return 1
    if (
        ready_pid != live_pid
        or server_alive is not True
        or main_thread_alive is not True
    ):
        return 1
    age = time.monotonic() - observed
    return 0 if 0 <= age <= max(5.0, heartbeat * 4) else 1


if __name__ == "__main__":
    sys.exit(main())
