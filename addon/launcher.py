"""Blender entry point for the persistent Printable bridge."""

from __future__ import annotations

import logging
import os
from pathlib import Path
import sys


ADDON_ROOT = Path(__file__).resolve().parent
if str(ADDON_ROOT) not in sys.path:
    sys.path.insert(0, str(ADDON_ROOT))

from printable_bridge.config import BridgeConfig, ConfigError  # noqa: E402
from printable_bridge.runtime import BridgeRuntime  # noqa: E402
from printable_bridge.watchdog import SupervisorWatchdog, WatchdogError  # noqa: E402


def main() -> int | None:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s %(message)s",
    )
    try:
        config = BridgeConfig.from_env()
    except ConfigError as error:
        logging.getLogger("printable_bridge.launcher").error(
            "invalid bridge configuration: %s", error
        )
        return 2
    try:
        watchdog = SupervisorWatchdog.from_env()
        runtime = BridgeRuntime(config, watchdog)
    except (RuntimeError, WatchdogError) as error:
        logging.getLogger("printable_bridge.launcher").error(
            "bridge initialization failed: %s", error
        )
        return 2
    import bpy

    if bpy.app.background:
        return runtime.run()
    bpy.context.preferences.view.show_splash = False
    try:
        runtime.start_ui(bpy.app.timers, _exit_ui)
    except Exception as error:
        logging.getLogger("printable_bridge.launcher").error(
            "UI bridge startup failed: %s", type(error).__name__
        )
        return 1
    return None


def _exit_ui(exit_code: int) -> None:
    # Timer exceptions do not terminate Blender; the supervisor needs the exit status.
    logging.shutdown()
    os._exit(exit_code)


if __name__ == "__main__":
    exit_code = main()
    if exit_code is not None:
        raise SystemExit(exit_code)
