"""Download the benchmark photographs listed in photos.json and verify their SHA-1 digests.

Usage: python3 fetch_photos.py <images-dir>

Files are written under a temporary name and renamed only after the digest
matches. Existing files with the right digest are kept. Wikimedia asks automated
clients to identify themselves, so requests carry a descriptive User-Agent.
"""

import hashlib
import json
import sys
import time
import urllib.request
from pathlib import Path

USER_AGENT = "PrintableDepthBenchmark/1 (https://github.com/chrisbennight/mcp-printable-rs)"
MANIFEST = Path(__file__).with_name("photos.json")


def sha1(path: Path) -> str:
    digest = hashlib.sha1()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def fetch(entry: dict, folder: Path) -> str:
    target = folder / entry["file"]
    if target.exists() and sha1(target) == entry["sha1"]:
        return "kept"
    partial = target.with_suffix(target.suffix + ".part")
    request = urllib.request.Request(entry["url"], headers={"User-Agent": USER_AGENT})
    for attempt in range(4):
        try:
            with urllib.request.urlopen(request, timeout=120) as response, partial.open("wb") as out:
                while block := response.read(1 << 20):
                    out.write(block)
            break
        except OSError:
            if attempt == 3:
                raise
            time.sleep(10 * (attempt + 1))
    if sha1(partial) != entry["sha1"]:
        partial.unlink()
        raise ValueError(f"{entry['file']}: digest does not match the manifest")
    partial.replace(target)
    return "downloaded"


def main() -> int:
    folder = Path(sys.argv[1])
    folder.mkdir(parents=True, exist_ok=True)
    for entry in json.loads(MANIFEST.read_text()):
        print(entry["file"], fetch(entry, folder), flush=True)
        time.sleep(2)
    return 0


if __name__ == "__main__":
    sys.exit(main())
