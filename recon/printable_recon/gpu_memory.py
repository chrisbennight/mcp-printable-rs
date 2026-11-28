"""Explain a GPU out-of-memory failure in terms a caller can act on.

A worker's GPU memory budget limits only its own process. Other software on the
same GPU can leave less memory free than the budget allows, and then a smaller
request does not help. The message names the stage that failed, how much GPU
memory was free, and how much this job held, so a caller can tell a busy GPU
from a request that is too large.

PyTorch is imported only when a failure is described, so the stage programs can
import this module before they start CUDA.
"""

from __future__ import annotations

from contextlib import contextmanager

GIB = 1024**3
# When free memory cannot be read, a job holding at least this share of its
# budget is taken to have run into the budget itself.
AT_BUDGET = 0.9
# Memory held outside this job's allocator below this amount is its own CUDA
# context and driver overhead rather than other software.
OTHER_SOFTWARE = GIB


class GpuMemoryError(Exception):
    """A stage could not obtain the GPU memory it needed."""

    def __init__(self, stage: str, *, lower_resolution_helps: bool, free: int | None,
                 total: int | None, held: int | None, budget: int | None = None):
        self.stage = stage
        self.lower_resolution_helps = lower_resolution_helps
        self.free, self.total, self.held, self.budget = free, total, held, budget
        super().__init__(message(stage, lower_resolution_helps=lower_resolution_helps,
                                 free=free, total=total, held=held, budget=budget))


def message(stage: str, *, lower_resolution_helps: bool, free: int | None, total: int | None,
            held: int | None, budget: int | None = None) -> str:
    """The caller-facing description of a stage that ran out of GPU memory."""
    text = f"the GPU ran out of memory during {stage}"
    others = total - free - held if None not in (free, total, held) else None
    facts = []
    if free is not None and total is not None:
        facts.append(f"{_gib(free)} of {_gib(total)} was free")
    if held is not None:
        held_text = f"this job held {_gib(held)}"
        if budget is not None:
            held_text += f" of its {_gib(budget)} budget"
        facts.append(held_text)
    if others is not None:
        facts.append(f"other processes held about {_gib(max(others, 0))}")
    if facts:
        text += ": " + ", ".join(facts[:-1]) + (" and " if len(facts) > 1 else "") + facts[-1]
    lower = " or lower the resolution" if lower_resolution_helps else ""
    # The worker caps its memory fraction at the whole device.
    limit = min(budget, total) if budget is not None and total is not None else budget
    if limit is not None and held is not None and (
            held >= AT_BUDGET * limit if free is None else free >= limit - held):
        # The budget, not free device memory, was the tighter limit.
        if total is not None and budget >= total:
            advice = "this stage needed more memory than the GPU has"
            advice += "; lower the resolution" if lower_resolution_helps else ""
        else:
            advice = "this stage needed more than the worker's GPU memory budget allows; " + (
                "lower the resolution or ask the operator to raise the budget" if lower_resolution_helps
                else "the operator can raise the budget")
    elif others is not None and others < OTHER_SOFTWARE:
        advice = "this stage needed more memory than the GPU has"
        advice += "; lower the resolution" if lower_resolution_helps else ""
    else:
        advice = "the GPU did not have enough free memory for this stage, usually because other software is " \
                 "using it; retry when that work has finished" + lower
    return f"{text}. {advice[0].upper()}{advice[1:]}."


def is_out_of_memory(error: BaseException) -> bool:
    """Whether a PyTorch or CUDA error inside a GPU stage reports exhausted GPU memory.

    Allocator failures raise PyTorch's OutOfMemoryError; CUDA libraries and
    compiled kernels report the same condition as a RuntimeError.
    """
    import torch

    return isinstance(error, torch.cuda.OutOfMemoryError) or (
        isinstance(error, RuntimeError) and "out of memory" in str(error).lower())


def failure(stage: str, *, lower_resolution_helps: bool, budget: int | None = None) -> GpuMemoryError:
    """Describe an out-of-memory failure that has just happened in this process."""
    import torch

    held = int(torch.cuda.memory_reserved())
    try:
        free, total = torch.cuda.mem_get_info()
    except RuntimeError:
        # Querying an exhausted device can fail too; report what is known.
        free = total = None
    return GpuMemoryError(stage, lower_resolution_helps=lower_resolution_helps,
                          free=None if free is None else int(free),
                          total=None if total is None else int(total), held=held, budget=budget)


@contextmanager
def watch(stage: str, *, lower_resolution_helps: bool, budget: int | None = None):
    """Turn a GPU out-of-memory error inside the block into a GpuMemoryError."""
    try:
        yield
    except RuntimeError as error:
        if not is_out_of_memory(error):
            raise
        raise failure(stage, lower_resolution_helps=lower_resolution_helps, budget=budget) from error


def _gib(value: int) -> str:
    return f"{value / GIB:.1f} GiB"
