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

    cuda = types.SimpleNamespace(OutOfMemoryError=FakeOutOfMemory, mem_get_info=mem_get_info,
                                 memory_reserved=lambda: int(held))
    return types.SimpleNamespace(cuda=cuda)


class MessageTests(unittest.TestCase):
    def test_a_busy_gpu_is_reported_with_its_free_memory_and_no_resolution_advice(self):
        text = message("background removal", lower_resolution_helps=False,
                       free=int(1.1 * GIB), total=16 * GIB, held=int(0.6 * GIB), budget=4 * GIB)
        self.assertIn("during background removal", text)
        self.assertIn("1.1 GiB of 16.0 GiB was free", text)
        self.assertIn("this job held 0.6 GiB of its 4.0 GiB budget and other processes held about 14.3 GiB", text)
        self.assertIn("other software", text)
        self.assertNotIn("resolution", text)

    def test_a_busy_gpu_suggests_a_lower_resolution_only_where_it_helps(self):
        text = message("shape sampling", lower_resolution_helps=True,
                       free=GIB, total=16 * GIB, held=GIB, budget=4 * GIB)
        self.assertIn("other software", text)
        self.assertIn("or lower the resolution", text)

    def test_a_stage_at_its_budget_is_told_apart_from_a_busy_gpu(self):
        lower = message("shape decoding", lower_resolution_helps=True,
                        free=8 * GIB, total=16 * GIB, held=int(3.9 * GIB), budget=4 * GIB)
        self.assertIn("needed more than the worker's GPU memory budget allows", lower)
        self.assertIn("lower the resolution or ask the operator to raise the budget", lower)
        self.assertNotIn("other software", lower)
        fixed = message("background removal", lower_resolution_helps=False,
                        free=8 * GIB, total=16 * GIB, held=int(3.9 * GIB), budget=4 * GIB)
        self.assertIn("the operator can raise the budget", fixed)
        self.assertNotIn("resolution", fixed)

    def test_without_a_budget_the_job_is_never_said_to_reach_one(self):
        text = message("surface", lower_resolution_helps=False, free=8 * GIB, total=16 * GIB, held=GIB)
        self.assertIn("this job held 1.0 GiB and other processes held about 7.0 GiB.", text)
        self.assertNotIn("budget", text)
        self.assertIn("other software", text)

    def test_a_job_that_filled_the_gpu_itself_is_not_blamed_on_other_software(self):
        text = message("surface", lower_resolution_helps=True, free=0, total=16 * GIB, held=int(15.5 * GIB))
        self.assertIn("needed more memory than the GPU has; lower the resolution.", text)
        self.assertNotIn("other software", text)
        self.assertNotIn("budget", text)

    def test_free_memory_decides_whether_the_budget_was_the_limit(self):
        # Little held but plenty free: one large allocation crossed the budget.
        text = message("shape sampling", lower_resolution_helps=True,
                       free=12 * GIB, total=16 * GIB, held=GIB, budget=4 * GIB)
        self.assertIn("needed more than the worker's GPU memory budget allows", text)
        self.assertNotIn("other software", text)

    def test_raising_a_budget_at_or_above_the_device_size_is_never_suggested(self):
        text = message("shape decoding", lower_resolution_helps=True,
                       free=int(14.5 * GIB), total=16 * GIB, held=GIB, budget=24 * GIB)
        self.assertIn("needed more memory than the GPU has; lower the resolution.", text)
        self.assertNotIn("raise", text)

    def test_without_free_memory_a_job_near_its_budget_is_taken_to_have_reached_it(self):
        text = message("shape sampling", lower_resolution_helps=False,
                       free=None, total=None, held=int(3.9 * GIB), budget=4 * GIB)
        self.assertIn("the operator can raise the budget", text)

    def test_unknown_figures_are_left_out(self):
        text = message("structure sampling", lower_resolution_helps=False, free=None, total=None, held=None)
        self.assertTrue(text.startswith("the GPU ran out of memory during structure sampling. The GPU"))
        self.assertNotIn("GiB", text)


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
