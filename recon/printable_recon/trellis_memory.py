"""Load TRELLIS parameters from safetensors when their stage or block runs.

Inactive parameters have empty storage. Releasing a stage never creates a CPU
copy of its weights. File-cache eviction applies only to its checkpoint.
"""

from __future__ import annotations

import os
from pathlib import Path
import types

import torch
from safetensors import safe_open


def _evict(path: Path) -> None:
    with path.open("rb", buffering=0) as source:
        os.posix_fadvise(source.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)


def stream_parameters(model: torch.nn.Module, path: Path, *, blocks: bool, evict: bool = True) -> dict:
    """Attach disk-backed transfers to one inference-only network.

    Constructors establish working precision before storage is released. The
    checkpoint must supply every parameter, with exactly its declared shape.
    Buffers remain small resident state. The upstream checkpoints omit derived
    position buffers; retain their constructor values when absent and copy any
    checkpoint buffers before the checkpoint mapping closes.
    """
    parameters = dict(model.named_parameters())
    buffers = dict(model.named_buffers())
    with safe_open(path, framework="pt") as reader:
        keys = set(reader.keys())
        if not set(parameters) <= keys or not keys <= set(parameters) | set(buffers):
            raise ValueError("the TRELLIS checkpoint does not match its network state")
        for name, parameter in parameters.items():
            if reader.get_slice(name).get_shape() != list(parameter.shape):
                raise ValueError("the TRELLIS checkpoint does not match its parameter shapes")
        for name, buffer in buffers.items():
            if name in keys:
                if reader.get_slice(name).get_shape() != list(buffer.shape):
                    raise ValueError("the TRELLIS checkpoint does not match its buffer shapes")
                buffer.copy_(reader.get_tensor(name))
    if blocks and not isinstance(getattr(model, "blocks", None), torch.nn.ModuleList):
        raise ValueError("block streaming requires a transformer block list")
    stats = {"block_loads": 0, "logical_weight_bytes": 0, "file_evictions": 0}
    active_device: torch.device | None = None

    def release(module: torch.nn.Module) -> None:
        for parameter in module.parameters():
            parameter.data = torch.empty(0, dtype=parameter.dtype)

    def move_buffers(module: torch.nn.Module, device: torch.device | str) -> None:
        for child in module.modules():
            for name, value in child._buffers.items():
                if value is not None:
                    child._buffers[name] = value.to(device)

    def load(module: torch.nn.Module, prefix: str, device: torch.device, *, recurse: bool = True) -> None:
        with safe_open(path, framework="pt", device=str(device)) as reader:
            for name, parameter in module.named_parameters(recurse=recurse):
                value = reader.get_tensor(prefix + name)
                stats["logical_weight_bytes"] += value.numel() * value.element_size()
                parameter.data = value.to(dtype=parameter.dtype)
        if evict:
            torch.cuda.synchronize(device)
            _evict(path)
            stats["file_evictions"] += 1

    def move(self, device, *args, **kwargs):
        nonlocal active_device
        if args or kwargs:
            raise ValueError("disk-backed TRELLIS transfers accept only a device")
        device = torch.device(device)
        if device.type == "cpu":
            release(self)
            move_buffers(self, "cpu")
            active_device = None
            return self
        if device.type != "cuda":
            raise ValueError("disk-backed TRELLIS execution requires CUDA")
        if active_device is not None:
            if active_device == device:
                return self
            release(self)
        if blocks:
            for name, child in self.named_children():
                if name != "blocks":
                    load(child, name + ".", device)
                    move_buffers(child, device)
            load(self, "", device, recurse=False)
            for name, value in self.named_buffers(recurse=False):
                self._buffers[name] = value.to(device)
        else:
            load(self, "", device)
            move_buffers(self, device)
        active_device = device
        return self

    release(model)
    model.to = types.MethodType(move, model)
    model.cpu = types.MethodType(lambda self: self.to("cpu"), model)
    model.cuda = types.MethodType(
        lambda self, device=None: self.to(torch.device("cuda", device) if isinstance(device, int) else device or "cuda"),
        model,
    )
    if blocks:
        for index, block in enumerate(model.blocks):
            prefix = f"blocks.{index}."

            def before(module, inputs, prefix=prefix):
                if active_device is None:
                    raise ValueError("the TRELLIS network must select its CUDA device before execution")
                load(module, prefix, active_device)
                move_buffers(module, active_device)
                stats["block_loads"] += 1

            def after(module, inputs, output):
                release(module)
                move_buffers(module, "cpu")

            block.register_forward_pre_hook(before)
            block.register_forward_hook(after, always_call=True)
    return stats
