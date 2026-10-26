# Licenses and bundled software

The [root MIT license](LICENSE) covers first-party source. It does not replace
the licenses of dependencies, fonts, or programs installed in the images.
Preserve the existing authorship and copyright notices when distributing source
or binaries.

| Component | Distribution boundary and notice location |
| --- | --- |
| Rust dependencies | Versions are recorded in `Cargo.lock`; each crate retains its declared license and upstream notices. Native dependencies linked by crates are also part of the binary inventory. |
| CadQuery and native CAD dependencies | Versions are recorded in `cad/requirements.txt`; installed distribution metadata and bundled license files belong to the CAD image inventory. Retain the notices and required source for the exact packages distributed. |
| Photo reconstruction runtime | PyTorch built for CUDA 13.0 with the NVIDIA CUDA runtime libraries it depends on, torchvision, timm, MediaPipe, Hugging Face diffusers and transformers, fast-simplification and their dependencies are recorded in `Dockerfile` and `recon/requirements.txt`; installed distribution metadata and bundled license files belong to the reconstruction image inventory. The depth runtime follows the inference procedure of Marigold V2 (Apache-2.0) and reads operator-supplied Qwen-Image-Edit-2509 and Marigold V2 weights (both Apache-2.0), which are not part of the image. Object generation follows the network definitions of Pixal3D and TRELLIS.2 (both MIT) and NAF (Apache-2.0), uses DINOv3 through Hugging Face transformers (Apache-2.0), and removes backgrounds with BiRefNet's network code (MIT), downloaded unmodified at a checksum-pinned revision; `recon/NOTICE` carries its license. The Pixal3D, DINOv3, NAF and BiRefNet weights are operator-supplied and not part of the image. OpenCV is built in `Dockerfile` from the hash-pinned opencv-python source with core and image processing only, and its Apache-2.0 license is installed with it. MediaPipe's face landmarker and multiclass selfie segmenter models are checksum-pinned in `Dockerfile` and are Apache-2.0 licensed. Network checkpoints are operator-supplied and are not part of the image. |
| OrcaSlicer | The fork AppImage is checksum-pinned in `Dockerfile`, with corresponding source at [the fork revision](https://github.com/chrisbennight/OrcaSlicer/tree/b08db1a4efc79abfbcfc05bcbd12005868f3b98f). It is based on upstream 2.4.2 and includes CLI thumbnail fixes and three upstream 2.4.4 backports for per-filament bounds checks and STL reader overflows. Retain its AGPL-3.0 license material, exact corresponding source, and build dependencies. The optional web resources are removed from the headless image; record that packaging change with the release materials. |
| PyYAML | MIT-licensed repository policy tooling, installed from `requirements-tooling.txt`; not a runtime or direct-client dependency. |
| Fira font | Embedded by `printable-imaging`; SIL Open Font License 1.1, with the Mozilla Foundation and Telefonica attribution in the [bundled notice](crates/printable-imaging/assets/LICENSE-Fira-OFL.txt). |
| Blender | The official archive is checksum-pinned in `blender/Dockerfile`; retain its bundled licenses and corresponding source for the exact redistributed version. See [Blender's licensing guidance](https://www.blender.org/about/license/). |
| Blender add-on | First-party MIT source is included in the image. Use with Blender must also respect Blender's GPL distribution requirements; this does not relicense unrelated Rust source. |
| urllib3 and certifi | The pinned wheels replace Blender's bundled copies. Their installed distribution metadata includes their licenses. |
| VirtualGL | The pinned Debian package retains its own license files under its installed documentation paths. |
| OpenSCAD, FFmpeg, x264, Xvfb, fonts, and Debian libraries | Installed package versions and `/usr/share/doc/*/copyright` determine the actual inventory and notices for each image. They are not all under one license. |

The server invokes OpenSCAD and FFmpeg as subprocesses. The packaged video
encoder uses `libx264`; do not describe this FFmpeg build as an LGPL-only build.
FFmpeg explains how optional GPL components affect its license in its
[official legal guidance](https://www.ffmpeg.org/legal.html).

The server image carries the first-party MIT text and Fira notice under
`/usr/share/doc/printable/`. The Blender, CAD, slicer, and reconstruction images carry the first-party MIT
text at the same location and retain their dependencies' own license material.
The reconstruction image also carries `NOTICE-recon`, which records the source
and license of the bundled face landmarker model, of its OpenCV build and of
BiRefNet's network code.
Do not strip upstream documentation directories when reducing image size.

## Binary release obligations

For each release, retain the source revision, lockfile,
Dockerfiles, exact image digests, package inventory, applicable license texts,
and the corresponding source and build material required by the components
actually shipped. Provide recipients access alongside the binaries. An SBOM or
a link to an upstream home page alone does not provide corresponding source.

Debian repositories can change between builds, so a checked-in Dockerfile does
not establish byte-for-byte reproducibility or identify every installed package
version. Capture evidence from the exact candidate images. Match Debian source
package versions to their binary versions, including distribution patches;
retain Blender's exact source and any changes; include vendored native sources
used by Rust crates where their license requires them.

Provide these materials for the exact images distributed. GPU tests do not
establish license compliance. These notes describe component boundaries and
distribution requirements, not a blanket legal certification.
