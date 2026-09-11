# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32

ARG SOURCE_REVISION=unknown

FROM debian:trixie-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258 AS fetch

ARG BLENDER_VERSION=5.2.0
ARG BLENDER_SHA256=96f6c181a30f4950607839dc84d42a354b250d8a0231b098b59b7bc69c351c48
ARG URLLIB3_WHEEL_URL=https://files.pythonhosted.org/packages/7f/3e/5db95bcf282c52709639744ca2a8b149baccf648e39c8cc87553df9eae0c/urllib3-2.7.0-py3-none-any.whl
ARG URLLIB3_SHA256=9fb4c81ebbb1ce9531cce37674bbc6f1360472bc18ca9a553ede278ef7276897
ARG VIRTUALGL_URL=https://github.com/VirtualGL/virtualgl/releases/download/3.1.5/virtualgl_3.1.5_amd64.deb
ARG VIRTUALGL_SHA256=df3f7788ce41b182a47c0d298e5cd6d2d63579522cb41825970b7726e825485e

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && curl --fail --location --proto '=https' --tlsv1.2 \
       --output /tmp/blender.tar.xz \
       "https://download.blender.org/release/Blender5.2/blender-${BLENDER_VERSION}-linux-x64.tar.xz" \
    && echo "${BLENDER_SHA256}  /tmp/blender.tar.xz" | sha256sum --check --strict \
    && mkdir -p /opt/blender \
    && tar --extract --xz --file /tmp/blender.tar.xz \
       --directory /opt/blender --strip-components=1 \
    && curl --fail --location --proto '=https' --tlsv1.2 \
       --output /tmp/urllib3-2.7.0-py3-none-any.whl "${URLLIB3_WHEEL_URL}" \
    && echo "${URLLIB3_SHA256}  /tmp/urllib3-2.7.0-py3-none-any.whl" | sha256sum --check --strict \
    && /opt/blender/5.2/python/bin/python3.13 -m pip install \
       --no-cache-dir --no-deps --force-reinstall /tmp/urllib3-2.7.0-py3-none-any.whl \
    && rm -rf \
       /opt/blender/5.2/python/bin \
       /opt/blender/5.2/python/lib/python3.13/ensurepip \
       /opt/blender/5.2/python/lib/python3.13/site-packages/pip* \
       /opt/blender/5.2/python/lib/python3.13/site-packages/setuptools*

RUN curl --fail --location --proto '=https' --tlsv1.2 \
       --output /tmp/virtualgl.deb "${VIRTUALGL_URL}" \
    && echo "${VIRTUALGL_SHA256}  /tmp/virtualgl.deb" | sha256sum --check --strict

FROM debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132

COPY --from=fetch /tmp/virtualgl.deb /tmp/virtualgl.deb

RUN apt-get update \
    && apt-get upgrade -y \
    && apt-get install -y --no-install-recommends \
       ca-certificates \
       libdbus-1-3 \
       libdecor-0-0 \
       libegl1 \
       libfontconfig1 \
       libfreetype6 \
       libgl1 \
       libice6 \
       libpulse0 \
       libsm6 \
       libtinfo6 \
       libwayland-client0 \
       libwayland-egl1 \
       libx11-6 \
       libxext6 \
       libxfixes3 \
       libxi6 \
       libxinerama1 \
       libxkbcommon0 \
       libxrandr2 \
       libxrender1 \
       libxxf86vm1 \
       python3 \
       tini \
       xauth \
       xvfb \
       /tmp/virtualgl.deb \
    && rm /tmp/virtualgl.deb \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 printable \
    && useradd --uid 10001 --gid 10001 --create-home --shell /usr/sbin/nologin printable \
    && mkdir -p /opt/printable /run/printable-blender /workspace \
    && chown 10001:10001 /run/printable-blender /workspace

COPY --from=fetch /opt/blender /opt/blender
COPY LICENSE /usr/share/doc/printable/LICENSE
COPY --chown=10001:10001 addon /opt/printable/addon
COPY --chown=10001:10001 scripts/run-headless-blender /opt/printable/scripts/run-headless-blender

ARG SOURCE_REVISION
LABEL org.opencontainers.image.source="https://gitea.cacahuate.org/bennight/mcp-printable-rs" \
      org.opencontainers.image.revision="${SOURCE_REVISION}" \
      org.printable.role="blender"

ENV BLENDER_BIN=/opt/blender/blender \
    HOME=/home/printable \
    PYTHONDONTWRITEBYTECODE=1 \
    PRINTABLE_BLENDER_BIND=0.0.0.0 \
    PRINTABLE_BLENDER_STATE_DIR=/run/printable-blender \
    PRINTABLE_BLENDER_WORKSPACE_ROOT=/workspace

WORKDIR /opt/printable
USER 10001:10001

HEALTHCHECK --interval=2s --timeout=2s --start-period=30s --retries=5 \
    CMD ["python3", "/opt/printable/addon/printable_bridge/healthcheck.py"]

ENTRYPOINT ["/usr/bin/tini", "--", "/opt/printable/scripts/run-headless-blender"]
