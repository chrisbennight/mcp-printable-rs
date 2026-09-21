# Presentation exposure and illumination

The optional `presentation` settings used by product renders, presented views,
and durable render jobs support two controls:

| Field | Default | Range | Effect |
| --- | --- | --- | --- |
| `exposure_stops` | 0 | -10 through 10 | Adjusts display exposure in stops. |
| `light_intensity_scale` | 1 | 0 through 10 | Multiplies all profile area lights and world illumination. |

These are service limits, not the full range of Blender's native controls.
Values must be finite numbers. A zero illumination scale disables the profile's
illumination, but does not disable emissive source materials. Exposure does not
change the energy of scene lights. These controls do not change the camera,
geometry, profile colors, or source scene. Omitted fields preserve existing
profile behavior.

For example, use this presentation object to brighten display exposure while
reducing illumination:

```json
{
  "profile": "studio_neutral",
  "exposure_stops": 1.25,
  "light_intensity_scale": 0.5
}
```

The result records effective exposure in `presentation.color_management.exposure`,
the illumination scale in `presentation.light_intensity_scale`, and resulting
energies in `presentation.lighting` and `presentation.world.strength`.
Durable jobs retain the requested controls and reject responses that ignore
explicitly requested values. This applies equally to functional parts, organic
models, and other supported evaluated geometry; no printer or assembly is required.
