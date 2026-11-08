//! Draft of a part's surface along the direction a mould releases it.
//!
//! A face's draft is the angle it leans off the pull direction: positive
//! when it faces partly along the pull and slides free, zero for a wall
//! parallel to the pull, and negative when it faces back against the pull, an
//! undercut that catches on the mould. With a parting plane the part sits in
//! two mould halves, each released away from the plane, so faces on either
//! side are judged against their own half's direction. A face crossing the
//! plane is split there, each part counting in its own half, and the halves'
//! patches are kept apart.
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

/// A face, or the part of one on one side of the parting plane.
struct Piece {
    triangle: usize,
    /// Whether the piece lies in the lower mould half.
    lower: bool,
    category: Category,
    draft: f64,
    area: f64,
    centre: Vector,
    corners: Vec<Vector>,
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
    let classify = |draft: f64| {
        if draft < -ZERO_DRAFT_DEGREES {
            Category::Undercut
        } else if draft + ZERO_DRAFT_DEGREES < options.minimum_draft_degrees {
            Category::Shallow
        } else {
            Category::Released
        }
    };

    let mut pieces = Vec::with_capacity(mesh.triangles.len());
    for (index, triangle) in mesh.triangles.iter().enumerate() {
        let points = triangle.map(|vertex| Vector::from_array(mesh.vertices[vertex as usize]));
        let cross = (points[1] - points[0]).cross(points[2] - points[0]);
        let doubled_area = cross.length();
        if doubled_area == 0.0 {
            continue;
        }
        let normal = cross / doubled_area;
        let heights = triangle.map(|vertex| projections[vertex as usize] - lowest);
        let Some(offset) = options.parting_offset_mm else {
            let on_base =
                normal.dot(pull) < 0.0 && heights.iter().all(|height| *height <= base_tolerance);
            let draft = normal.dot(pull).clamp(-1.0, 1.0).asin().to_degrees();
            pieces.push(Piece {
                triangle: index,
                lower: false,
                category: if on_base {
                    Category::Base
                } else {
                    classify(draft)
                },
                draft: if on_base { 0.0 } else { draft },
                area: doubled_area * 0.5,
                centre: (points[0] + points[1] + points[2]) / 3.0,
                corners: points.to_vec(),
            });
            continue;
        };
        // Each half releases away from the parting plane; a face crossing
        // it is split there and each part judged against its own half.
        for (lower, corners) in split(points, heights, offset) {
            let Some((area, centre)) = polygon_area_and_centre(&corners) else {
                continue;
            };
            let direction = if lower { -pull } else { pull };
            let draft = normal.dot(direction).clamp(-1.0, 1.0).asin().to_degrees();
            pieces.push(Piece {
                triangle: index,
                lower,
                category: classify(draft),
                draft,
                area,
                centre,
                corners,
            });
        }
    }

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
    for piece in &pieces {
        let total = match piece.category {
            Category::Base => &mut report.base,
            Category::Released => &mut report.released,
            Category::Shallow => &mut report.shallow,
            Category::Undercut => &mut report.undercut,
        };
        total.faces += 1;
        total.area_mm2 += piece.area;
    }
    (report.shallow_patch_count, report.shallow_patches) =
        patches(mesh, &pieces, Category::Shallow);
    (report.undercut_patch_count, report.undercut_patches) =
        patches(mesh, &pieces, Category::Undercut);
    report
}

/// The parts of a triangle above and below the parting plane, each with
/// whether it is the lower part. A triangle on one side gives one part.
fn split(points: [Vector; 3], heights: [f64; 3], offset: f64) -> Vec<(bool, Vec<Vector>)> {
    if heights.iter().all(|height| *height >= offset) {
        return vec![(false, points.to_vec())];
    }
    if heights.iter().all(|height| *height <= offset) {
        return vec![(true, points.to_vec())];
    }
    let (mut upper, mut lower) = (Vec::new(), Vec::new());
    for corner in 0..3 {
        let next = (corner + 1) % 3;
        let (point, height) = (points[corner], heights[corner]);
        if height >= offset {
            upper.push(point);
        }
        if height <= offset {
            lower.push(point);
        }
        let (a, b) = (height - offset, heights[next] - offset);
        if a * b < 0.0 {
            let crossing = point + (points[next] - point) * (a / (a - b));
            upper.push(crossing);
            lower.push(crossing);
        }
    }
    vec![(false, upper), (true, lower)]
}

/// The area and area-weighted centre of a flat convex polygon.
fn polygon_area_and_centre(corners: &[Vector]) -> Option<(f64, Vector)> {
    let (mut area, mut weighted) = (0.0, Vector::ZERO);
    for pair in corners[1..].windows(2) {
        let part = 0.5 * (pair[0] - corners[0]).cross(pair[1] - corners[0]).length();
        area += part;
        weighted += (corners[0] + pair[0] + pair[1]) / 3.0 * part;
    }
    (area > 0.0).then(|| (area, weighted / area))
}

/// Connected patches of pieces in `category`. Pieces in the same mould half
/// whose faces share a vertex join; the two halves never join.
fn patches(mesh: &TriangleMesh, pieces: &[Piece], category: Category) -> (usize, Vec<DraftPatch>) {
    let mut parent: Vec<usize> = (0..pieces.len()).collect();
    let mut owner: HashMap<(u32, bool), usize> = HashMap::new();
    for (index, piece) in pieces.iter().enumerate() {
        if piece.category != category {
            continue;
        }
        for &vertex in &mesh.triangles[piece.triangle] {
            let first = *owner.entry((vertex, piece.lower)).or_insert(index);
            let (a, b) = (root(&mut parent, index), root(&mut parent, first));
            parent[a] = b;
        }
    }
    let mut index_of: HashMap<usize, usize> = HashMap::new();
    let mut found: Vec<(DraftPatch, Vector, f64)> = Vec::new();
    for (index, piece) in pieces.iter().enumerate() {
        if piece.category != category {
            continue;
        }
        let key = root(&mut parent, index);
        let slot = *index_of.entry(key).or_insert_with(|| {
            let corner = piece.corners[0].to_array();
            found.push((
                DraftPatch {
                    area_mm2: 0.0,
                    faces: 0,
                    minimum_draft_degrees: piece.draft,
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
        let (patch, weighted_centre, weighted_draft) = &mut found[slot];
        patch.area_mm2 += piece.area;
        patch.faces += 1;
        patch.minimum_draft_degrees = patch.minimum_draft_degrees.min(piece.draft);
        *weighted_centre += piece.centre * piece.area;
        *weighted_draft += piece.draft * piece.area;
        for corner in &piece.corners {
            for (axis, value) in corner.to_array().into_iter().enumerate() {
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
    fn a_taper_below_the_parting_plane_is_undercut_for_the_lower_half() {
        // The walls lean in going up, which releases upwards but catches the
        // lower half as it pulls down. Every wall triangle crosses the plane.
        let report = draft(&block(20.0, 10.0, 2.0), 1.0, Some(2.0));
        let expected = 4.0 * 39.2 * 1.04_f64.sqrt();
        assert!(close(report.undercut.area_mm2, expected), "{report:?}");
        assert_eq!(report.undercut_patch_count, 1);
        let patch = &report.undercut_patches[0];
        assert!(close(patch.bounds.maximum_mm[2], 2.0));
        assert!(close(
            patch.minimum_draft_degrees,
            -(0.2_f64).atan().to_degrees()
        ));
    }

    #[test]
    fn each_mould_half_keeps_its_own_patches() {
        let report = draft(&block(20.0, 10.0, 0.0), 1.0, Some(5.0));
        assert!(close(report.shallow.area_mm2, 800.0));
        assert_eq!(report.shallow_patch_count, 2, "{report:?}");
        for patch in &report.shallow_patches {
            assert!(close(patch.area_mm2, 400.0));
            let bounds = &patch.bounds;
            assert!(close(bounds.maximum_mm[2], 5.0) || close(bounds.minimum_mm[2], 5.0));
        }
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
