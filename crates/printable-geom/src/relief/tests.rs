use std::time::Duration;

use super::*;
use crate::limit::WorkLimit;
use crate::{GeometryError, ValidationOptions, analyze_mesh};

/// A small deterministic generator for property tests.
struct Random(u64);

impl Random {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A perspective scene: depth and input-convention normals per pixel.
struct Scene {
    width: usize,
    height: usize,
    focal: f64,
    depth: Vec<f32>,
    normals: Vec<[f32; 3]>,
}

impl Scene {
    /// Trace each pixel's ray against `surface`, which returns the depth
    /// along the optical axis and the camera-frame normal (x right, y down,
    /// z forward) where the ray with direction `(x, y, 1)` meets it.
    fn trace(
        width: usize,
        height: usize,
        focal: f64,
        surface: impl Fn(f64, f64) -> (f64, [f64; 3]),
    ) -> Self {
        let mut depth = Vec::new();
        let mut normals = Vec::new();
        for i in 0..width * height {
            let x = ((i % width) as f64 - (width - 1) as f64 / 2.0) / focal;
            let y = ((i / width) as f64 - (height - 1) as f64 / 2.0) / focal;
            let (z, n) = surface(x, y);
            depth.push(z as f32);
            normals.push([n[0] as f32, -n[1] as f32, -n[2] as f32]);
        }
        Self {
            width,
            height,
            focal,
            depth,
            normals,
        }
    }

    /// A sphere of `radius` centred `distance` in front of the camera, before
    /// a wall facing the camera at `wall`.
    fn sphere(size: usize, radius: f64, distance: f64, wall: f64) -> Self {
        Self::trace(size, size, size as f64, |x, y| {
            // Points z * (x, y, 1) with |p - (0, 0, distance)| = radius.
            let a = x * x + y * y + 1.0;
            let b = -2.0 * distance;
            let c = distance * distance - radius * radius;
            let discriminant = b * b - 4.0 * a * c;
            if discriminant > 0.0 {
                let z = (-b - discriminant.sqrt()) / (2.0 * a);
                let n = [x * z / radius, y * z / radius, (z - distance) / radius];
                (z, n)
            } else {
                (wall, [0.0, 0.0, -1.0])
            }
        })
    }

    fn analysis(&self) -> Analysis {
        Analysis {
            depth: Grid::new(self.width, self.height, self.depth.clone()).unwrap(),
            normals: Some(Grid::new(self.width, self.height, self.normals.clone()).unwrap()),
            mask: None,
            confidence: None,
            focal_length_px: Some(self.focal),
        }
    }

    /// Mean angle in degrees between the true normals and those of the
    /// surface `distances` describes, over pixels whose 3 x 3 neighbourhood
    /// lies on the surface where `inside` holds.
    fn normal_error(&self, distances: &[f64], inside: impl Fn(usize) -> bool) -> f64 {
        let (w, h) = (self.width, self.height);
        let point = |i: usize| {
            let x = ((i % w) as f64 - (w - 1) as f64 / 2.0) / self.focal;
            let y = ((i / w) as f64 - (h - 1) as f64 / 2.0) / self.focal;
            let z = distances[i];
            [x * z, y * z, z]
        };
        let mut total = 0.0;
        let mut count = 0;
        for y in 1..h - 1 {
            for x in 1..w - 1 {
                let i = y * w + x;
                let near = [
                    i - w - 1,
                    i - w,
                    i - w + 1,
                    i - 1,
                    i,
                    i + 1,
                    i + w - 1,
                    i + w,
                    i + w + 1,
                ];
                if !near.iter().all(|&j| inside(j)) {
                    continue;
                }
                let (pr, pl, pd, pu) = (point(i + 1), point(i - 1), point(i + w), point(i - w));
                let du = [pr[0] - pl[0], pr[1] - pl[1], pr[2] - pl[2]];
                let dv = [pd[0] - pu[0], pd[1] - pu[1], pd[2] - pu[2]];
                let n = [
                    du[1] * dv[2] - du[2] * dv[1],
                    du[2] * dv[0] - du[0] * dv[2],
                    du[0] * dv[1] - du[1] * dv[0],
                ];
                let truth = self.normals[i].map(f64::from);
                let truth = [truth[0], -truth[1], -truth[2]];
                let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                let cosine = -(n[0] * truth[0] + n[1] * truth[1] + n[2] * truth[2]) / length;
                total += cosine.clamp(-1.0, 1.0).acos().to_degrees();
                count += 1;
            }
        }
        total / count as f64
    }
}

/// Distances back from heights: the farthest subject point minus height.
fn distances(field: &HeightField, far: f64) -> Vec<f64> {
    field.heights.values().iter().map(|h| far - h).collect()
}

fn assert_sound_solid(solid: &ReliefSolid) -> f64 {
    let report = analyze_mesh(solid.mesh(), ValidationOptions::default()).unwrap();
    assert!(report.topology.watertight, "{:?}", report.issues);
    assert!(report.topology.manifold && report.topology.consistently_oriented);
    assert_eq!(report.topology.connected_components, 1);
    assert_eq!(report.topology.degenerate_triangles, 0);
    assert_eq!(report.topology.self_intersections.unwrap_or(0), 0);
    assert!(report.solid_geometry, "{:?}", report.issues);
    assert!(!report.overhang.requires_support);
    assert!((report.bounds.minimum_mm[2]).abs() < 1e-12);
    let volume = report.solid_properties.unwrap().volume_mm3;
    assert!(volume > 0.0);
    volume
}

fn map(
    width: usize,
    height: usize,
    spacing: f64,
    heights: impl Fn(usize, usize) -> f64,
) -> ReliefMap {
    let values = (0..width * height)
        .map(|i| heights(i % width, i / width))
        .collect();
    ReliefMap {
        heights_mm: Grid::new(width, height, values).unwrap(),
        spacing_mm: [spacing, spacing],
    }
}

fn integrate_with(analysis: &Analysis, method: IntegrationMethod) -> HeightField {
    integrate(
        analysis,
        &IntegrationOptions {
            method,
            ..IntegrationOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn depth_only_is_the_default_and_keeps_the_depth_map() {
    assert_eq!(
        IntegrationOptions::default().method,
        IntegrationMethod::DepthOnly
    );
    let scene = Scene::sphere(24, 0.3, 1.0, 3.0);
    let field = integrate(&scene.analysis(), &IntegrationOptions::default()).unwrap();
    assert_eq!(field.report.method, IntegrationMethod::DepthOnly);
    for (height, depth) in field.heights.values().iter().zip(&scene.depth) {
        assert!((height - (3.0 - f64::from(*depth))).abs() < 1e-6);
    }
    // The sphere stands in front of the wall across a depth jump.
    assert!(field.report.regions >= 2);
}

#[test]
fn anchored_integration_recovers_exact_depth_and_normals() {
    let scene = Scene::sphere(48, 0.3, 1.0, 1.5);
    let field = integrate_with(&scene.analysis(), IntegrationMethod::AnchoredNormals);
    assert!(field.report.converged, "{:?}", field.report);
    // Each pixel's equations use the slope at its centre while neighbouring
    // pixels differ by a finite step, so curved surfaces carry a small
    // discretisation error, largest near the sphere's outline.
    let found = distances(&field, 1.5);
    let truth: Vec<f64> = scene.depth.iter().map(|depth| f64::from(*depth)).collect();
    let mean_error = found
        .iter()
        .zip(&truth)
        .map(|(found, truth)| (found - truth).abs() / truth)
        .sum::<f64>()
        / truth.len() as f64;
    assert!(mean_error < 1e-3, "{mean_error}");
    for i in 0..truth.len() {
        assert!((found[i] - truth[i]).abs() < 0.02 * truth[i]);
        if truth[i] > 1.4 {
            assert!((found[i] - truth[i]).abs() < 1e-9);
        }
    }
    let on_sphere = |i: usize| truth[i] < 1.4;
    let angle = scene.normal_error(&found, on_sphere);
    assert!(
        angle < scene.normal_error(&truth, on_sphere) + 0.1,
        "{angle}"
    );
}

#[test]
fn anchored_normals_restore_surface_direction_lost_to_depth_noise() {
    let mut scene = Scene::sphere(64, 0.35, 1.0, 1.6);
    let clean = scene.depth.clone();
    let mut random = Random(7);
    for depth in &mut scene.depth {
        *depth *= 1.0 + 0.004 * (random.next() as f32 - 0.5);
    }
    let on_sphere = |i: usize| clean[i] < 1.5;
    let analysis = scene.analysis();
    let depth_only = integrate_with(&analysis, IntegrationMethod::DepthOnly);
    let anchored = integrate_with(&analysis, IntegrationMethod::AnchoredNormals);
    // Each pixel's distance plus its height is the farthest distance, and
    // the integrated distances match the depth map on average.
    let far = |field: &HeightField| {
        let heights = field.heights.values();
        (0..heights.len())
            .map(|i| f64::from(scene.depth[i]) + heights[i])
            .sum::<f64>()
            / heights.len() as f64
    };
    let depth_error = scene.normal_error(&distances(&depth_only, far(&depth_only)), on_sphere);
    let anchored_error = scene.normal_error(&distances(&anchored, far(&anchored)), on_sphere);
    assert!(
        anchored_error < depth_error / 3.0,
        "anchored {anchored_error} degrees against depth alone {depth_error}"
    );
}

#[test]
fn each_side_of_an_occlusion_keeps_its_own_depth() {
    // A near plane on the left and a far plane on the right, both facing
    // the camera: normals alone cannot tell how far apart they are.
    let scene = Scene::trace(32, 16, 40.0, |x, _| {
        (if x < 0.0 { 1.0 } else { 2.5 }, [0.0, 0.0, -1.0])
    });
    let analysis = scene.analysis();
    let field = integrate_with(&analysis, IntegrationMethod::AnchoredNormals);
    assert_eq!(field.report.regions, 2);
    let found = distances(&field, 2.5);
    for (found, truth) in found.iter().zip(&scene.depth) {
        assert!((found - f64::from(*truth)).abs() < 1e-9);
    }
}

#[test]
fn frankot_chellappa_integrates_an_orthographic_sphere_cap() {
    let size = 64;
    let (radius, cap) = (40.0, 20.0);
    let centre = (size - 1) as f64 / 2.0;
    let surface = |x: f64, y: f64| {
        let r2 = (x - centre).powi(2) + (y - centre).powi(2);
        if r2 < cap * cap {
            (radius * radius - r2).sqrt() - (radius * radius - cap * cap).sqrt()
        } else {
            0.0
        }
    };
    let mut normals = Vec::new();
    let mut truth = Vec::new();
    for i in 0..size * size {
        let (x, y) = ((i % size) as f64, (i / size) as f64);
        let h = surface(x, y);
        truth.push(h);
        let (hx, hy) = if h > 0.0 {
            let z = h + (radius * radius - cap * cap).sqrt();
            (-(x - centre) / z, -(y - centre) / z)
        } else {
            (0.0, 0.0)
        };
        // Image rows run down, the normal's y runs up.
        let n = [-hx, hy, 1.0];
        let length = (n[0] * n[0] + n[1] * n[1] + 1.0f64).sqrt();
        normals.push(n.map(|value| (value / length) as f32));
    }
    let analysis = Analysis {
        depth: Grid::new(size, size, vec![1.0; size * size]).unwrap(),
        normals: Some(Grid::new(size, size, normals).unwrap()),
        mask: None,
        confidence: None,
        focal_length_px: None,
    };
    let field = integrate_with(&analysis, IntegrationMethod::FrankotChellappa);
    let found = field.heights.values();
    let offset = (0..size * size).map(|i| truth[i] - found[i]).sum::<f64>() / (size * size) as f64;
    let peak = truth.iter().copied().fold(0.0, f64::max);
    for i in 0..size * size {
        assert!(
            (found[i] + offset - truth[i]).abs() < 0.03 * peak,
            "pixel {i}: {} against {}",
            found[i] + offset,
            truth[i]
        );
    }
}

#[test]
fn invalid_analyses_are_refused_with_typed_errors() {
    let scene = Scene::sphere(8, 0.3, 1.0, 1.4);
    let options = IntegrationOptions::default();

    let mut too_large = options.clone();
    too_large.max_pixels = 63;
    assert!(matches!(
        integrate(&scene.analysis(), &too_large),
        Err(ReliefError::TooLarge {
            pixels: 64,
            limit: 63
        })
    ));

    let mut analysis = scene.analysis();
    analysis.mask = Some(Grid::new(4, 16, vec![true; 64]).unwrap());
    assert!(matches!(
        integrate(&analysis, &options),
        Err(ReliefError::SizeMismatch { name: "mask", .. })
    ));
    analysis.mask = Some(Grid::new(8, 8, vec![false; 64]).unwrap());
    assert!(matches!(
        integrate(&analysis, &options),
        Err(ReliefError::EmptyMask)
    ));

    let mut analysis = scene.analysis();
    analysis.depth = Grid::new(8, 8, {
        let mut depth = scene.depth.clone();
        depth[10] = -1.0;
        depth
    })
    .unwrap();
    let error = integrate(&analysis, &options).unwrap_err();
    assert!(matches!(
        error,
        ReliefError::InvalidValue {
            name: "depth",
            x: 2,
            y: 1,
            ..
        }
    ));
    assert_eq!(error.code(), "relief_invalid_value");

    let mut anchored = options.clone();
    anchored.method = IntegrationMethod::AnchoredNormals;
    let mut analysis = scene.analysis();
    analysis.focal_length_px = None;
    assert!(matches!(
        integrate(&analysis, &anchored),
        Err(ReliefError::MissingInput(_, "a focal length"))
    ));
    analysis.normals = None;
    analysis.focal_length_px = Some(8.0);
    assert!(matches!(
        integrate(&analysis, &anchored),
        Err(ReliefError::MissingInput(_, "normals"))
    ));

    let mut supplied = options;
    supplied.method = IntegrationMethod::Supplied;
    assert!(matches!(
        integrate(&scene.analysis(), &supplied),
        Err(ReliefError::InvalidOptions(_))
    ));
    assert!(matches!(
        Grid::new(3, 3, vec![0.0f32; 8]),
        Err(ReliefError::GridSize { expected: 9, .. })
    ));
}

#[test]
fn integration_and_solids_stop_at_the_work_limit() {
    let scene = Scene::sphere(16, 0.3, 1.0, 1.4);
    let expired = WorkLimit::new(Duration::ZERO, || false);
    let stopped = |result: Result<(), ReliefError>| {
        matches!(
            result,
            Err(ReliefError::Geometry(GeometryError::TimeLimit { .. }))
        )
    };
    for method in [
        IntegrationMethod::DepthOnly,
        IntegrationMethod::AnchoredNormals,
    ] {
        let options = IntegrationOptions {
            method,
            ..IntegrationOptions::default()
        };
        let result = expired.apply(|| integrate(&scene.analysis(), &options).map(drop));
        assert!(stopped(result), "{method:?}");
    }
    let relief = map(4, 4, 1.0, |_, _| 0.0);
    let result = expired.apply(|| relief_solid(&relief, &SolidOptions::default()).map(drop));
    assert!(stopped(result));
}

#[test]
fn compression_refuses_heights_resized_away_from_the_mask() {
    let heights = Grid::new(3, 3, vec![0.0; 9]).unwrap();
    let mut field = HeightField::from_heights(heights, None).unwrap();
    field.heights = Grid::new(4, 3, vec![0.0; 12]).unwrap();
    assert!(matches!(
        compress(&field, &CompressionOptions::new(10.0, 1.0)),
        Err(ReliefError::SizeMismatch { name: "mask", .. })
    ));
}

#[test]
fn detail_scale_is_capped() {
    let heights = Grid::new(3, 3, (0..9).map(f64::from).collect()).unwrap();
    let field = HeightField::from_heights(heights, None).unwrap();
    let mut options = CompressionOptions::new(10.0, 1.0);
    options.detail_sigma_px = MAX_DETAIL_SIGMA_PX;
    assert!(compress(&field, &options).is_ok());
    options.detail_sigma_px = MAX_DETAIL_SIGMA_PX + 1.0;
    assert!(matches!(
        compress(&field, &options),
        Err(ReliefError::InvalidOptions(_))
    ));
}

#[test]
fn a_constant_slope_compresses_to_an_exact_ramp() {
    let (width, height) = (20, 6);
    let heights = Grid::new(
        width,
        height,
        (0..width * height)
            .map(|i| (i % width) as f64 * 3.0)
            .collect(),
    )
    .unwrap();
    let field = HeightField::from_heights(heights, None).unwrap();
    for compression in [0.0, 1.0, 8.0] {
        let mut options = CompressionOptions::new(19.0, 5.0);
        options.compression = compression;
        let (relief, report) = compress(&field, &options).unwrap();
        assert!(report.converged);
        assert_eq!(relief.spacing_mm, [1.0, 1.0]);
        for (i, value) in relief.heights_mm.values().iter().enumerate() {
            let expected = 5.0 * (i % width) as f64 / (width - 1) as f64;
            assert!(
                (value - expected).abs() < 1e-9,
                "{value} against {expected}"
            );
        }
    }
}

#[test]
fn logarithmic_compression_shrinks_steep_slopes_more_than_gentle_ones() {
    // Slope 1 on the left half, slope 10 on the right.
    let width = 40;
    let profile: Vec<f64> = (0..width)
        .map(|x| {
            if x < 20 {
                x as f64
            } else {
                20.0 + 10.0 * (x - 20) as f64
            }
        })
        .collect();
    let heights = Grid::new(
        width,
        4,
        (0..width * 4).map(|i| profile[i % width]).collect(),
    )
    .unwrap();
    let field = HeightField::from_heights(heights, None).unwrap();
    let ratio = |compression: f64| {
        let mut options = CompressionOptions::new(39.0, 10.0);
        options.compression = compression;
        options.detail_sigma_px = 0.0;
        let (relief, _) = compress(&field, &options).unwrap();
        let row = &relief.heights_mm.values()[..width];
        (row[35] - row[30]) / (row[10] - row[5])
    };
    assert!((ratio(0.0) - 10.0).abs() < 1e-6);
    let mut previous = ratio(0.0);
    for compression in [0.5, 2.0, 8.0] {
        let current = ratio(compression);
        assert!(
            current < previous && current > 1.0,
            "{compression}: {current}"
        );
        previous = current;
    }
}

#[test]
fn silhouettes_and_depth_jumps_lose_their_steps() {
    // A disc-shaped subject on a wall; the disc's left half stands further
    // forward than its right, and each half is a gentle dome.
    let scene = Scene::trace(48, 48, 48.0, |x, y| {
        let r2 = x * x + y * y;
        if r2 < 0.16 {
            let base = if x < 0.0 { 1.0 } else { 1.3 };
            (base - 0.2 * (0.16 - r2), [0.0, 0.0, -1.0])
        } else {
            (3.0, [0.0, 0.0, -1.0])
        }
    });
    let mut analysis = scene.analysis();
    analysis.mask = Some(
        Grid::new(
            48,
            48,
            scene.depth.iter().map(|depth| *depth < 2.0).collect(),
        )
        .unwrap(),
    );
    let field = integrate(&analysis, &IntegrationOptions::default()).unwrap();
    assert_eq!(field.report.regions, 2);
    let (relief, report) = compress(&field, &CompressionOptions::new(47.0, 4.0)).unwrap();
    assert!(report.converged);
    let heights = relief.heights_mm.values();
    let mask = field.mask.values();
    // The background stays flat. Least squares lets the subject's outline
    // dip slightly below it, so it sits a little above the lowest point.
    let background = report.background_mm.unwrap();
    assert!(background < 0.05 * 4.0, "{background}");
    for i in 0..heights.len() {
        if !mask[i] {
            assert!((heights[i] - background).abs() < 1e-3);
        }
    }
    // Across the jump the step is no larger than the dome's own slope.
    let row = 24 * 48;
    let jump = (heights[row + 23] - heights[row + 24]).abs();
    let slope = (heights[row + 15] - heights[row + 14]).abs().max(1e-3);
    assert!(jump < 3.0 * slope, "jump {jump} against slope {slope}");

    let mut keep = CompressionOptions::new(47.0, 4.0);
    keep.keep_depth_steps = true;
    let (stepped, _) = compress(&field, &keep).unwrap();
    let stepped = stepped.heights_mm.values();
    assert!((stepped[row + 23] - stepped[row + 24]).abs() > 10.0 * jump);
}

#[test]
fn flat_and_ramped_reliefs_have_their_analytic_volumes() {
    let flat = map(11, 6, 0.5, |_, _| 0.0);
    let volume = assert_sound_solid(&relief_solid(&flat, &SolidOptions::default()).unwrap());
    assert!((volume - 5.0 * 2.5 * 2.0).abs() < 1e-9);

    // The ramp rises to the right. Mirroring or inverting it alone raises
    // its left edge to the top; doing both lowers it again.
    let ramp = map(11, 6, 0.5, |x, _| x as f64 * 0.3);
    for (mirror, invert, left_edge) in [
        (false, false, 2.0),
        (true, false, 5.0),
        (false, true, 5.0),
        (true, true, 2.0),
    ] {
        let options = SolidOptions {
            mirror,
            invert,
            ..SolidOptions::default()
        };
        let mesh = relief_solid(&ramp, &options).unwrap();
        let volume = assert_sound_solid(&mesh);
        assert!((volume - 5.0 * 2.5 * (2.0 + 1.5)).abs() < 1e-9, "{volume}");
        let highest = |at_left: bool| {
            mesh.mesh()
                .vertices
                .iter()
                .filter(|v| !at_left || v[0] == 0.0)
                .map(|v| v[2])
                .fold(0.0, f64::max)
        };
        assert_eq!(highest(false), 5.0);
        assert_eq!(highest(true), left_edge);
    }
    assert_eq!(
        SolidOptions::mould(),
        SolidOptions {
            mirror: true,
            invert: true,
            ..SolidOptions::default()
        }
    );

    let mut bordered = SolidOptions {
        border_mm: 1.0,
        ..SolidOptions::default()
    };
    let volume = assert_sound_solid(&relief_solid(&flat, &bordered).unwrap());
    assert!((volume - 7.0 * 4.5 * 2.0).abs() < 1e-9);
    bordered.border_height_mm = 0.6;
    let mesh = relief_solid(&ramp, &bordered).unwrap();
    assert_sound_solid(&mesh);
    let report = analyze_mesh(mesh.mesh(), ValidationOptions::default()).unwrap();
    assert_eq!(report.bounds.dimensions_mm, [7.0, 4.5, 5.0]);
}

#[test]
fn every_random_relief_becomes_a_sound_solid() {
    let mut random = Random(0x5eed);
    for case in 0..24 {
        let width = 2 + (random.next() * 45.0) as usize;
        let height = 2 + (random.next() * 45.0) as usize;
        let spacing = 0.05 + random.next();
        let rough = random.next() < 0.5;
        let values: Vec<f64> = (0..width * height)
            .map(|_| {
                if rough {
                    random.next() * 3.0
                } else {
                    (random.next() * 4.0).floor()
                }
            })
            .collect();
        let relief = ReliefMap {
            heights_mm: Grid::new(width, height, values).unwrap(),
            spacing_mm: [spacing, spacing * (0.5 + random.next())],
        };
        let options = SolidOptions {
            base_mm: 0.2 + random.next() * 3.0,
            border_mm: if case % 3 == 0 {
                0.0
            } else {
                random.next() * 2.0
            },
            border_height_mm: random.next() * 2.0,
            mirror: case % 2 == 0,
            invert: case % 4 < 2,
        };
        let mesh = relief_solid(&relief, &options).unwrap();
        let volume = assert_sound_solid(&mesh);
        // The certification by construction agrees with the full analysis.
        let check = check_relief(&relief, &mesh, &PrintabilityOptions::default()).unwrap();
        assert!(check.solid, "case {case}");
        let certified = check.volume_mm3.unwrap();
        assert!(
            (certified - volume).abs() <= 1e-9 * volume,
            "{certified} {volume}"
        );
    }
}

#[test]
fn a_solid_off_its_construction_is_not_certified() {
    let relief = map(6, 5, 0.5, |x, y| (x * y) as f64 * 0.1);
    let solid = relief_solid(&relief, &SolidOptions::default()).unwrap();
    let options = PrintabilityOptions::default();
    assert!(check_relief(&relief, &solid, &options).unwrap().solid);
    // A top sample on the bottom, one moved off the grid, and a grid
    // reversed into an inside-out solid.
    let mut touching = solid.clone();
    touching.mesh.vertices[8][2] = 0.0;
    let mut moved = solid.clone();
    moved.mesh.vertices[8][0] += 0.1;
    let mut reversed = solid.clone();
    reversed
        .mesh
        .triangles
        .iter_mut()
        .for_each(|t| t.swap(1, 2));
    for broken in [touching, moved, reversed] {
        let check = check_relief(&relief, &broken, &options).unwrap();
        assert!(!check.solid && !check.passed);
    }
}

#[test]
fn a_mirrored_relief_is_an_exact_reflection() {
    let mut random = Random(0x0517);
    let relief = ReliefMap {
        heights_mm: Grid::new(7, 5, (0..35).map(|_| random.next() * 2.0).collect()).unwrap(),
        spacing_mm: [0.5, 0.5],
    };
    for border_mm in [0.0, 1.0] {
        let options = SolidOptions {
            border_mm,
            border_height_mm: 0.5,
            ..SolidOptions::default()
        };
        let plain = relief_solid(&relief, &options).unwrap();
        let mirrored = relief_solid(
            &relief,
            &SolidOptions {
                mirror: true,
                ..options
            },
        )
        .unwrap();
        let columns = plain.columns;
        let reflect = |index: u32| {
            let index = index as usize;
            (index / columns * columns + columns - 1 - index % columns) as u32
        };
        let faces = |triangles: &mut dyn Iterator<Item = [u32; 3]>| {
            let mut faces: Vec<[u32; 3]> = triangles
                .map(|mut triangle| {
                    triangle.sort_unstable();
                    triangle
                })
                .collect();
            faces.sort_unstable();
            faces
        };
        let reflected = faces(&mut plain.top_triangles().iter().map(|t| t.map(reflect)));
        assert_eq!(
            reflected,
            faces(&mut mirrored.top_triangles().iter().copied())
        );
        for (index, vertex) in plain.top_vertices().iter().enumerate() {
            let image = mirrored.top_vertices()[reflect(index as u32) as usize];
            assert_eq!(vertex[2], image[2]);
        }
        assert_sound_solid(&mirrored);
    }
}

#[test]
fn a_full_size_relief_is_certified_quickly() {
    let size = 2048;
    let relief = map(size, size, 0.5, |x, y| {
        ((x as f64 * 0.01).sin() + (y as f64 * 0.013).cos()) * 2.0
    });
    let started = std::time::Instant::now();
    let solid = relief_solid(&relief, &SolidOptions::default()).unwrap();
    let check = check_relief(&relief, &solid, &PrintabilityOptions::default()).unwrap();
    assert!(check.solid && check.passed);
    assert!(check.volume_mm3.unwrap() > 0.0);
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn invalid_solid_options_are_refused() {
    let relief = map(3, 3, 1.0, |_, _| 0.0);
    for options in [
        SolidOptions {
            base_mm: 0.0,
            ..SolidOptions::default()
        },
        SolidOptions {
            border_mm: -1.0,
            ..SolidOptions::default()
        },
    ] {
        assert!(matches!(
            relief_solid(&relief, &options),
            Err(ReliefError::InvalidOptions(_))
        ));
    }
    let mut bad = relief.clone();
    bad.spacing_mm = [0.0, 1.0];
    assert!(relief_solid(&bad, &SolidOptions::default()).is_err());
}

#[test]
fn detail_narrower_than_the_nozzle_is_reported() {
    // A plate 0.1 mm per sample with a ridge one sample wide, a wall twenty
    // samples wide and a groove one sample wide, each 1 mm high or deep.
    let relief = map(80, 30, 0.1, |x, _| match x {
        10 => 2.0,
        30..50 => 2.0,
        65 => 0.0,
        _ => 1.0,
    });
    let solid = relief_solid(&relief, &SolidOptions::default()).unwrap();
    let check = check_relief(&relief, &solid, &PrintabilityOptions::default()).unwrap();
    assert!(check.solid && check.passed);
    assert!((check.relief_height_mm - 2.0).abs() < 1e-12);
    // The one-sample ridge is lost over its own 30 samples; the wall is not.
    assert!((check.narrow_ridges.area_mm2 - 30.0 * 0.01).abs() < 1e-9);
    assert!((check.narrow_ridges.maximum_loss_mm - 1.0).abs() < 1e-12);
    assert!((check.narrow_grooves.area_mm2 - 30.0 * 0.01).abs() < 1e-9);

    let wide = map(
        80,
        30,
        0.1,
        |x, _| if (30..50).contains(&x) { 2.0 } else { 1.0 },
    );
    let solid = relief_solid(&wide, &SolidOptions::default()).unwrap();
    let check = check_relief(&wide, &solid, &PrintabilityOptions::default()).unwrap();
    assert_eq!(check.narrow_ridges.area_mm2, 0.0);
    assert_eq!(check.narrow_grooves.area_mm2, 0.0);
}

#[test]
fn relief_height_limits_are_checked() {
    let relief = map(12, 12, 0.5, |x, y| {
        ((x as f64 - 5.5).powi(2) + (y as f64 - 5.5).powi(2)).sqrt() * 0.1
    });
    let mould = relief_solid(&relief, &SolidOptions::mould()).unwrap();
    assert!(
        check_relief(&relief, &mould, &PrintabilityOptions::default())
            .unwrap()
            .passed
    );

    let too_tall = PrintabilityOptions {
        maximum_relief_mm: Some(0.5),
        ..PrintabilityOptions::default()
    };
    let check = check_relief(&relief, &mould, &too_tall).unwrap();
    assert!(!check.relief_height_within_limits && !check.passed);
    let flat = map(4, 4, 1.0, |_, _| 0.05);
    let solid = relief_solid(&flat, &SolidOptions::default()).unwrap();
    let check = check_relief(&flat, &solid, &PrintabilityOptions::default()).unwrap();
    assert!(!check.relief_height_within_limits);
}

#[test]
fn a_photograph_analysis_becomes_a_printable_relief() {
    let scene = Scene::sphere(40, 0.3, 1.0, 1.5);
    let mut analysis = scene.analysis();
    analysis.mask = Some(
        Grid::new(
            40,
            40,
            scene.depth.iter().map(|depth| *depth < 1.45).collect(),
        )
        .unwrap(),
    );
    for method in [
        IntegrationMethod::DepthOnly,
        IntegrationMethod::FrankotChellappa,
        IntegrationMethod::AnchoredNormals,
    ] {
        let field = integrate_with(&analysis, method);
        let (relief, report) = compress(&field, &CompressionOptions::new(40.0, 3.0)).unwrap();
        assert!(report.converged, "{method:?}");
        let mesh = relief_solid(&relief, &SolidOptions::default()).unwrap();
        let check = check_relief(&relief, &mesh, &PrintabilityOptions::default()).unwrap();
        assert!(check.passed, "{method:?}");
        assert!((check.relief_height_mm - 3.0).abs() < 1e-9);
        let top = mesh
            .mesh()
            .vertices
            .iter()
            .map(|v| v[2])
            .fold(0.0, f64::max);
        assert!((top - 5.0).abs() < 1e-9);
    }
}
