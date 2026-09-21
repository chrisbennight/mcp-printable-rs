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
 && apt-get install -y --no-install-recommends cmake g++ git \
 && rm -rf /var/lib/apt/lists/*

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
RUN cargo build --release --locked --bin printable-server --bin printable-geometry-worker --bin printable-cad-worker
RUN strip target/release/printable-server target/release/printable-geometry-worker || true

# CI exports the MCP smoke driver from the same Trixie build environment as
# the runtime image. It is never copied into the production stage.
FROM build AS smoke-build
RUN cargo build --release --locked --bin printable-smoke
RUN strip target/release/printable-smoke || true

FROM scratch AS smoke-export
COPY --from=smoke-build /app/target/release/printable-smoke /printable-smoke

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
