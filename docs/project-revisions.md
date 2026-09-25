# Retain design requirements and revisions

Projects can keep an optional design manifest without changing ordinary file,
modeling, or rendering workflows. Existing project metadata stays at version 1;
there is no implicit migration or inference of requirements from old files.

Read `printable://contracts/project/revise` and
`printable://contracts/project/revision` for the typed requests. A revision
contains a version 1 manifest, explicit millimetre modeling coordinates,
numeric parameter definitions, requirements, and descriptive assumptions.
Assumptions and descriptions are data; the service never executes them.

Each parameter declares its value, unit (`mm`, `inch`, `degrees`, or `scalar`),
inclusive minimum and maximum, and description. Values enter the model script
unchanged; the script must implement the declared units. The manifest allows
bounded parameters, requirements, and notes, and rejects nonfinite or
out-of-range values. It does not infer parameter meaning from Python source.

## Capture and edit

Call `project` with `action: "revise"` and these parameters:

```json
{
  "project_id": "housing",
  "expected_parent": null,
  "source": "part.py",
  "expected_source_sha256": "<SHA-256 of the source bytes>",
  "manifest": {
    "format_version": 1,
    "units": "mm",
    "parameters": {
      "width": {"value": 40, "unit": "mm", "minimum": 30, "maximum": 80,
                "description": "Enclosure width"}
    },
    "requirements": {
      "mounts": {"kind": "hole_pattern", "centers_mm": [[-12,-8],[-12,8],[12,-8],[12,8]],
                 "radius_mm": 2, "tolerance_mm": 0.05},
      "envelope": {"kind": "build_envelope", "size_mm": [80,40,20]},
      "fit": {"kind": "physical_test", "description": "Check fastener fit on a prototype"}
    },
    "assumptions": ["Mounting-hole positions stay fixed when width changes"]
  }
}
```

Supply the digest of bytes you have read or uploaded; a pathname is not a
source identity. Include additional project-relative dependencies in `inputs`.
The combined source snapshot is bounded. The result includes `identity` with
an ID and SHA-256 digest and the complete revision record.

For the next parameter edit, supply that exact identity as `expected_parent`,
the expected source digest, and the updated manifest. The authoritative head
update is protected across cooperating server processes. Concurrent edits
cannot both replace the same parent: a contender receives busy or stale-state
rejection and must reopen the current revision. There is no automatic replay.
Ordinary source files are never overwritten by a revision update.

Read the current record with `project.revision` using `project_id`, or supply
`revision` to reopen a specific identity. Sources and records live in the
reserved `.printable/revisions` namespace and reject ordinary artifact writes.
Retain this directory in backups. A crash during capture can leave an orphan
snapshot; the head advances only after the complete revision is retained.

## Build and measure

Pass the revision `identity` as `revision` in `cad_build.model`. Keep `source`,
`inputs`, and numeric `parameters` equal to that record. CAD reads and verifies
the retained source snapshots, so replacing the original file cannot change
this build. STEP imports can also reference revisions with no script parameters.

The result adds `revision`, `requirements`, and a `measurement` descriptor.
The latter names a reserved immutable report and its digest. Its evidence
pointers are relative to the native `report` object. `project.revision` returns
the measurements directory for artifact discovery. Reports can be reopened by
their recorded paths and verified against their digests.

| Requirement | Current measured scope |
| --- | --- |
| `dimensions` | Compare native X/Y/Z bounds with the declared size and tolerance. |
| `build_envelope` | Compare those bounds with the declared axis-aligned envelope; does not choose a printer or optimize orientation. |
| `hole_pattern` | Match distinct complete vertical cylindrical inner walls to declared XY positions and radii. External bosses do not count. This is not a thread, through-hole, clearance, or fastener-fit test. |
| `clearance` | Retained as `unmeasured`; use assembly analysis for actual clearance evidence. |
| `physical_test` | Remains `physical_test_required`. |

A failed requirement makes the assessment `failed`. Missing measurements or
physical tests make it `incomplete`. `passed` means only that every declared
requirement was measured and passed; it never means complete manufacturing
qualification. Invalid geometry, unsupported hole orientations, partial
cylindrical faces, and an excessive face inventory do not supply a positive
hole match. An empty requirement set remains incomplete.

Manifests can retain sources from other modeling engines; automatic requirement
assessment currently runs on native CAD reports. An OpenSCAD or Blender source
record does not gain measured evidence merely by being captured. Explicit native
Python remains an authorized capability with filesystem access in its container;
these records provide ordinary service integrity, not a sandbox against that
code or an attestation of a model author's honesty.

The installation fixture in
[project_revision_smoke.py](../scripts/project_revision_smoke.py) widens a part
while preserving its mounting grid, moves one hole while retaining valid solid
geometry, and verifies that the old source snapshot survives file replacement.
