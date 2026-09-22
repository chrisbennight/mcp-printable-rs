# Licenses and bundled software

The [root MIT license](LICENSE) covers first-party source. It does not replace
the licenses of dependencies, fonts, or programs installed in the images.
Preserve the existing authorship and copyright notices when distributing source
or binaries.

| Component | Distribution boundary and notice location |
| --- | --- |
| Rust dependencies | Versions are recorded in `Cargo.lock`; each crate retains its declared license and upstream notices. Native dependencies linked by crates are also part of the binary inventory. |
| CadQuery and native CAD dependencies | Versions are recorded in `cad/requirements.txt`; installed distribution metadata and bundled license files belong to the CAD image inventory. Retain the notices and required source for the exact packages distributed. |
| OrcaSlicer | The fork AppImage is checksum-pinned in `Dockerfile`, with corresponding source at [the fork revision](https://github.com/chrisbennight/OrcaSlicer/tree/6915edbfae3746da236b22630ff75a2fa304b796). It is based on upstream 2.4.2 and includes CLI thumbnail fixes. Retain its AGPL-3.0 license material, exact corresponding source, and build dependencies. The optional web resources are removed from the headless image; record that packaging change with the release materials. |
| PyYAML | MIT-licensed repository policy tooling, installed from `requirements-tooling.txt`; not a runtime or direct-client dependency. |
| Fira font | Embedded by `printable-imaging`; SIL Open Font License 1.1, with the Mozilla Foundation and Telefonica attribution in the [bundled notice](crates/printable-imaging/assets/LICENSE-Fira-OFL.txt). |
| Blender | The official archive is checksum-pinned in `blender/Dockerfile`; retain its bundled licenses and corresponding source for the exact redistributed version. See [Blender's licensing guidance](https://www.blender.org/about/license/). |
| Blender add-on | First-party MIT source is included in the image. Use with Blender must also respect Blender's GPL distribution requirements; this does not relicense unrelated Rust source. |
| urllib3 | The pinned wheel replaces Blender's bundled copy. Its installed distribution metadata includes its license. |
| VirtualGL | The pinned Debian package retains its own license files under its installed documentation paths. |
| OpenSCAD, FFmpeg, x264, Xvfb, fonts, and Debian libraries | Installed package versions and `/usr/share/doc/*/copyright` determine the actual inventory and notices for each image. They are not all under one license. |

The server invokes OpenSCAD and FFmpeg as subprocesses. The packaged video
encoder uses `libx264`; do not describe this FFmpeg build as an LGPL-only build.
FFmpeg explains how optional GPL components affect its license in its
[official legal guidance](https://www.ffmpeg.org/legal.html).

The server image carries the first-party MIT text and Fira notice under
`/usr/share/doc/printable/`. The Blender, CAD, and slicer images carry the first-party MIT
text at the same location and retain their dependencies' own license material.
Do not strip upstream documentation directories when reducing image size.

## Binary release obligations

Before distributing a qualified pair, retain the source revision, lockfile,
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

The current migration has not qualified a publicly distributed image pair.
Public binary publication remains gated on completion of this per-pair material
and NVIDIA qualification. These notes describe the project's distribution
process and component boundaries, not a blanket legal certification.
