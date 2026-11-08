//! Draft of a part's surface along the direction a mould releases it.
//!
//! A face's draft is the angle it leans off the pull direction: positive
//! when it faces partly along the pull and slides free, zero for a wall
//! parallel to the pull, and negative when it faces back against the pull, an
//! undercut that catches on the mould. With a parting plane the part sits in
//! two mould halves, each released away from the plane, so faces on either
//! side are judged against their own half's direction.
//!
//! Without a parting plane, the faces lying on the part's lowest plane and
//! facing against the pull are its base: they rest on the tray floor or the
//! mould's open face rather than being moulded.

use std::collections::HashMap;

use parry3d_f64::math::Vector;
use serde::{Deserialize, Serialize};

use crate::print_check::root;
use crate::{FaceCategory, MeshBounds, TriangleMesh};

/// At most this many patches are listed for each category, largest first.
const MAX_PATCHES: usize = 10;
/// Drafts within this many degrees of zero count as zero, so a wall parallel
/// to the pull is not an undercut through rounding.
const ZERO_DRAFT_DEGREES: f64 = 1e-6;

/// How a mould releases the part.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftOptions {
    /// Direction the part leaves the mould, or the upper mould half moves
    /// away from it. Magnitude is normalized.
    pub pull_direction: [f64; 3],
    /// Faces leaning less than this, in degrees, are reported as shallow.
    pub minimum_draft_degrees: f64,
    /// Distance along the pull from the part's lowest point to a parting
    /// plane between two mould halves. Without one, the whole part releases
    /// along the pull.
    pub parting_offset_mm: Option<f64>,
}

/// Surface draft along a mould's pull direction.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DraftReport {
    pub pull_direction: [f64; 3],
    pub minimum_draft_degrees: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parting_offset_mm: Option<f64>,
    /// Faces resting on the lowest plane, when there is no parting plane.
    pub base: FaceCategory,
    /// Faces with at least the minimum draft.
    pub released: FaceCategory,
    /// Faces parallel to the pull or leaning less than the minimum draft.
    pub shallow: FaceCategory,
    /// Faces leaning back against the pull.
    pub undercut: FaceCategory,
    pub shallow_patch_count: usize,
    /// The largest connected shallow patches, largest first.
    pub shallow_patches: Vec<DraftPatch>,
    pub undercut_patch_count: usize,
    /// The largest connected undercut patches, largest first.
    pub undercut_patches: Vec<DraftPatch>,
}

/// One connected patch of faces in the same draft category.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DraftPatch {
    pub area_mm2: f64,
    pub faces: usize,
    /// The patch's most negative draft, in degrees.
    pub minimum_draft_degrees: f64,
    /// Area-weighted mean draft, in degrees.
    pub mean_draft_degrees: f64,
    /// Area-weighted centre of the patch's faces.
    pub centre_mm: [f64; 3],
    pub bounds: MeshBounds,
}

#[derive(Clone, Copy, PartialEq)]
enum Category {
    Base,
    Released,
    Shallow,
    Undercut,
}

/// Classify every face of `mesh` by its draft. `pull` must be a unit vector.
pub(crate) fn analyze(mesh: &TriangleMesh, pull: [f64; 3], options: DraftOptions) -> DraftReport {
    let pull = Vector::from_array(pull);
    let projections: Vec<f64> = mesh
        .vertices
        .iter()
        .map(|vertex| Vector::from_array(*vertex).dot(pull))
        .collect();
    let lowest = projections.iter().copied().fold(f64::INFINITY, f64::min);
    let highest = projections
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    let base_tolerance = (highest - lowest).max(1.0) * f64::from(f32::EPSILON) * 8.0;

    let empty = || FaceCategory {
        faces: 0,
        area_mm2: 0.0,
    };
    let mut report = DraftReport {
        pull_direction: pull.to_array(),
        minimum_draft_degrees: options.minimum_draft_degrees,
        parting_offset_mm: options.parting_offset_mm,
        base: empty(),
        released: empty(),
        shallow: empty(),
        undercut: empty(),
        shallow_patch_count: 0,
        shallow_patches: Vec::new(),
        undercut_patch_count: 0,
        undercut_patches: Vec::new(),
    };
    // Per face: category, draft in degrees, area and centroid.
    let mut faces: Vec<Option<(Category, f64, f64, Vector)>> =
        Vec::with_capacity(mesh.triangles.len());
    for triangle in &mesh.triangles {
        let points = triangle.map(|index| Vector::from_array(mesh.vertices[index as usize]));
        let cross = (points[1] - points[0]).cross(points[2] - points[0]);
        let doubled_area = cross.length();
        if doubled_area == 0.0 {
            faces.push(None);
            continue;
        }
        let area = doubled_area * 0.5;
        let normal = cross / doubled_area;
        let centre = (points[0] + points[1] + points[2]) / 3.0;
        let heights = triangle.map(|index| projections[index as usize] - lowest);
        let (category, draft) = match options.parting_offset_mm {
            None if normal.dot(pull) < 0.0
                && heights.iter().all(|height| *height <= base_tolerance) =>
            {
                (Category::Base, 0.0)
            }
            parting => {
                // The lower half releases the other way.
                let below = parting.is_some_and(|offset| centre.dot(pull) - lowest < offset);
                let direction = if below { -pull } else { pull };
                let draft = normal.dot(direction).clamp(-1.0, 1.0).asin().to_degrees();
                let category = if draft < -ZERO_DRAFT_DEGREES {
                    Category::Undercut
                } else if draft + ZERO_DRAFT_DEGREES < options.minimum_draft_degrees {
                    Category::Shallow
                } else {
                    Category::Released
                };
                (category, draft)
            }
        };
        let total = match category {
            Category::Base => &mut report.base,
            Category::Released => &mut report.released,
            Category::Shallow => &mut report.shallow,
            Category::Undercut => &mut report.undercut,
        };
        total.faces += 1;
        total.area_mm2 += area;
        faces.push(Some((category, draft, area, centre)));
    }
    (report.shallow_patch_count, report.shallow_patches) = patches(mesh, &faces, Category::Shallow);
    (report.undercut_patch_count, report.undercut_patches) =
        patches(mesh, &faces, Category::Undercut);
    report
}

/// Connected patches of faces in `category`; faces sharing a vertex join.
fn patches(
    mesh: &TriangleMesh,
    faces: &[Option<(Category, f64, f64, Vector)>],
    category: Category,
) -> (usize, Vec<DraftPatch>) {
    let member = |face: usize| faces[face].is_some_and(|(kind, ..)| kind == category);
    let mut parent: Vec<usize> = (0..faces.len()).collect();
    let mut owner = vec![usize::MAX; mesh.vertices.len()];
    for (face, triangle) in mesh.triangles.iter().enumerate() {
        if !member(face) {
            continue;
        }
        for &vertex in triangle {
            let first = &mut owner[vertex as usize];
            if *first == usize::MAX {
                *first = face;
            } else {
                let (a, b) = (root(&mut parent, face), root(&mut parent, *first));
                parent[a] = b;
            }
        }
    }
    let mut index_of: HashMap<usize, usize> = HashMap::new();
    let mut found: Vec<(DraftPatch, Vector, f64)> = Vec::new();
    for (face, triangle) in mesh.triangles.iter().enumerate() {
        let Some((kind, draft, area, centre)) = faces[face] else {
            continue;
        };
        if kind != category {
            continue;
        }
        let key = root(&mut parent, face);
        let index = *index_of.entry(key).or_insert_with(|| {
            let corner = mesh.vertices[triangle[0] as usize];
            found.push((
                DraftPatch {
                    area_mm2: 0.0,
                    faces: 0,
                    minimum_draft_degrees: draft,
                    mean_draft_degrees: 0.0,
                    centre_mm: [0.0; 3],
                    bounds: MeshBounds {
                        minimum_mm: corner,
                        maximum_mm: corner,
                        dimensions_mm: [0.0; 3],
                    },
                },
                Vector::ZERO,
                0.0,
            ));
            found.len() - 1
        });
        let (patch, weighted_centre, weighted_draft) = &mut found[index];
        patch.area_mm2 += area;
        patch.faces += 1;
        patch.minimum_draft_degrees = patch.minimum_draft_degrees.min(draft);
        *weighted_centre += centre * area;
        *weighted_draft += draft * area;
        for &vertex in triangle {
            for (axis, value) in mesh.vertices[vertex as usize].into_iter().enumerate() {
                patch.bounds.minimum_mm[axis] = patch.bounds.minimum_mm[axis].min(value);
                patch.bounds.maximum_mm[axis] = patch.bounds.maximum_mm[axis].max(value);
            }
        }
    }
    let count = found.len();
    let mut patches: Vec<DraftPatch> = found
        .into_iter()
        .map(|(mut patch, weighted_centre, weighted_draft)| {
            patch.centre_mm = (weighted_centre / patch.area_mm2).to_array();
            patch.mean_draft_degrees = weighted_draft / patch.area_mm2;
            patch.bounds.dimensions_mm =
                [0, 1, 2].map(|axis| patch.bounds.maximum_mm[axis] - patch.bounds.minimum_mm[axis]);
            patch
        })
        .collect();
    patches.sort_by(|a, b| b.area_mm2.total_cmp(&a.area_mm2));
    patches.truncate(MAX_PATCHES);
    (count, patches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ValidationOptions, analyze_mesh};

    /// A closed block whose top face is inset by `taper` on every side; a
    /// negative taper widens it instead.
    fn block(size: f64, height: f64, taper: f64) -> TriangleMesh {
        let (low, high) = (0.0, size);
        let (top_low, top_high) = (taper, size - taper);
        let vertices = vec![
            [low, low, 0.0],
            [high, low, 0.0],
            [high, high, 0.0],
            [low, high, 0.0],
            [top_low, top_low, height],
            [top_high, top_low, height],
            [top_high, top_high, height],
            [top_low, top_high, height],
        ];
        let triangles = vec![
            [0, 2, 1],
            [0, 3, 2],
            [4, 5, 6],
            [4, 6, 7],
            [0, 1, 5],
            [0, 5, 4],
            [1, 2, 6],
            [1, 6, 5],
            [2, 3, 7],
            [2, 7, 6],
            [3, 0, 4],
            [3, 4, 7],
        ];
        TriangleMesh {
            vertices,
            triangles,
        }
    }

    /// An L-shaped section in X/Z, extruded 10 mm along Y: 4 mm wide up to
    /// Z = 6, then 6 mm wide up to Z = 8, so a 2 mm ledge faces down at Z = 6.
    fn ledge() -> TriangleMesh {
        let section = [
            [0.0, 0.0],
            [4.0, 0.0],
            [4.0, 6.0],
            [6.0, 6.0],
            [6.0, 8.0],
            [0.0, 8.0],
        ];
        let caps = [[0, 1, 2], [0, 2, 5], [2, 3, 4], [2, 4, 5]];
        let mut vertices = Vec::new();
        for y in [0.0, 10.0] {
            for [x, z] in section {
                vertices.push([x, y, z]);
            }
        }
        let n = section.len() as u32;
        let mut triangles = Vec::new();
        for [a, b, c] in caps {
            triangles.push([a, b, c]);
            triangles.push([a + n, c + n, b + n]);
        }
        for i in 0..n {
            let j = (i + 1) % n;
            triangles.push([i, j + n, j]);
            triangles.push([i, i + n, j + n]);
        }
        let mut mesh = TriangleMesh {
            vertices,
            triangles,
        };
        // Wind the faces outward whichever way the section runs.
        let volume: f64 = mesh
            .triangles
            .iter()
            .map(|triangle| {
                let [a, b, c] =
                    triangle.map(|index| Vector::from_array(mesh.vertices[index as usize]));
                a.dot(b.cross(c))
            })
            .sum();
        if volume < 0.0 {
            for triangle in &mut mesh.triangles {
                triangle.swap(1, 2);
            }
        }
        mesh
    }

    fn draft(mesh: &TriangleMesh, minimum: f64, parting: Option<f64>) -> DraftReport {
        let report = analyze_mesh(
            mesh,
            ValidationOptions {
                draft: Some(DraftOptions {
                    pull_direction: [0.0, 0.0, 2.0],
                    minimum_draft_degrees: minimum,
                    parting_offset_mm: parting,
                }),
                ..ValidationOptions::default()
            },
        )
        .unwrap();
        report.draft.unwrap()
    }

    fn close(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() < 1e-6
    }

    #[test]
    fn a_tapered_block_releases_and_reports_its_draft() {
        let report = draft(&block(20.0, 10.0, 2.0), 1.0, None);
        assert_eq!(report.pull_direction, [0.0, 0.0, 1.0]);
        assert_eq!(report.base.faces, 2);
        assert!(close(report.base.area_mm2, 400.0));
        assert_eq!(report.released.faces, 10);
        assert_eq!(report.shallow.faces + report.undercut.faces, 0);
        assert!(report.shallow_patches.is_empty() && report.undercut_patches.is_empty());
    }

    #[test]
    fn a_block_wider_at_the_top_is_undercut_all_round() {
        let mesh = block(20.0, 10.0, -2.0);
        let report = draft(&mesh, 1.0, None);
        assert_eq!(report.undercut.faces, 8);
        assert_eq!(report.undercut_patch_count, 1);
        let patch = &report.undercut_patches[0];
        let expected = -(2.0_f64 / 10.0).atan().to_degrees();
        assert!(close(patch.minimum_draft_degrees, expected), "{patch:?}");
        assert!(close(patch.mean_draft_degrees, expected));
        let validation = analyze_mesh(
            &mesh,
            ValidationOptions {
                draft: Some(DraftOptions {
                    pull_direction: [0.0, 0.0, 1.0],
                    minimum_draft_degrees: 1.0,
                    parting_offset_mm: None,
                }),
                ..ValidationOptions::default()
            },
        )
        .unwrap();
        assert!(
            validation
                .issues
                .iter()
                .any(|issue| issue.code == "mould_undercuts")
        );
    }

    #[test]
    fn straight_walls_are_shallow_not_undercut() {
        let report = draft(&block(20.0, 10.0, 0.0), 1.0, None);
        assert_eq!(report.shallow.faces, 8);
        assert!(close(report.shallow.area_mm2, 800.0));
        assert_eq!(report.undercut.faces, 0);
        assert_eq!(report.shallow_patch_count, 1);
        // With no minimum, a straight wall releases.
        let report = draft(&block(20.0, 10.0, 0.0), 0.0, None);
        assert_eq!(report.shallow.faces, 0);
        assert_eq!(report.released.faces, 10);
    }

    #[test]
    fn a_ledge_facing_down_is_undercut_until_the_mould_parts_below_it() {
        let one_piece = draft(&ledge(), 1.0, None);
        assert_eq!(one_piece.undercut_patch_count, 1);
        assert!(close(one_piece.undercut.area_mm2, 20.0));
        let patch = &one_piece.undercut_patches[0];
        assert!(close(patch.minimum_draft_degrees, -90.0));
        assert!(close(patch.centre_mm[2], 6.0));
        assert!(close(patch.bounds.minimum_mm[0], 4.0) && close(patch.bounds.maximum_mm[0], 6.0));

        // A parting plane at Z = 7 puts the ledge in the lower half, which
        // releases downwards.
        let two_piece = draft(&ledge(), 1.0, Some(7.0));
        assert_eq!(two_piece.parting_offset_mm, Some(7.0));
        assert_eq!(two_piece.undercut.faces, 0);
        assert_eq!(two_piece.base.faces, 0);
    }

    #[test]
    fn draft_options_are_validated() {
        let mesh = block(10.0, 10.0, 0.0);
        for (pull, minimum, parting) in [
            ([0.0, 0.0, 0.0], 1.0, None),
            ([0.0, 0.0, 1.0], -1.0, None),
            ([0.0, 0.0, 1.0], 90.0, None),
            ([0.0, 0.0, 1.0], f64::NAN, None),
            ([0.0, 0.0, 1.0], 1.0, Some(f64::INFINITY)),
        ] {
            let error = analyze_mesh(
                &mesh,
                ValidationOptions {
                    draft: Some(DraftOptions {
                        pull_direction: pull,
                        minimum_draft_degrees: minimum,
                        parting_offset_mm: parting,
                    }),
                    ..ValidationOptions::default()
                },
            )
            .unwrap_err();
            assert_eq!(error.code(), "validation", "{error}");
        }
    }
}
