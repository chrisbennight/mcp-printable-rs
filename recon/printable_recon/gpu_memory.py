"""Explain a GPU out-of-memory failure in terms a caller can act on.

A worker's GPU memory budget limits only its own process. Other software on the
same GPU can leave less memory free than the budget allows, and then a smaller
request does not help. The message names the stage that failed, how much GPU
memory was free, how much this job held and about how much other processes
held, and lists the remedies those figures leave open.

PyTorch is imported only when a failure is described, so the stage programs can
import this module before they start CUDA.
"""

from __future__ import annotations

from contextlib import contextmanager

GIB = 1024**3


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
    # The size of the failed request is unknown, so every remedy the figures
    # leave open is offered rather than a guess at a single cause.
    remedies = ["retry when other GPU work has finished"]
    if lower_resolution_helps:
        remedies.append("lower the resolution")
    # A larger budget helps only when more memory was free than the budget
    # still allowed, and the worker caps its memory fraction at the device.
    if None not in (budget, total, free, held) and budget < total and free >= budget - held:
        remedies.append("ask the operator to raise the worker's GPU memory budget")
    return f"{text}. To continue, " + ", ".join(remedies[:-1]) + (" or " if len(remedies) > 1 else "") + remedies[-1] + "."


def is_out_of_memory(error: BaseException) -> bool:
    """Whether a PyTorch or CUDA error inside a GPU stage reports exhausted GPU memory.

    Allocator failures raise PyTorch's OutOfMemoryError; CUDA libraries and
    compiled kernels report the same condition as a RuntimeError.
    """
    import torch

    text = str(error).lower()
    return isinstance(error, torch.cuda.OutOfMemoryError) or (
        isinstance(error, RuntimeError) and ("out of memory" in text or "cuda_error_out_of_memory" in text))


def failure(stage: str, *, lower_resolution_helps: bool, budget: int | None = None,
            device=None) -> GpuMemoryError:
    """Describe an out-of-memory failure that has just happened in this process on
    `device`, or on the current CUDA device when none is given."""
    import torch

    held = int(torch.cuda.memory_reserved(device))
    try:
        free, total = torch.cuda.mem_get_info(device)
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
