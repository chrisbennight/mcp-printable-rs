"""GPU out-of-memory failures name the stage, the memory figures and useful advice."""

import sys
import types
import unittest
from unittest.mock import patch

from printable_recon import gpu_memory
from printable_recon.gpu_memory import GIB, GpuMemoryError, message


class FakeOutOfMemory(RuntimeError):
    pass


def fake_torch(free=1.1 * GIB, total=16 * GIB, held=0.6 * GIB, query_fails=False):
    def mem_get_info():
        if query_fails:
            raise RuntimeError("CUDA error: out of memory")
        return int(free), int(total)

    cuda = types.SimpleNamespace(OutOfMemoryError=FakeOutOfMemory, mem_get_info=lambda device=None: mem_get_info(),
                                 memory_reserved=lambda device=None: int(held))
    return types.SimpleNamespace(cuda=cuda)


class MessageTests(unittest.TestCase):
    def test_the_figures_are_reported_with_other_processes_estimated(self):
        text = message("background removal", lower_resolution_helps=False,
                       free=int(1.1 * GIB), total=16 * GIB, held=int(0.6 * GIB), budget=4 * GIB)
        self.assertEqual(text, "the GPU ran out of memory during background removal: 1.1 GiB of 16.0 GiB was free, "
                               "this job held 0.6 GiB of its 4.0 GiB budget and other processes held about 14.3 GiB. "
                               "To continue, retry when other GPU work has finished.")

    def test_every_open_remedy_is_offered_when_the_cause_is_ambiguous(self):
        # Live allocations near the budget while other software holds most of the GPU.
        text = message("shape sampling", lower_resolution_helps=True,
                       free=GIB // 2, total=16 * GIB, held=3 * GIB, budget=4 * GIB)
        self.assertTrue(text.endswith("To continue, retry when other GPU work has finished or lower the resolution."))

    def test_raising_the_budget_is_offered_only_when_more_was_free_than_it_allowed(self):
        text = message("shape sampling", lower_resolution_helps=True,
                       free=12 * GIB, total=16 * GIB, held=int(3.5 * GIB), budget=4 * GIB)
        self.assertTrue(text.endswith("To continue, retry when other GPU work has finished, lower the resolution "
                                      "or ask the operator to raise the worker's GPU memory budget."))

    def test_resolution_is_offered_only_where_it_helps(self):
        text = message("background removal", lower_resolution_helps=False,
                       free=GIB, total=16 * GIB, held=GIB, budget=4 * GIB)
        self.assertNotIn("resolution", text)

    def test_a_budget_at_or_above_the_device_size_is_never_offered_for_raising(self):
        text = message("shape decoding", lower_resolution_helps=True,
                       free=int(14.5 * GIB), total=16 * GIB, held=GIB, budget=24 * GIB)
        self.assertEqual(text.split(". ", 1)[1],
                         "To continue, retry when other GPU work has finished or lower the resolution.")

    def test_without_a_budget_none_is_mentioned(self):
        text = message("surface", lower_resolution_helps=False, free=8 * GIB, total=16 * GIB, held=GIB)
        self.assertIn("this job held 1.0 GiB and other processes held about 7.0 GiB.", text)
        self.assertNotIn("budget", text)

    def test_without_free_memory_raising_the_budget_is_never_offered(self):
        text = message("surface", lower_resolution_helps=False, free=None, total=None, held=int(3.9 * GIB),
                       budget=4 * GIB)
        self.assertNotIn("raise", text)

    def test_unknown_figures_are_left_out_and_retrying_stays_open(self):
        text = message("structure sampling", lower_resolution_helps=False, free=None, total=None, held=None,
                       budget=4 * GIB)
        self.assertEqual(text, "the GPU ran out of memory during structure sampling. "
                               "To continue, retry when other GPU work has finished.")


class WatchTests(unittest.TestCase):
    def test_an_out_of_memory_error_becomes_a_described_failure(self):
        with patch.dict(sys.modules, {"torch": fake_torch()}):
            with self.assertRaises(GpuMemoryError) as caught:
                with gpu_memory.watch("background removal", lower_resolution_helps=False, budget=4 * GIB):
                    raise FakeOutOfMemory("CUDA out of memory. Tried to allocate 2.00 GiB")
        error = caught.exception
        self.assertEqual((error.stage, error.free, error.total, error.held, error.budget),
                         ("background removal", int(1.1 * GIB), 16 * GIB, int(0.6 * GIB), 4 * GIB))
        self.assertIsInstance(error.__cause__, FakeOutOfMemory)
        self.assertIn("1.1 GiB of 16.0 GiB was free", str(error))

    def test_a_device_that_cannot_be_queried_still_reports_the_stage(self):
        with patch.dict(sys.modules, {"torch": fake_torch(query_fails=True)}):
            with self.assertRaises(GpuMemoryError) as caught:
                with gpu_memory.watch("model loading", lower_resolution_helps=False):
                    raise FakeOutOfMemory("CUDA out of memory")
        self.assertIsNone(caught.exception.free)
        self.assertIn("during model loading: this job held 0.6 GiB", str(caught.exception))

    def test_a_cuda_library_out_of_memory_error_is_described_too(self):
        with patch.dict(sys.modules, {"torch": fake_torch()}):
            with self.assertRaises(GpuMemoryError) as caught:
                with gpu_memory.watch("shape sampling", lower_resolution_helps=True):
                    raise RuntimeError("CUDA error: out of memory")
        self.assertEqual(caught.exception.stage, "shape sampling")

    def test_a_cuda_driver_status_out_of_memory_error_is_described_too(self):
        with patch.dict(sys.modules, {"torch": fake_torch()}):
            with self.assertRaises(GpuMemoryError):
                with gpu_memory.watch("shape sampling", lower_resolution_helps=True):
                    raise RuntimeError("CUDA driver error: CUDA_ERROR_OUT_OF_MEMORY")

    def test_other_errors_pass_through_unchanged(self):
        with patch.dict(sys.modules, {"torch": fake_torch()}):
            with self.assertRaises(ValueError):
                with gpu_memory.watch("shape sampling", lower_resolution_helps=True):
                    raise ValueError("unrelated")
            with self.assertRaises(RuntimeError):
                with gpu_memory.watch("shape sampling", lower_resolution_helps=True):
                    raise RuntimeError("an unrelated CUDA failure")


if __name__ == "__main__":
    unittest.main()
