use manifold_csg::Manifold;
use serde::{Deserialize, Serialize};

use crate::{GeometryError, ValidationOptions, analyze_stl, bust_stl, decode_stl, prepare_mesh};

/// A rectangle on a planar, upward-facing surface, in STL coordinates.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextureRegion {
    /// Left and right along X.
    pub x_mm: [f64; 2],
    /// Front and back along Y.
    pub y_mm: [f64; 2],
    pub surface_z_mm: f64,
}

#[derive(Debug, Serialize)]
pub struct TextureReport {
    pub region: TextureRegion,
    pub samples: [usize; 2],
    pub depth_mm: f64,
    pub applied_extreme_mm: f64,
    pub volume_change_mm3: f64,
    pub triangles: usize,
}

/// Union or recess a sampled height field on a supported planar patch. The
/// first image row maps to maximum Y. Black leaves the surface at its old Z.
pub fn bake_texture_stl(
    source: &[u8],
    mut region: TextureRegion,
    depth_mm: f64,
    [width, height]: [usize; 2],
    samples: &[u16],
) -> Result<(Vec<u8>, TextureReport), GeometryError> {
    // The source STL stores its positions in single precision.
    region.x_mm = region.x_mm.map(|value| f64::from(value as f32));
    region.y_mm = region.y_mm.map(|value| f64::from(value as f32));
    region.surface_z_mm = f64::from(region.surface_z_mm as f32);
    let [left, right] = region.x_mm;
    let [front, back] = region.y_mm;
    let surface = region.surface_z_mm;
    if [left, right, front, back, surface, depth_mm]
        .iter()
        .any(|value| !value.is_finite() || !(*value as f32).is_finite())
        || (left as f32) >= (right as f32)
        || (front as f32) >= (back as f32)
        || (surface as f32) == ((surface + depth_mm) as f32)
        || !((surface + depth_mm) as f32).is_finite()
    {
        return Err(invalid(
            "texture needs a finite, nonempty region and a nonzero depth in STL precision",
        ));
    }
    if width < 2
        || height < 2
        || width.checked_mul(height) != Some(samples.len())
        || samples.len() > (u32::MAX / 2) as usize
    {
        return Err(invalid(
            "height map needs at least two samples on each side and matching dimensions",
        ));
    }
    let level = samples.iter().copied().max().unwrap_or_default();
    let extreme = depth_mm * f64::from(level) / f64::from(u16::MAX);
    if ((surface + extreme) as f32) == (surface as f32) {
        return Err(invalid(
            "height map and depth produce no change in STL precision",
        ));
    }

    let mut source = decode_stl(source)?;
    // Estimate clearance in a coordinate frame local to the chosen surface.
    for vertex in &mut source.vertices {
        vertex[2] -= surface;
    }
    let local_region = TextureRegion {
        surface_z_mm: 0.0,
        ..region
    };
    let prepared = prepare_mesh(
        &source,
        ValidationOptions::default(),
        crate::thickness::Precision::Single,
    )?;
    let solid = prepared
        .manifold
        .ok_or_else(|| invalid("texture requires a closed source solid"))?;
    if prepared
        .report
        .issues
        .iter()
        .any(|issue| issue.code == "intersecting_shells")
    {
        return Err(invalid(
            "join intersecting source shells before baking texture",
        ));
    }

    // Give the brush a small overlap across the original surface. A solid
    // probe on either side confirms the footprint is supported and clear.
    let overlap = 0.001;
    let box_between = |bottom, top| {
        Manifold::cube(right - left, back - front, top - bottom, false)
            .translate(left, front, bottom)
    };
    let below = if depth_mm.is_sign_positive() {
        box_between(-overlap, 0.0)
    } else {
        height_field(
            local_region,
            depth_mm,
            [0.0, -overlap],
            width,
            height,
            samples,
        )?
    };
    let above = if depth_mm.is_sign_positive() {
        height_field(
            local_region,
            depth_mm,
            [0.0, overlap],
            width,
            height,
            samples,
        )?
    } else {
        box_between(0.0, overlap)
    };
    let missing = below.difference(&solid);
    let blocked = solid.intersection(&above);
    if !missing.is_empty() || !blocked.is_empty() {
        return Err(invalid(
            "the entire texture rectangle must lie on a clear upward-facing planar surface with material underneath the requested cut",
        ));
    }

    let fixed_offset = if depth_mm.is_sign_positive() {
        -overlap
    } else {
        overlap
    };
    let brush = height_field(
        local_region,
        depth_mm,
        [fixed_offset, 0.0],
        width,
        height,
        samples,
    )?;
    let textured = if depth_mm.is_sign_positive() {
        solid.union(&brush)
    } else {
        solid.difference(&brush)
    };
    textured
        .status()
        .map_err(|error| GeometryError::Kernel(format!("texture bake failed: {error}")))?;
    let mut mesh = crate::bust::mesh_from(&textured);
    for vertex in &mut mesh.vertices {
        vertex[2] += surface;
    }
    let stl = bust_stl(&mesh);
    // Check the actual exported mesh after coordinates are rounded for STL.
    let checked = analyze_stl(&stl, ValidationOptions::default())?;
    let delta = checked.signed_volume_mm3 - prepared.report.signed_volume_mm3;
    if !checked.solid_geometry
        || checked
            .issues
            .iter()
            .any(|issue| issue.code == "intersecting_shells")
        || delta * depth_mm <= 0.0
    {
        return Err(GeometryError::Kernel("baked texture did not produce a valid solid with the requested volume change in STL precision".into()));
    }
    Ok((
        stl,
        TextureReport {
            region,
            samples: [width, height],
            depth_mm,
            applied_extreme_mm: extreme,
            volume_change_mm3: delta,
            triangles: checked.triangles,
        },
    ))
}

fn height_field(
    region: TextureRegion,
    depth: f64,
    [fixed_offset, relief_offset]: [f64; 2],
    width: usize,
    height: usize,
    samples: &[u16],
) -> Result<Manifold, GeometryError> {
    let [left, right] = region.x_mm;
    let [front, back] = region.y_mm;
    let surface = region.surface_z_mm;
    let count = samples.len() as u64;
    let mut vertices = Vec::with_capacity(samples.len() * 6);
    for top in [false, true] {
        for row in 0..height {
            crate::limit::check()?;
            let y = front + (back - front) * row as f64 / (height - 1) as f64;
            for column in 0..width {
                let x = left + (right - left) * column as f64 / (width - 1) as f64;
                let sample = samples[(height - 1 - row) * width + column];
                let relief = depth * f64::from(sample) / f64::from(u16::MAX);
                let z = surface
                    + if top == depth.is_sign_positive() {
                        relief + relief_offset
                    } else {
                        fixed_offset
                    };
                vertices.extend([x, y, z]);
            }
        }
    }

    let mut faces = Vec::new();
    for row in 0..height - 1 {
        crate::limit::check()?;
        for column in 0..width - 1 {
            let a = (row * width + column) as u64;
            let b = a + 1;
            let d = a + width as u64;
            let c = d + 1;
            faces.extend([a, c, b, a, d, c]);
            faces.extend([
                count + a,
                count + b,
                count + c,
                count + a,
                count + c,
                count + d,
            ]);
        }
    }
    let perimeter: Vec<_> = (0..width - 1)
        .chain((0..height - 1).map(|row| row * width + width - 1))
        .chain((1..width).rev().map(|column| (height - 1) * width + column))
        .chain((1..height).rev().map(|row| row * width))
        .map(|index| index as u64)
        .collect();
    for (&a, &b) in perimeter.iter().zip(perimeter.iter().cycle().skip(1)) {
        faces.extend([a, b, b + count, a, b + count, a + count]);
    }
    Manifold::from_mesh_f64(&vertices, 3, &faces).map_err(|error| {
        GeometryError::Kernel(format!("height field is not a closed solid: {error}"))
    })
}

fn invalid(message: &str) -> GeometryError {
    GeometryError::InvalidOptions(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGION: TextureRegion = TextureRegion {
        x_mm: [2.0, 8.0],
        y_mm: [2.0, 8.0],
        surface_z_mm: 3.0,
    };

    fn source() -> Vec<u8> {
        bust_stl(&crate::bust::mesh_from(&Manifold::cube(
            10.0, 10.0, 3.0, false,
        )))
    }

    #[test]
    fn raised_and_recessed_maps_export_closed_geometry_with_the_same_outline() {
        let source = source();
        let map = [0, 0, 0, 0, u16::MAX, 0, 0, 0, 0];
        let box_around_patch = Manifold::cube(6.0, 6.0, 6.0, false).translate(2.0, 2.0, -1.0);
        let outside_patch = Manifold::cube(10.0, 10.0, 3.0, false).difference(&box_around_patch);
        for depth in [0.8_f64, -0.8] {
            let (stl, report) = bake_texture_stl(&source, REGION, depth, [3, 3], &map).unwrap();
            assert_eq!(report.applied_extreme_mm, depth);
            assert!(report.volume_change_mm3 * depth > 0.0);
            let mesh = decode_stl(&stl).unwrap();
            let center = mesh
                .vertices
                .iter()
                .find(|point| point[0] == 5.0 && point[1] == 5.0)
                .unwrap();
            assert!((center[2] - (3.0 + depth)).abs() < 1e-6);
            let prepared = prepare_mesh(
                &mesh,
                ValidationOptions::default(),
                crate::thickness::Precision::Single,
            )
            .unwrap();
            let baked_outside = prepared.manifold.unwrap().difference(&box_around_patch);
            assert!(outside_patch.difference(&baked_outside).is_empty());
            assert!(baked_outside.difference(&outside_patch).is_empty());
        }
    }

    #[test]
    fn a_translated_thin_plate_keeps_its_recess_clearance() {
        let surface = 1_000_000.0;
        let plate = Manifold::cube(10.0, 10.0, 0.75, false).translate(0.0, 0.0, surface - 0.75);
        let stl = bust_stl(&crate::bust::mesh_from(&plate));
        let region = TextureRegion {
            surface_z_mm: surface,
            ..REGION
        };
        let (_, report) = bake_texture_stl(&stl, region, -0.5, [2, 2], &[u16::MAX; 4]).unwrap();
        assert!((report.volume_change_mm3 + 18.0).abs() < 1e-6);
    }

    #[test]
    fn rejects_an_unsupported_region_or_a_cut_through_the_source() {
        let source = source();
        let map = [u16::MAX; 4];
        for (region, depth) in [
            (
                TextureRegion {
                    x_mm: [-1.0, 8.0],
                    ..REGION
                },
                0.8,
            ),
            (
                TextureRegion {
                    surface_z_mm: 2.0,
                    ..REGION
                },
                0.8,
            ),
            (REGION, -3.0),
        ] {
            assert!(matches!(
                bake_texture_stl(&source, region, depth, [2, 2], &map),
                Err(GeometryError::InvalidOptions(_))
            ));
        }
    }

    #[test]
    fn a_recess_checks_stock_under_the_local_depth_on_a_decimal_surface() {
        let height = 3.2;
        let mut mesh = crate::bust::mesh_from(&Manifold::cube(10.0, 10.0, height, false));
        for point in &mut mesh.vertices {
            if point[0] == 10.0 && point[2] == 0.0 {
                point[2] = height - 0.5;
            }
        }
        let region = TextureRegion {
            surface_z_mm: height,
            ..REGION
        };
        let stl = bust_stl(&mesh);
        let (_, report) =
            bake_texture_stl(&stl, region, -1.7, [2, 2], &[u16::MAX, 0, u16::MAX, 0]).unwrap();
        assert!(report.volume_change_mm3 < 0.0);
        assert!(bake_texture_stl(&stl, region, -1.7, [2, 2], &[u16::MAX; 4]).is_err());
    }

    #[test]
    fn raised_clearance_follows_the_local_relief() {
        let block = Manifold::cube(10.0, 10.0, 3.0, false);
        let overhang = Manifold::cube(1.0, 4.0, 1.0, false).translate(2.3, 3.0, 4.0);
        let stl = bust_stl(&crate::bust::mesh_from(&block.union(&overhang)));
        let samples = [0, u16::MAX, 0, u16::MAX];
        let (_, report) = bake_texture_stl(&stl, REGION, 1.7, [2, 2], &samples).unwrap();
        assert!(report.volume_change_mm3 > 0.0);
        assert!(bake_texture_stl(&stl, REGION, 1.7, [2, 2], &[u16::MAX; 4]).is_err());
    }
}
