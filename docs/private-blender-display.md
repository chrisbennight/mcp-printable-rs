# Private Blender display

The Blender image includes an authenticated, container-local Xvfb display and
VirtualGL's EGL backend. `PRINTABLE_BLENDER_MODE=ui` enables the private display;
the default remains `background` until production cutover. The supervisor
creates the display before starting Blender and keeps it through Blender
watchdog restarts.

`PRINTABLE_BLENDER_UI_BACKEND=software` uses Xvfb's software graphics for CPU
integration checks. `egl` redirects graphics to EGL device `egl0` through
VirtualGL. Production GPU validation requires NVIDIA as the actual graphics
vendor; a software result cannot satisfy that gate. The deployed GPU allocation
must expose compute, utility, graphics, and display driver capabilities.

The virtual display is 1600 by 1200 pixels. Xvfb listens only on local sockets
and uses an Xauthority cookie in its temporary directory. No desktop server,
host display socket, published X port, privileged mode, or host PID access is
required. The supervisor owns Xvfb separately from Blender and caller-created
descendants. Shutdown stops Blender before removing the display and its
ephemeral authority file.

The supervisor passes its owned display process identity to Blender at startup.
Python execution excludes that verified sibling from caller-process containment;
other unexpected siblings and caller-created descendants remain subject to
containment. This identity is internal supervisor metadata, not a user setting.

The native UI smoke uses a real Blender window, draws through GPUOffScreen's
native viewport path, and repeats the probe after a checkpoint restore. It
reads back the pixels and rejects black, empty, or uniform output, and records
the actual graphics vendor and renderer. The CPU container smoke also
runs the existing capability corpus in UI mode. The production GPU smoke adds
NVIDIA verification while retaining GPU coexistence and rendering checks.
These checks establish runtime capability; the agent-facing observation
contract and capture fidelity remain separate delivery outcomes.

## Dependency evidence

The [VirtualGL project](https://virtualgl.org/Documentation/Documentation)
documents its EGL backend as accessing a GPU without a 3D X server. We use that
backend so a private software X display can present GPU-rendered Blender
windows without adding a host display dependency.

[VirtualGL 3.1.5](https://github.com/VirtualGL/virtualgl/releases/tag/3.1.5) is
an actively supported stable release. The official amd64 Debian package is
checksum-pinned against the release asset digest and was independently
downloaded and checked before inclusion. Its declared dependencies are standard
X11, GLU, and EGL libraries. Xvfb and Xauthority come from the image's existing
Debian distribution. Image release scanning applies to the resulting image.

References:

- [VirtualGL EGL configuration and device selection](https://github.com/VirtualGL/virtualgl/blob/3.1.5/doc/index.html)
- [Blender command-line GPU and window options](https://docs.blender.org/manual/en/latest/advanced/command_line/arguments.html)
- [Blender native GPU viewport drawing](https://docs.blender.org/api/current/gpu.types.html)
- [NVIDIA container driver capabilities](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/docker-specialized.html)
- [Overall product intent](product-refactor.md)
