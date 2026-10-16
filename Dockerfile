# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32

ARG SOURCE_REVISION=unknown

# Builder: compile printable-server against the pinned toolchain. The whole
# workspace is copied because cargo must parse every member to build one crate
# (`-p printable-server`); `.dockerignore` keeps the context lean. manifold3d
# compiles its C++ library from source on first build, so the builder needs
# cmake + a C++ toolchain + git (the same deps the CI installs).
FROM rust:1.97-trixie@sha256:b1b3c9c0d921d7fa0a6d1f9ec7e4eab87f8c8ec97644c3d791450f131dec813f AS build
WORKDIR /app
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake g++ git curl xz-utils \
 && rm -rf /var/lib/apt/lists/*
RUN curl --fail --location --proto '=https' --tlsv1.2 --retry 3 \
      --output /tmp/cargo-auditable.tar.xz \
      https://github.com/rust-secure-code/cargo-auditable/releases/download/v0.7.7/cargo-auditable-x86_64-unknown-linux-gnu.tar.xz \
 && echo '4c8d3b08c3c34e9f2333367224dbbfb4aab68067698b43de59fe19aded3416dd  /tmp/cargo-auditable.tar.xz' | sha256sum --check --strict \
 && tar -xJf /tmp/cargo-auditable.tar.xz --strip-components=1 -C /usr/local/bin \
      cargo-auditable-x86_64-unknown-linux-gnu/cargo-auditable \
 && rm /tmp/cargo-auditable.tar.xz

# Where crates come from. Cargo reads no environment variable for a mirror, so
# unlike pip or npm it cannot simply be told: the redirect has to be a config
# file, written below from this value. The name stays outside cargo's own
# CARGO_ namespace deliberately - cargo maps CARGO_REGISTRY_INDEX onto its
# removed registry.index key and aborts every invocation, and an ARG reaches
# RUN as an environment variable, so that spelling would break the build it was
# meant to route.
#
# Left unsupplied it stays unset, no file is written, and cargo resolves from
# crates.io. That fallback is what keeps this image buildable away from the
# network the proxy lives on.
ARG CRATES_INDEX_URL

COPY . .

# Source replacement rather than an additional registry: it redirects the
# existing crates.io source instead of introducing a second one, so Cargo.lock
# goes on naming crates-io and stays resolvable from the public index. Written
# after COPY so the build context cannot overwrite it, and appended so anything
# the context already carries survives.
RUN if [ -n "${CRATES_INDEX_URL}" ]; then \
      mkdir -p .cargo && \
      printf '\n[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "%s"\n' \
        "${CRATES_INDEX_URL}" >> .cargo/config.toml; \
    fi
RUN cargo auditable build --release --locked --bin printable-server --bin printable-geometry-worker --bin printable-cad-worker --bin printable-slicer-worker --bin printable-recon-worker
RUN strip target/release/printable-server target/release/printable-geometry-worker || true

# CI exports the MCP smoke driver from the same Trixie build environment as
# the runtime image. It is never copied into the production stage.
FROM build AS smoke-build
RUN cargo build --release --locked --bin printable-smoke
RUN strip target/release/printable-smoke || true

FROM scratch AS smoke-export
COPY --from=smoke-build /app/target/release/printable-smoke /printable-smoke

FROM debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132 AS slicer-runtime
ARG SOURCE_REVISION
ARG SOURCE_REPOSITORY=https://github.com/chrisbennight/mcp-printable-rs
RUN apt-get update && apt-get upgrade -y \
 && apt-get install -y --no-install-recommends curl ca-certificates tini libopengl0 libglu1-mesa libgtk-3-0 libgstreamer1.0-0 libgstreamer-plugins-base1.0-0 libwebkit2gtk-4.1-0 libsecret-1-0 libmspack0 libsm6 xvfb xauth libgl1-mesa-dri \
 && rm -rf /var/lib/apt/lists/*
RUN curl --fail --location --output /tmp/orca.AppImage https://github.com/chrisbennight/OrcaSlicer/releases/download/local-2.4.2-6915edb/OrcaSlicer_Linux_AppImage_Ubuntu2404_local-2.4.2-6915edb.AppImage \
 && echo 'eb46a67b37063cc5ccbe00c058b45cf5cd71e4c0c15e66a96ca2c0c89ca1da13  /tmp/orca.AppImage' | sha256sum --check \
 && chmod 0755 /tmp/orca.AppImage && cd /opt && /tmp/orca.AppImage --appimage-extract >/dev/null \
 && mv squashfs-root orca && rm /tmp/orca.AppImage \
 && rm -rf /opt/orca/resources/web \
 && useradd --uid 10001 --create-home --shell /usr/sbin/nologin app
COPY --from=build /app/target/release/printable-slicer-worker /usr/local/bin/printable-slicer-worker
COPY release/notices/orca /opt/orca/licenses
COPY --chmod=0755 scripts/orca-headless /usr/local/bin/orca-headless
ENV PRINTABLE_SLICER_BIN=/usr/local/bin/orca-headless
COPY LICENSE /usr/share/doc/printable/LICENSE
LABEL org.opencontainers.image.source="${SOURCE_REPOSITORY}" \
      org.opencontainers.image.revision="${SOURCE_REVISION}" \
      org.printable.role="slicer-worker"
USER 10001
EXPOSE 8003
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["/usr/local/bin/printable-slicer-worker", "--healthcheck"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/printable-slicer-worker"]

FROM debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132 AS cad-runtime
ARG SOURCE_REVISION
ARG SOURCE_REPOSITORY=https://github.com/chrisbennight/mcp-printable-rs
RUN apt-get update && apt-get upgrade -y \
 && apt-get install -y --no-install-recommends python3-venv libgl1 libxrender1 libglib2.0-0 libgomp1 tini ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && python3 -m venv /opt/cad
COPY cad/requirements.txt /opt/printable/cad/requirements.txt
RUN /opt/cad/bin/pip install --no-cache-dir -r /opt/printable/cad/requirements.txt
COPY cad/build.py cad/step.py /opt/printable/cad/
COPY --from=build /app/target/release/printable-cad-worker /usr/local/bin/printable-cad-worker
COPY LICENSE /usr/share/doc/printable/LICENSE
RUN useradd --uid 10001 --create-home --shell /usr/sbin/nologin app
LABEL org.opencontainers.image.source="${SOURCE_REPOSITORY}" \
      org.opencontainers.image.revision="${SOURCE_REVISION}" \
      org.printable.role="cad-worker"
USER 10001
EXPOSE 8001
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["/usr/local/bin/printable-cad-worker", "--healthcheck"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/printable-cad-worker"]

# Photograph jobs: FLAME reconstruction with PyTorch, the network definitions
# and MediaPipe's face landmarker, and Marigold V2 depth estimation with the
# diffusers model definitions. Network checkpoints and the converted Marigold
# V2 model are operator-supplied on mounted volumes.
# The default PyTorch index serves CUDA 12.8 builds for NVIDIA hosts; CI uses
# the CPU index to test without a GPU.
# OpenCV for the reconstruction worker, built from the hash-pinned
# opencv-python source with core and image processing only. Published OpenCV
# wheels bundle FFmpeg and other media libraries the worker never uses.
FROM debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132 AS recon-opencv
ARG OPENCV_SOURCE_URL=https://files.pythonhosted.org/packages/1d/99/76b7c80252aa83c1af16393454aafd125a0287101afe8deb0a6821af0e30/opencv_python_headless-5.0.0.93.tar.gz
ARG OPENCV_SOURCE_SHA256=b82f9831daab90b725c7c1ee1b36cb5732c367096ac76d119e64e14eb70d5f3c
RUN apt-get update && apt-get upgrade -y \
 && apt-get install -y --no-install-recommends build-essential cmake ninja-build python3-dev python3-venv ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && python3 -m venv /opt/build
COPY recon/requirements.txt /tmp/recon-requirements.txt
RUN /opt/build/bin/pip install --no-cache-dir --no-deps "$(grep '^numpy==' /tmp/recon-requirements.txt)"
RUN curl --fail --location --output /tmp/opencv.tar.gz "${OPENCV_SOURCE_URL}" \
 && echo "${OPENCV_SOURCE_SHA256}  /tmp/opencv.tar.gz" | sha256sum --check \
 && mkdir /tmp/opencv \
 && tar -xzf /tmp/opencv.tar.gz -C /tmp/opencv --strip-components=1 \
 && cmake -S /tmp/opencv/opencv -B /tmp/opencv-build -G Ninja \
      -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/opt/opencv \
      -DBUILD_LIST=core,imgproc,python3 -DBUILD_SHARED_LIBS=OFF \
      -DBUILD_TESTS=OFF -DBUILD_PERF_TESTS=OFF -DBUILD_DOCS=OFF -DBUILD_EXAMPLES=OFF -DBUILD_opencv_apps=OFF \
      -DWITH_FFMPEG=OFF -DWITH_GSTREAMER=OFF -DWITH_V4L=OFF -DWITH_1394=OFF \
      -DWITH_GTK=OFF -DWITH_QT=OFF -DWITH_OPENGL=OFF -DWITH_VTK=OFF \
      -DWITH_IPP=OFF -DWITH_OPENCL=OFF -DWITH_ADE=OFF -DWITH_PROTOBUF=OFF -DWITH_EIGEN=OFF \
      -DWITH_LAPACK=OFF -DWITH_ITT=OFF -DWITH_OPENEXR=OFF -DWITH_JPEG=OFF -DWITH_PNG=OFF \
      -DWITH_TIFF=OFF -DWITH_WEBP=OFF -DWITH_OPENJPEG=OFF -DWITH_JASPER=OFF -DWITH_AVIF=OFF \
      -DPYTHON3_EXECUTABLE=/opt/build/bin/python -DPYTHON3_LIMITED_API=ON \
      -DOPENCV_PYTHON3_INSTALL_PATH=/opt/opencv-python \
 && cmake --build /tmp/opencv-build \
 && cmake --install /tmp/opencv-build \
 && install -m 0644 /tmp/opencv/opencv/LICENSE /opt/opencv-python/LICENSE-OpenCV

FROM debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132 AS recon-runtime
ARG SOURCE_REVISION
ARG SOURCE_REPOSITORY=https://github.com/chrisbennight/mcp-printable-rs
ARG TORCH_INDEX_URL=https://download.pytorch.org/whl/cu128
RUN apt-get update && apt-get upgrade -y \
 && apt-get install -y --no-install-recommends python3-venv libegl1 libgles2 libglib2.0-0 libgomp1 tini ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && python3 -m venv /opt/recon
COPY recon/requirements.txt /opt/printable/recon/requirements.txt
RUN /opt/recon/bin/pip install --no-cache-dir --index-url "${TORCH_INDEX_URL}" torch==2.14.1 torchvision==0.29.1 \
 && /opt/recon/bin/pip install --no-cache-dir --no-deps -r /opt/printable/recon/requirements.txt
COPY --from=recon-opencv /opt/opencv-python /opt/opencv-python
# The worker installs nothing at run time, so pip leaves the environment.
RUN site="$(/opt/recon/bin/python -c 'import sysconfig; print(sysconfig.get_paths()["purelib"])')" \
 && echo /opt/opencv-python > "${site}/printable-opencv.pth" \
 && /opt/recon/bin/python -c 'import cv2; assert not hasattr(cv2, "VideoCapture")' \
 && /opt/recon/bin/python -m pip uninstall --yes pip
RUN curl --fail --location --output /opt/printable/recon/face_landmarker.task \
      https://storage.googleapis.com/mediapipe-models/face_landmarker/face_landmarker/float16/1/face_landmarker.task \
 && echo '64184e229b263107bc2b804c6625db1341ff2bb731874b0bcc2fe6544e0bc9ff  /opt/printable/recon/face_landmarker.task' | sha256sum --check \
 && curl --fail --location --output /opt/printable/recon/selfie_multiclass_256x256.tflite \
      https://storage.googleapis.com/mediapipe-models/image_segmenter/selfie_multiclass_256x256/float32/1/selfie_multiclass_256x256.tflite \
 && echo 'c6748b1253a99067ef71f7e26ca71096cd449baefa8f101900ea23016507e0e0  /opt/printable/recon/selfie_multiclass_256x256.tflite' | sha256sum --check
COPY recon/reconstruct.py recon/estimate_depth.py recon/convert_marigold.py /opt/printable/recon/
COPY recon/printable_recon /opt/printable/recon/printable_recon
COPY --from=build /app/target/release/printable-recon-worker /usr/local/bin/printable-recon-worker
COPY LICENSE /usr/share/doc/printable/LICENSE
COPY recon/NOTICE /usr/share/doc/printable/NOTICE-recon
RUN useradd --uid 10001 --create-home --shell /usr/sbin/nologin app
LABEL org.opencontainers.image.source="${SOURCE_REPOSITORY}" \
      org.opencontainers.image.revision="${SOURCE_REVISION}" \
      org.printable.role="recon-worker"
USER 10001
EXPOSE 8004
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["/usr/local/bin/printable-recon-worker", "--healthcheck"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/printable-recon-worker"]

# Runtime: debian-slim (not distroless — OpenSCAD, Xvfb, and FFmpeg need apt).
# OpenSCAD renders under a virtual framebuffer via the openscad-headless
# wrapper; tini reaps the xvfb-run/Xvfb grandchildren OpenSCAD spawns.
# libstdc++ (for the manifold3d code linked into the binary) arrives
# transitively with openscad.
FROM debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132 AS runtime
RUN apt-get update \
 && apt-get upgrade -y \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      ffmpeg \
      fonts-dejavu-core \
      openscad \
      tini \
      xvfb \
      xauth \
 && rm -rf /var/lib/apt/lists/*

RUN useradd --uid 10001 --create-home --shell /usr/sbin/nologin app \
 && mkdir -p /tmp/printable \
 && chown 10001:10001 /tmp/printable

COPY --from=build /app/target/release/printable-server /usr/local/bin/printable-server
COPY --from=build /app/target/release/printable-geometry-worker /usr/local/bin/printable-geometry-worker
COPY scripts/openscad-headless /usr/local/bin/openscad-headless
COPY LICENSE /usr/share/doc/printable/LICENSE
COPY crates/printable-imaging/assets/LICENSE-Fira-OFL.txt /usr/share/doc/printable/LICENSE-Fira-OFL.txt
RUN chmod 0755 /usr/local/bin/openscad-headless

ARG SOURCE_REVISION
ARG SOURCE_REPOSITORY=https://github.com/chrisbennight/mcp-printable-rs
LABEL org.opencontainers.image.source="${SOURCE_REPOSITORY}" \
      org.opencontainers.image.revision="${SOURCE_REVISION}" \
      org.printable.role="server"

USER 10001

# Safe image defaults; the deployment sets the workspace root, Blender host, and
# the DNS-rebinding Host allowlist (see DEPLOYMENT.md). HOME is writable by uid
# 10001 so xvfb-run/OpenSCAD have a home.
ENV PRINTABLE_HTTP_HOST=0.0.0.0 \
    PRINTABLE_HTTP_PORT=8000 \
    PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB=1024 \
    FFMPEG_BIN=/usr/bin/ffmpeg \
    OPENSCAD_BIN=/usr/local/bin/openscad-headless \
    HOME=/tmp/printable
EXPOSE 8000

# Exec form: the binary's own --healthcheck hits loopback /healthz and exits 0/1
# (no shell or curl needed in the runtime path). Deployment gates use /readyz;
# container health remains process liveness so healthy queued work is not killed.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["/usr/local/bin/printable-server", "--healthcheck"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/printable-server", \
            "--transport", "streamable-http"]
