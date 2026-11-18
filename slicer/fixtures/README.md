# OrcaSlicer migration fixture

`orca-2.4.2-flat.3mf` contains the synthetic flat part from
[`extruded_fixture`](../preparation_fixture.py). The previous
[Printable OrcaSlicer release](https://github.com/chrisbennight/OrcaSlicer/releases/tag/local-2.4.2-63c97a1)
sliced it with the Bambu Lab X1 Carbon 0.4 nozzle, the 0.20 mm Standard
process preset, and Bambu PLA Basic on a Textured PEI Plate. The file retains
the original 2.4.2 settings and toolpaths so the smoke suite can reopen it,
apply explicit overrides, and verify that the input bytes remain unchanged.
Its toolpaths are for software testing only.
