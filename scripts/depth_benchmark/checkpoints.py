"""Model checkpoints at pinned revisions, verified by SHA-256 before they are loaded.

Hugging Face files are fetched at a fixed commit; Depth Pro 2 files come from
Apple's published URLs into the torch hub cache, where the Depth Pro 2 code
looks for them. A file whose digest differs is an error, not a warning.
"""

from __future__ import annotations

import hashlib
from pathlib import Path

DEPTH_PRO_2_ROOT = "https://ml-site.cdn-apple.com/models/depth-pro-2"

# repository: (commit, {file: sha256})
HUGGING_FACE = {
    "Ruicheng/moge-3-vitg": ("6ef26c5a4b4148dab5ccaacdb08b72dc66380475", {
        "model.pt": "ce7c15417e9105c2ace7b4272e2cc69e36940921211eb7fa05d4d0bb03f0a00c"}),
    "Ruicheng/moge-3-vitl": ("184008f877d7ad1ad4c2cd2182a9bd1f63d0e5be", {
        "model.pt": "9b41b7b9f65ad80aab7ad686f5e9cc0d1fd33f1964022618dfbcd52fc1fb7925"}),
    "Ruicheng/moge-2-vitl-normal": ("cb0e8bbd6b1e243589717c78e750b1ba4c093acf", {
        "model.pt": "280741fd09bc3f403ccff9967784c2a391b52d2c0742ae3efdb21d9f90cc1a01"}),
    "Ruicheng/moge-2-vitb-normal": ("ca5f0e07ff01d3e5a364c1d954ed12ee1814b368", {
        "model.pt": "16b8110e86d5dc5a849db120ca96ef3a223fd30b0c9146d1d81db504073da5f6"}),
    "Ruicheng/moge-2-vits-normal": ("26b477f41595707c5db6770294c0d1721e8ed4ed", {
        "model.pt": "79a16621928c2bf0ed04659218c55c01075e950507f40bb3332fb4c873d3e1dc"}),
    "depth-anything/DA3NESTED-GIANT-LARGE-1.1": ("b2359bdf726fb44ef62acca04d629dcf158053e7", {
        "config.json": "09adf89474017e717bc05aa86fd3a378708ba8914b036d61874eced328069468",
        "model.safetensors": "8ebe871a022ed58d2fc8fdfb2ebdb31d57b60fe39611c849095851a7b7c6020c"}),
    "depth-anything/DA3MONO-LARGE": ("f465978e618db8cc79c83b8bbf24964857db1875", {
        "config.json": "649809acdfe99631293430bfb8bcd85620a1bba47bb4e27689bd204c645a4204",
        "model.safetensors": "7a799a7f95eb8d4c404c2ca8be3dc3276b350a417ddc4420db72ba850cc0e960"}),
    "depth-anything/DA3-BASE": ("f4a6c9b3c95e41c82048423d3493a81ec3fa810e", {
        "config.json": "5e34115ebc17bd2d8d43033c5f72e9446ac8833fd61d3fa160b7e67e0bb5b7b5",
        "model.safetensors": "e01067dc1659613083d9145a9a2547ccdbe6ccbbf83c4fe7b3e8a4e2bdae78b5"}),
    "depth-anything/DA3-SMALL": ("e08cab65ca0ec38e7826075418411ab90cab4da3", {
        "config.json": "a486e29e82b7ab4a7d4cefc1ea4526cfe2ae438a572c8ca98917cfbcde7447d2",
        "model.safetensors": "364492e38a3a06d221ac75da7f6621ada3f2361cd24fde11ba79091e9f40efcf"}),
}

# file name under DEPTH_PRO_2_ROOT: sha256
DEPTH_PRO_2 = {
    "depthpro2-fov-ew55zg4kq7.pt": "37fd480f30883879bfed412716fc26e6f5fdd1dbc22fe7a3e37fa659a0a817c4",
    "depthpro2-s-bgcc6f8bg3.pt": "23ce46c7b39678e2e9d6983fbcfa2366e0f691ffedc300f6cb3efa082f2fc590",
    "depthpro2-b-xb7m9egktw.pt": "172a7bf6c3e2078a31b56f47148f861e25961f5bb0247b999fd7bad9f1036aae",
    "depthpro2-l-tegstu7fim.pt": "24d717ae51d75a14e3c4c15699c5453129d4e8907cdc53dd756d0d5265f3e769",
    "depthpro2-h-eivtm5w7m5.pt": "ae39cfb7b08539f84db4a1e550761716adffb570d5c9ac3aa43a84a818340a2a",
}


class ChecksumError(RuntimeError):
    pass


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 24):
            digest.update(chunk)
    return digest.hexdigest()


def verify(path: Path, expected: str) -> Path:
    actual = sha256(path)
    if actual != expected:
        raise ChecksumError(f"{path.name}: SHA-256 {actual} does not match the pinned {expected}")
    return path


def hugging_face(repo: str) -> Path:
    """The verified snapshot directory of a pinned repository."""
    from huggingface_hub import hf_hub_download

    revision, files = HUGGING_FACE[repo]
    paths = [verify(Path(hf_hub_download(repo, name, revision=revision)), digest) for name, digest in files.items()]
    return paths[0].parent


def hugging_face_file(repo: str, name: str) -> Path:
    return hugging_face(repo) / name


def depth_pro_2(*names: str) -> None:
    """Place verified Depth Pro 2 checkpoints in the torch hub cache, downloading any that are missing."""
    import torch

    root = Path(torch.hub.get_dir()) / "checkpoints"
    root.mkdir(parents=True, exist_ok=True)
    for name in names:
        path = root / name
        if not path.exists():
            torch.hub.download_url_to_file(f"{DEPTH_PRO_2_ROOT}/{name}", str(path), progress=False)
        verify(path, DEPTH_PRO_2[name])


def digests() -> dict[str, str]:
    """Every pinned file and its SHA-256, for the evidence."""
    out = {f"{repo}@{rev}/{name}": d for repo, (rev, files) in HUGGING_FACE.items() for name, d in files.items()}
    out.update({f"{DEPTH_PRO_2_ROOT}/{name}": d for name, d in DEPTH_PRO_2.items()})
    return out
