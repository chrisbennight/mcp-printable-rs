"""Scene preconditions checked at the serialized Blender command boundary."""

from __future__ import annotations

from typing import Any
import threading
import uuid


MAX_REVISION = 9007199254740991
READ_COMMANDS = frozenset({
    "bridge_status", "get_scene_info", "get_object_info", "get_node_tree_info", "get_editing_state",
    "save_blend", "job_save_checkpoint", "export_stl", "render_product",
    "capture_native_view",
    "render_diagnostic", "render_views", "bridge_test_wait",
})
REPLACE_COMMANDS = frozenset({"clear_scene", "restore_checkpoint", "job_restore_checkpoint"})


class SceneStateError(ValueError):
    def __init__(self, message: str, code: str):
        super().__init__(message)
        self.code = code


class SceneState:
    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._generation = str(uuid.uuid4())
        self._revision = 0

    @property
    def snapshot(self) -> dict[str, Any]:
        with self._lock:
            return {"generation": self._generation, "revision": self._revision}

    def begin(self, command: str, expected: Any = None) -> None:
        with self._lock:
            if expected is not None:
                self._check(expected)
            if command in READ_COMMANDS:
                return
            if command in REPLACE_COMMANDS or self._revision == MAX_REVISION:
                self.replaced()
            else:
                self._revision += 1

    def replaced(self) -> None:
        with self._lock:
            self._generation = str(uuid.uuid4())
            self._revision = 0

    def _check(self, expected: Any) -> None:
        valid = isinstance(expected, dict) and set(expected) == {"generation", "revision"}
        if valid:
            generation = expected["generation"]
            revision = expected["revision"]
            valid = (
                isinstance(generation, str)
                and type(revision) is int
                and 0 <= revision <= MAX_REVISION
            )
        if valid:
            try:
                valid = str(uuid.UUID(generation)) == generation
            except (ValueError, AttributeError):
                valid = False
        if not valid:
            raise SceneStateError(
                "expected_scene requires a canonical generation UUID and non-negative revision",
                "invalid_scene_state",
            )
        if expected != self.snapshot:
            raise SceneStateError(
                "expected_scene is stale; inspect the current scene before retrying",
                "stale_scene_state",
            )
