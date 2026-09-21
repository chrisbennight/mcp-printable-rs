from __future__ import annotations

from types import SimpleNamespace
import unittest
from unittest.mock import Mock

from printable_bridge.handlers import BlenderHandlers, HandlerError
from printable_bridge.state import SceneState, SceneStateError


class SceneStateTests(unittest.TestCase):
    def test_stale_precondition_cannot_advance_or_enter_handler(self) -> None:
        handlers = self.handlers()
        expected = handlers.scene_state
        handlers.dispatch("rename_object", {"expected_scene": expected})
        observed = handlers.scene_state
        with self.assertRaises(SceneStateError) as caught:
            handlers.dispatch("rename_object", {"expected_scene": expected})
        self.assertEqual(caught.exception.code, "stale_scene_state")
        self.assertEqual(handlers.scene_state, observed)
        self.assertEqual(handlers._registry["rename_object"].call_count, 1)

    def test_failed_mutation_invalidates_prior_expectation(self) -> None:
        handlers = self.handlers()
        expected = handlers.scene_state
        handlers._registry["rename_object"].side_effect = HandlerError("partial failure")
        with self.assertRaises(HandlerError):
            handlers.dispatch("rename_object", {"expected_scene": expected})
        self.assertGreater(handlers.scene_state["revision"], expected["revision"])
        self.assertFalse(handlers._dispatching)

    def test_reads_preserve_revision_and_clear_replaces_generation(self) -> None:
        state = SceneState()
        before = state.snapshot
        state.begin("get_scene_info", before)
        self.assertEqual(state.snapshot, before)
        state.begin("clear_scene", before)
        self.assertNotEqual(state.snapshot["generation"], before["generation"])

    def test_invalid_preconditions_fail_without_mutation(self) -> None:
        state = SceneState()
        before = state.snapshot
        for value in (False, [], {}, {**before, "revision": True},
                      {**before, "revision": -1}, {**before, "generation": "bad"},
                      {**before, "extra": 1}):
            with self.subTest(value=value), self.assertRaises(SceneStateError) as caught:
                state.begin("execute_code", value)
            self.assertEqual(caught.exception.code, "invalid_scene_state")
            self.assertEqual(state.snapshot, before)

    def test_file_load_and_outside_updates_are_observed_and_callbacks_removed(self) -> None:
        handlers = self.handlers()
        callbacks = SimpleNamespace(persistent=lambda callback: callback,
                                    load_post=[], save_pre=[], depsgraph_update_post=[])
        handlers._bpy = SimpleNamespace(app=SimpleNamespace(handlers=callbacks),
                                        context=SimpleNamespace(scene={}))
        handlers.install_state_observers()
        before = handlers.scene_state
        callbacks.depsgraph_update_post[0](None, SimpleNamespace(updates=[object()]))
        self.assertGreater(handlers.scene_state["revision"], before["revision"])
        observed = handlers.scene_state
        handlers._dispatching = True
        callbacks.depsgraph_update_post[0](None, SimpleNamespace(updates=[object()]))
        self.assertEqual(handlers.scene_state, observed)
        callbacks.load_post[0](None)
        self.assertNotEqual(handlers.scene_state["generation"], before["generation"])
        handlers.close()
        self.assertEqual(callbacks.load_post, [])
        self.assertEqual(callbacks.depsgraph_update_post, [])

    def test_pending_outside_changes_are_evaluated_before_precondition_check(self) -> None:
        handlers = self.handlers()
        callbacks = SimpleNamespace(persistent=lambda callback: callback,
                                    load_post=[], save_pre=[], depsgraph_update_post=[])
        def update() -> None:
            callbacks.depsgraph_update_post[0](None, SimpleNamespace(updates=[object()]))
        handlers._bpy = SimpleNamespace(
            app=SimpleNamespace(handlers=callbacks),
            context=SimpleNamespace(view_layer=SimpleNamespace(update=update)),
        )
        handlers.install_state_observers()
        expected = handlers.scene_state
        with self.assertRaises(SceneStateError):
            handlers.dispatch("rename_object", {"expected_scene": expected})
        handlers._registry["rename_object"].assert_not_called()

    def test_project_binding_rejects_stale_and_cross_project_mutation(self) -> None:
        state = SceneState()
        generic = state.snapshot
        state.bind_project("enclosure")
        observed = state.snapshot
        self.assertEqual(observed["project_id"], "enclosure")
        for stale in (generic, {**observed, "project_id":"fixture"},
                      {key:value for key,value in observed.items() if key != "project_id"}):
            with self.assertRaises(SceneStateError):
                state.begin("attach_cad", stale)
            self.assertEqual(state.snapshot, observed)
        state.begin("attach_cad", observed)
        self.assertEqual(state.snapshot["project_id"], "enclosure")

    @staticmethod
    def handlers() -> BlenderHandlers:
        handlers = object.__new__(BlenderHandlers)
        handlers._scene_state = SceneState()
        handlers._dispatching = False
        handlers._state_callbacks = []
        handlers._registry = {"rename_object": Mock(return_value={})}
        handlers._workspace = Mock()
        return handlers


if __name__ == "__main__":
    unittest.main()
