# Depth benchmark environment. Build from the repository root:
#   docker build -f scripts/depth_benchmark/Dockerfile -t printable-depth-benchmark .
# Three environments share the base image's PyTorch, which matches the
# photograph worker's PyTorch and CUDA versions:
#   /opt/main      MoGe and Depth Pro 2, plus scoring and reporting
#   /opt/da3       Depth Anything 3
#   /opt/marigold  Printable's Marigold V2 worker code with its pinned packages
# The base image and model code are pinned; model weights are downloaded at run
# time into a mounted cache. Python packages resolve at build time and are
# recorded in /src/freeze-<environment>.txt.
FROM pytorch/pytorch:2.14.1-cuda13.0-cudnn9-devel@sha256:4e76657a91117d4373f0a4ba514fb55cbac4c846539aae8f36ed6800254c9a46

ENV DEBIAN_FRONTEND=noninteractive PIP_NO_CACHE_DIR=1 PIP_DISABLE_PIP_VERSION_CHECK=1
RUN apt-get update && apt-get install -y --no-install-recommends \
      git curl xz-utils ffmpeg libgl1 libglib2.0-0 libxrender1 libxi6 libxkbcommon0 \
      libsm6 libxext6 libxxf86vm1 libxfixes3 libegl1 \
 && rm -rf /var/lib/apt/lists/*

# Blender renders the scenes with exact depth and normals. The archive is
# checked against a digest recorded here, not against a checksum file from the
# same server; change BLENDER_SHA256 whenever BLENDER_VERSION changes.
ARG BLENDER_SERIES=5.3
ARG BLENDER_VERSION=5.3.0
ARG BLENDER_SHA256=dacb8264197a3f7fda6001285a5a506fc97ee943881261c2c3bd63c882f22e05
RUN archive=blender-${BLENDER_VERSION}-linux-x64.tar.xz \
 && curl -fsSL -o /tmp/${archive} https://download.blender.org/release/Blender${BLENDER_SERIES}/${archive} \
 && echo "${BLENDER_SHA256}  /tmp/${archive}" | sha256sum --check --strict \
 && mkdir -p /opt/blender && tar -xJf /tmp/${archive} -C /opt/blender --strip-components=1 \
 && rm /tmp/${archive}

# Model code at the revisions the recorded evidence used.
ARG MOGE_REVISION=74fbce054ebed49800de42d0ad0e83495065719a
ARG DEPTH_PRO_2_REVISION=aa389cbb0b74341664ed663a0966c859905cd6c0
ARG DEPTH_ANYTHING_3_REVISION=3d835ec1a5802d64a8b8b15f817a1ab54809bfe4
WORKDIR /src
RUN set -e; fetch() { git init -q "$1" && git -C "$1" fetch -q --depth 1 "$2" "$3" && git -C "$1" checkout -q FETCH_HEAD; } \
 && fetch MoGe https://github.com/microsoft/MoGe "$MOGE_REVISION" \
 && fetch ml-depth-pro-2 https://github.com/apple-aiml-research/ml-depth-pro-2 "$DEPTH_PRO_2_REVISION" \
 && fetch Depth-Anything-3 https://github.com/ByteDance-Seed/Depth-Anything-3 "$DEPTH_ANYTHING_3_REVISION" \
 && for d in MoGe ml-depth-pro-2 Depth-Anything-3; do echo "$d $(git -C $d rev-parse HEAD)"; done > /src/revisions.txt

# The base image's Python cannot bootstrap pip into a new environment, so each
# environment is created without pip and installed into with the base pip.
RUN python -m venv --without-pip --system-site-packages /opt/main \
 && python -m pip --python /opt/main/bin/python install -e /src/MoGe -e /src/ml-depth-pro-2 \
      opencv-python-headless scipy matplotlib OpenEXR pillow \
 && /opt/main/bin/python -c "import moge, depth_pro_2, OpenEXR, scipy"

# Depth Anything 3's declared dependencies, without xformers (optional, and it
# would replace the base PyTorch) and pre-commit (development only).
RUN python -m venv --without-pip --system-site-packages /opt/da3 \
 && python -m pip --python /opt/da3/bin/python install einops omegaconf huggingface_hub safetensors \
      opencv-python-headless imageio pillow trimesh typer plyfile pillow_heif evo e3nn \
      moviepy==1.0.3 pycolmap open3d requests addict fastapi uvicorn \
 && python -m pip --python /opt/da3/bin/python install --no-deps -e /src/Depth-Anything-3 \
 && /opt/da3/bin/python -c "from depth_anything_3.api import DepthAnything3"

COPY recon/requirements.txt /src/printable-recon/requirements.txt
COPY recon/convert_marigold.py /src/printable-recon/convert_marigold.py
COPY recon/printable_recon/__init__.py recon/printable_recon/files.py recon/printable_recon/marigold.py \
     recon/printable_recon/nf4.py /src/printable-recon/printable_recon/
RUN python -m venv --without-pip --system-site-packages /opt/marigold \
 && python -m pip --python /opt/marigold/bin/python install --no-deps -r /src/printable-recon/requirements.txt \
 && /opt/marigold/bin/python -c "import sys; sys.path.insert(0, '/src/printable-recon'); from printable_recon import marigold"

RUN for env in main da3 marigold; do python -m pip --python /opt/$env/bin/python freeze > /src/freeze-$env.txt; done
COPY scripts/depth_benchmark /bench/scripts
WORKDIR /bench/scripts
