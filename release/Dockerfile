# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32

FROM scratch

ARG SOURCE_REVISION
ARG SERVER_IMAGE
ARG SERVER_DIGEST
ARG BLENDER_IMAGE
ARG BLENDER_DIGEST
ARG CAD_IMAGE
ARG CAD_DIGEST
ARG SLICER_IMAGE
ARG SLICER_DIGEST

LABEL org.opencontainers.image.title="Printable compatible image set" \
      org.opencontainers.image.revision="${SOURCE_REVISION}" \
      org.printable.server.image="${SERVER_IMAGE}" \
      org.printable.server.digest="${SERVER_DIGEST}" \
      org.printable.blender.image="${BLENDER_IMAGE}" \
      org.printable.blender.digest="${BLENDER_DIGEST}" \
      org.printable.cad.image="${CAD_IMAGE}" \
      org.printable.cad.digest="${CAD_DIGEST}" \
      org.printable.slicer.image="${SLICER_IMAGE}" \
      org.printable.slicer.digest="${SLICER_DIGEST}"
