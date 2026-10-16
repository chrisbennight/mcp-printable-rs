"""Convert Marigold V2's published files into the layout the worker streams.

Usage: convert_marigold.py <qwen-image-edit-2509> <marigold-v2> <output>
       convert_marigold.py --version

<qwen-image-edit-2509> is a download of Qwen/Qwen-Image-Edit-2509 with at
least its transformer and vae folders. <marigold-v2> is a download of
huawei-bayerlab/marigold-v2-0 with at least depth/Log-stage2, normals and
qwen_text_embeddings. <output> must not exist; it is written under a
temporary name beside itself and renamed once complete, so a failed
conversion leaves nothing behind.

Every linear weight of the base transformer is rounded to NF4 exactly as
Marigold V2 held it in training, and each transformer block is written to its
own file so the worker can read one block at a time. Each task keeps its
low-rank adapter, its fine-tuned VAE decoder and its precomputed prompt. The
prompts are PyTorch pickles; they are opened with the weights-only loader,
which builds tensors only and never runs code from the file. One JSON object
describing the output is printed.
"""

import json
import shutil
import sys
from contextlib import ExitStack
from pathlib import Path

VERSION = "printable-convert-marigold 1"

# Each task's checkpoint and prompt files within the Marigold V2 download.
TASKS = {
    "depth": ("depth/Log-stage2/trainables.safetensors", "qwen_edit_2509_qwen_depth_realimg512"),
    "normals": ("normals/trainables.safetensors", "qwen_edit_2509_qwen_normals_dummy512"),
}
# Marigold V2 applies every released adapter with this alpha.
LORA_ALPHA = 128.0


class ConversionError(Exception):
    """A source file is missing or not what the conversion expects."""


def main(argv: list[str]) -> int:
    if argv == ["--version"]:
        print(VERSION)
        return 0
    if len(argv) != 3:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    try:
        summary = convert(*(Path(argument) for argument in argv))
    except ConversionError as error:
        print(f"conversion failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(summary))
    return 0


def convert(qwen: Path, marigold: Path, output: Path) -> dict:
    """Write the converted model to `output` and describe it."""
    from printable_recon import files

    if output.exists():
        raise ConversionError(f"{output} already exists")
    staging = output.with_name(f"{output.name}.partial")
    if staging.exists():
        raise ConversionError(f"{staging} is left from an earlier conversion; remove it first")
    try:
        staging.mkdir(parents=True)
    except OSError as error:
        raise ConversionError(f"{staging} could not be created: {error}") from error
    try:
        blocks = _transformer(qwen / "transformer", staging / "transformer")
        (staging / "vae").mkdir()
        for name in ("config.json", "diffusion_pytorch_model.safetensors"):
            source = qwen / "vae" / name
            if not source.is_file():
                raise ConversionError(f"{source} is missing")
            shutil.copyfile(source, staging / "vae" / name)
        for task, (trainables, prompt) in TASKS.items():
            _task(marigold / trainables, marigold / "qwen_text_embeddings", prompt, staging / files.task_file(task))
        manifest = {"format": files.MARIGOLD_FORMAT, "blocks": blocks, "tasks": list(TASKS)}
        (staging / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        staging.rename(output)
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    return {"output": str(output), **manifest}


def _transformer(source: Path, target: Path) -> int:
    """Write the base transformer one block per file; return the block count."""
    import torch
    from safetensors import safe_open
    from safetensors.torch import save_file

    from printable_recon import files, marigold, nf4

    try:
        config = json.loads((source / "config.json").read_text(encoding="utf-8"))
        layers = int(config["num_layers"])
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise ConversionError(f"{source / 'config.json'} could not be used: {error}") from error
    index = source / "diffusion_pytorch_model.safetensors.index.json"
    if index.is_file():
        try:
            weight_map = json.loads(index.read_text(encoding="utf-8"))["weight_map"]
        except (OSError, ValueError, KeyError) as error:
            raise ConversionError(f"{index} could not be used: {error}") from error
    else:
        single = source / "diffusion_pytorch_model.safetensors"
        if not single.is_file():
            raise ConversionError(f"{source} holds no transformer weights")
        with safe_open(str(single), framework="pt", device="cpu") as stored:
            weight_map = {name: single.name for name in stored.keys()}
    groups: dict[int, list[str]] = {}
    for name in weight_map:
        parts = name.split(".")
        group = int(parts[1]) if parts[0] == "transformer_blocks" else -1
        if group >= layers:
            raise ConversionError(f"{name} belongs to a block beyond the {layers} the configuration names")
        groups.setdefault(group, []).append(name)
    if sorted(groups) != list(range(-1, layers)):
        raise ConversionError(f"the weights do not cover the {layers} blocks the configuration names")
    target.mkdir()
    shutil.copyfile(source / "config.json", target / "config.json")
    with ExitStack() as stack:
        shards = {}
        for shard in sorted(set(weight_map.values())):
            path = source / shard
            if not path.is_file():
                raise ConversionError(f"{path} is missing")
            shards[shard] = stack.enter_context(safe_open(str(path), framework="pt", device="cpu"))
        for group in range(-1, layers):
            prefix = "" if group < 0 else f"transformer_blocks.{group}."
            stored = {}
            for name in sorted(groups[group]):
                tensor = shards[weight_map[name]].get_tensor(name)
                local = name[len(prefix):]
                # Every linear weight of the published model fills whole NF4
                # blocks; one that did not would keep its full precision.
                if marigold.quantized(name, tuple(tensor.shape)) and nf4.fits(tensor):
                    packed, scales = nf4.quantize(tensor)
                    stored[f"{local}.nf4"] = packed
                    stored[f"{local}.scale"] = scales
                    stored[f"{local}.shape"] = torch.tensor(tensor.shape, dtype=torch.int64)
                else:
                    stored[local] = tensor.contiguous()
            output = "base.safetensors" if group < 0 else Path(files.block_file(group)).name
            save_file(stored, str(target / output))
    return layers


def _task(trainables: Path, embeddings: Path, prompt: str, target: Path) -> None:
    """Write one task's adapter, VAE decoder and prompt to `target`."""
    import torch
    from safetensors import safe_open
    from safetensors.torch import save_file

    if not trainables.is_file():
        raise ConversionError(f"{trainables} is missing")
    stored = {}
    with safe_open(str(trainables), framework="pt", device="cpu") as source:
        names = set(source.keys())
        for name in sorted(names):
            if name.startswith("Diffuser.") and name.endswith(".lora_A.default.weight"):
                layer = name[len("Diffuser."):-len(".lora_A.default.weight")]
                up_name = f"Diffuser.{layer}.lora_B.default.weight"
                if up_name not in names:
                    raise ConversionError(f"{trainables.name} has no up projection for {layer}")
                down = source.get_tensor(name)
                up = source.get_tensor(up_name)
                stored[f"lora.{layer}.a"] = down.contiguous()
                stored[f"lora.{layer}.b"] = (up.float() * (LORA_ALPHA / down.shape[0])).to(up.dtype)
            elif name.startswith("VAE."):
                stored[f"vae.{name[len('VAE.'):]}"] = source.get_tensor(name).contiguous()
    if not any(name.startswith("lora.") for name in stored):
        raise ConversionError(f"{trainables} holds no adapter")
    tensors = {}
    for part in ("embeds", "mask"):
        path = embeddings / f"{prompt}_prompt_{part}.pt"
        if not path.is_file():
            raise ConversionError(f"{path} is missing")
        try:
            value = torch.load(path, map_location="cpu", weights_only=True)
        except Exception as error:  # the unpickler raises many unrelated types
            raise ConversionError(f"{path.name} could not be read: {error}") from error
        if not torch.is_tensor(value):
            raise ConversionError(f"{path.name} does not hold a tensor")
        tensors[part] = value
    embeds, mask = tensors["embeds"], tensors["mask"]
    if embeds.ndim != 3 or tuple(mask.shape) != tuple(embeds.shape[:2]):
        raise ConversionError(f"the {prompt} prompt and its mask do not match")
    stored["prompt.embeds"] = embeds.contiguous()
    stored["prompt.mask"] = (mask > 0).contiguous()
    save_file(stored, str(target))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
