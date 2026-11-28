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
# A job holding at least this share of its budget ran into the budget itself
# rather than into memory that other software was using.
AT_BUDGET = 0.9


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
    facts = []
    if free is not None and total is not None:
        facts.append(f"{_gib(free)} of {_gib(total)} was free")
    if held is not None:
        held_text = f"this job held {_gib(held)}"
        if budget is not None:
            held_text += f" of its {_gib(budget)} budget"
        facts.append(held_text)
    if facts:
        text += ": " + " and ".join(facts)
    if budget is not None and held is not None and held >= AT_BUDGET * budget:
        advice = "this stage reached the worker's GPU memory budget; "
        advice += "lower the resolution or ask the operator to raise the budget" if lower_resolution_helps \
            else "the operator can raise the budget"
    elif budget is None and held is not None and total is not None and held >= AT_BUDGET * total:
        advice = "this stage needed more memory than the GPU has"
        if lower_resolution_helps:
            advice += "; lower the resolution"
    else:
        advice = "the GPU did not have enough free memory for this stage, usually because other software is using it; " \
                 "retry when that work has finished"
        if lower_resolution_helps:
            advice += " or lower the resolution"
    return f"{text}. {advice[0].upper()}{advice[1:]}."


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
    """Turn PyTorch's GPU out-of-memory error inside the block into a GpuMemoryError."""
    import torch

    try:
        yield
    except torch.cuda.OutOfMemoryError as error:
        raise failure(stage, lower_resolution_helps=lower_resolution_helps, budget=budget) from error


def _gib(value: int) -> str:
    return f"{value / GIB:.1f} GiB"
