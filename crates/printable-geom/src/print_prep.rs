//! Print preparation for any mesh: rebuild it as one exact solid, report or
//! remove separate parts, and optionally add a plinth or hollow it.
//!
//! The triangles of each connected piece are first turned to face the same
//! way as most of their neighbours, and every simple opening is closed with
//! a fan of triangles, so that the winding count the rebuild relies on is
//! defined everywhere. The union of the closed surfaces is then remeshed as
//! one solid with edges of about the requested length, which closes small
//! gaps, joins overlapping or touching parts and removes self-intersections.
//! Parts of the result that do not rest on the build plate float in the air
//! and are removed unless kept.

use std::collections::HashMap;

use manifold_csg::Manifold;
use serde::{Deserialize, Serialize};

use crate::bust::{add_plinth, hollow_out, single_precision};
use crate::print_check::{self, PrintChecks, SurfaceFinding, root};
use crate::{GeometryError, Hollow, HollowReport, Plinth, PlinthReport, TriangleMesh, remesh};

/// A part whose lowest point is more than this, about one printed layer, or
/// one grid step if that is more, above the build plate does not rest on it.
/// The same band above the plate counts as resting on it in the overhang
/// check, since the first layer covers it.
const PLATE_TOLERANCE_MM: f64 = 0.2;
/// The default edge is this share of the longest side, which keeps a rebuilt
/// solid at a few hundred thousand triangles whatever its size and, at
/// 100 mm, matches a 0.4 mm nozzle; but no finer than the minimum.
const DEFAULT_EDGE_SHARE: f64 = 0.004;
const MIN_DEFAULT_EDGE_MM: f64 = 0.1;

/// Choices for [`prepare_print`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrintPrepOptions {
    /// Edge length of the rebuilt surface. Detail finer than about one and
    /// a half edges is smoothed away. Defaults to 0.4% of the mesh's longest
    /// side, at least 0.1 mm.
    #[serde(default)]
    pub edge_length_mm: Option<f64>,
    /// Without an edge length, choose one that gives about this many
    /// triangles over the source's surface area.
    #[serde(default)]
    pub target_triangles: Option<u64>,
    /// Which parts of the rebuilt solid are kept.
    #[serde(default)]
    pub parts: KeptParts,
    /// Keep each closed piece's facing, so that one facing inwards stays a
    /// cavity. By default every closed piece is turned to face outwards,
    /// which never loses material but fills intended cavities.
    #[serde(default)]
    pub keep_cavities: bool,
    #[serde(default)]
    pub plinth: Option<Plinth>,
    #[serde(default)]
    pub hollow: Option<Hollow>,
    #[serde(default)]
    pub checks: Option<PrintChecks>,
    /// Scale the result so that its longest side has this length, centred on
    /// the origin, as a generated object is sized.
    #[serde(default)]
    pub longest_side_mm: Option<f64>,
}

/// Which separate parts of the rebuilt solid are kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeptParts {
    /// Every part. The build plate is under the lowest of them, and parts
    /// above it are reported as floating.
    #[default]
    All,
    /// The largest part and those resting on the build plate under it, above
    /// or below; the others are removed as floating.
    Grounded,
    /// Only the largest part, for a single photographed subject.
    Largest,
}

/// The mesh as it was given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceSummary {
    pub triangles: usize,
    /// Pieces connected through edges used by exactly two triangles.
    pub shells: usize,
    /// Edges used by only one triangle: the rims of holes.
    pub open_edges: usize,
    /// Absent when the surface has openings, since it then encloses no
    /// definite volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_mm3: Option<f64>,
}

/// Separate parts of the rebuilt solid: those floating above the build
/// plate, which lies under the largest part, or those other than the largest.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Parts {
    pub count: usize,
    pub volume_mm3: f64,
    pub removed: bool,
}

/// Surface found by a check, in millimetres on the prepared solid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub area_mm2: f64,
    /// The largest connected regions, largest first.
    pub regions: Vec<Region>,
    /// Faces resting on the build plate were not measured, so a thin part
    /// lying flat on the plate is not found.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub excludes_plate_faces: bool,
}

/// One connected region of a [`Finding`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub area_mm2: f64,
    /// A point on the region.
    pub centre_mm: [f64; 3],
}

impl From<SurfaceFinding> for Finding {
    fn from(found: SurfaceFinding) -> Self {
        Self {
            area_mm2: found.area_mm2,
            regions: found
                .regions
                .into_iter()
                .map(|region| Region {
                    area_mm2: region.area_mm2,
                    centre_mm: region.centre_mm,
                })
                .collect(),
            excludes_plate_faces: false,
        }
    }
}

/// What the preparation found and changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrintPrepReport {
    pub source: SourceSummary,
    /// Triangles turned to face the same way as most of their piece.
    pub flipped_triangles: usize,
    /// Closed pieces facing inwards that were turned to face outwards, which
    /// fills any that were meant as cavities.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cavities_filled: usize,
    /// With `keep_cavities`, closed pieces facing inwards kept as cavities.
    /// One that is inside no other part disappears in the rebuild.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cavities: usize,
    /// Openings closed before the rebuild.
    pub openings_closed: usize,
    /// Openings whose rim crosses itself or meets another, left for the
    /// rebuild to resolve.
    pub openings_left: usize,
    /// Facet size of the solid as written, after any rescaling.
    pub edge_length_mm: f64,
    pub triangles: usize,
    /// Separate solids in the result, each printable on its own.
    pub shells: usize,
    pub volume_mm3: f64,
    /// Extent along x, y and z; the solid rests on z = 0.
    pub size_mm: [f64; 3],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating_parts: Option<Parts>,
    /// With every part kept, those that `remove_floating_parts` would
    /// remove: parts other than the largest not resting on the plate under
    /// it, above or below. They can differ from `floating_parts`, which are
    /// judged from the lowest part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub would_remove: Option<Parts>,
    /// With only the largest part kept, the others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smaller_parts: Option<Parts>,
    /// Separate parts lying wholly within the slice the plinth's flat base
    /// cuts off, which are removed with it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub removed_by_base_cut: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plinth: Option<PlinthReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hollow: Option<HollowReport>,
    /// Surface facing down more steeply than the checked angle on the solid
    /// as written, other than at the build plate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overhang: Option<Finding>,
    /// Surface where the solid is thinner than the checked size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thin: Option<Finding>,
}

/// Rebuild a mesh, in millimetres with Z up, as a printable solid standing
/// on z = 0.
pub fn prepare_print(
    mesh: TriangleMesh,
    request: &PrintPrepOptions,
) -> Result<(TriangleMesh, PrintPrepReport), GeometryError> {
    let TriangleMesh {
        mut vertices,
        mut triangles,
    } = mesh;
    if vertices.is_empty() || triangles.is_empty() {
        return Err(GeometryError::EmptyMesh);
    }
    if let Some(index) = vertices
        .iter()
        .position(|vertex| vertex.iter().any(|value| !value.is_finite()))
    {
        return Err(GeometryError::NonFiniteVertex { index });
    }
    if let Some(triangle) = triangles.iter().position(|triangle| {
        triangle
            .iter()
            .any(|&index| index as usize >= vertices.len())
    }) {
        return Err(GeometryError::InvalidIndex { triangle });
    }
    // Degenerate triangles carry no surface and confuse orientation.
    triangles.retain(|[a, b, c]| a != b && b != c && c != a);
    if triangles.is_empty() {
        return Err(GeometryError::EmptyMesh);
    }
    let pieces = pieces(&triangles);
    let (open_edges, _) = edge_counts(&triangles);
    let source = SourceSummary {
        triangles: triangles.len(),
        shells: pieces.count,
        open_edges,
        volume_mm3: (open_edges == 0).then(|| signed_volume(&vertices, &triangles)),
    };
    let flipped_triangles = orient(&mut triangles, &pieces);
    let (openings_closed, openings_left) = close_openings(&mut vertices, &mut triangles);
    drop(pieces);
    let inward = inward_pieces(&vertices, &mut triangles, !request.keep_cavities);
    let (cavities_filled, cavities) = if request.keep_cavities {
        (0, inward)
    } else {
        (inward, 0)
    };
    // Sized after facing is settled, so the largest piece is the one that
    // encloses the most volume, as the part finally kept is.
    let edge = match (request.edge_length_mm, request.target_triangles) {
        (Some(edge), _) => edge,
        // An equilateral triangle with edge e has area e^2 * sqrt(3) / 4.
        (None, Some(target)) => {
            (4.0 * if request.parts == KeptParts::Largest {
                largest_piece(&vertices, &triangles).0
            } else {
                surface_area(&vertices, &triangles)
            } / (3f64.sqrt() * target.max(1) as f64))
                .sqrt()
                .max(MIN_DEFAULT_EDGE_MM)
        }
        // Only the largest part is kept, so its size sets the default.
        (None, None) => (DEFAULT_EDGE_SHARE
            * if request.parts == KeptParts::Largest {
                largest_piece(&vertices, &triangles).1
            } else {
                print_check::extent(&vertices)
            })
        .max(MIN_DEFAULT_EDGE_MM),
    };
    if !(edge.is_finite() && edge > 0.0) {
        return Err(GeometryError::InvalidOptions(
            "edge length must be a positive number of millimetres".into(),
        ));
    }

    // By default anything inside any closed surface is solid, whichever way
    // it faces, so a body whose facing was misjudged is never lost. Keeping
    // cavities trusts the mesh's facing.
    let rule = if request.keep_cavities {
        remesh::InsideRule::Positive
    } else {
        remesh::InsideRule::NonZero
    };
    let solid = remesh::remesh_union_with(vertices, triangles, edge, rule)?;
    let spacing = edge / remesh::EDGE_PER_SPACING;
    let mut parts = solids(&solid);
    parts.sort_by(|a, b| b.volume().total_cmp(&a.volume()));
    let lowest_of = |part: &Manifold| part.bounding_box().map_or(f64::INFINITY, |b| b.min()[2]);
    let tolerance = PLATE_TOLERANCE_MM.max(spacing);
    // The parts removing floating parts would delete: those other than the
    // largest not resting on the plate under it, above or below.
    let not_grounded = |part: &Manifold, plate: f64| (lowest_of(part) - plate).abs() > tolerance;
    let mut would_remove = None;
    let (floating_parts, smaller_parts, removed) = match request.parts {
        KeptParts::All => {
            let largest_plate = parts.first().map_or(0.0, lowest_of);
            let removable: Vec<&Manifold> = parts
                .iter()
                .skip(1)
                .filter(|part| not_grounded(part, largest_plate))
                .collect();
            would_remove = (!removable.is_empty()).then(|| Parts {
                count: removable.len(),
                volume_mm3: removable.iter().map(|part| part.volume()).sum(),
                removed: false,
            });
            // Every part stays, so the plate is where the lowest one sits.
            let plate = parts.iter().map(lowest_of).fold(f64::INFINITY, f64::min);
            let floating: Vec<&Manifold> = parts
                .iter()
                .filter(|part| lowest_of(part) > plate + tolerance)
                .collect();
            let report = (!floating.is_empty()).then(|| Parts {
                count: floating.len(),
                volume_mm3: floating.iter().map(|part| part.volume()).sum(),
                removed: false,
            });
            (report, None, Vec::new())
        }
        KeptParts::Grounded => {
            // The plate is under the largest part; any other part not
            // resting on it, above or below, goes.
            let plate = parts.first().map_or(0.0, lowest_of);
            let floating: Vec<Manifold> = parts
                .into_iter()
                .skip(1)
                .filter(|part| not_grounded(part, plate))
                .collect();
            let report = (!floating.is_empty()).then(|| Parts {
                count: floating.len(),
                volume_mm3: floating.iter().map(Manifold::volume).sum(),
                removed: true,
            });
            (report, None, floating)
        }
        KeptParts::Largest => {
            let others: Vec<Manifold> = parts.into_iter().skip(1).collect();
            let report = (!others.is_empty()).then(|| Parts {
                count: others.len(),
                volume_mm3: others.iter().map(Manifold::volume).sum(),
                removed: true,
            });
            (None, report, others)
        }
    };
    // Subtracting removed parts keeps any cavity inside the others.
    let solid = if removed.is_empty() {
        solid
    } else {
        Manifold::batch_difference(&[vec![solid], removed].concat())
    };
    let bounds = solid
        .bounding_box()
        .ok_or_else(|| GeometryError::Kernel("the prepared solid is empty".into()))?;
    let (low, high) = (bounds.min(), bounds.max());
    let (solid, scale) = match request.longest_side_mm {
        Some(longest) => {
            let scale = longest / bounds.dimensions().into_iter().fold(0.0, f64::max);
            let solid = solid
                .translate(
                    -(low[0] + high[0]) / 2.0,
                    -(low[1] + high[1]) / 2.0,
                    -low[2],
                )
                .scale(scale, scale, scale);
            (solid, scale)
        }
        None => (solid.translate(0.0, 0.0, -low[2]), 1.0),
    };
    let mut removed_by_base_cut = 0;
    let (solid, plinth) = match request.plinth {
        Some(plinth) => {
            // A flat base one grid step above the lowest point, so the solid
            // meets the plinth over a whole section rather than at points.
            // Cutting at exactly zero puts the base exactly on the plinth.
            // The grid step on the solid as rescaled. A part lying wholly
            // below the cut is lost with it.
            let cut = spacing * scale;
            removed_by_base_cut = parts_below(&solid, cut);
            let base = solid
                .translate(0.0, 0.0, -cut)
                .trim_by_plane([0.0, 0.0, 1.0], 0.0);
            drop(solid);
            let (solid, report) = add_plinth(base, plinth)?;
            (solid, Some(report))
        }
        None => (solid, None),
    };
    let raise = plinth.map_or(0.0, |plinth| plinth.height_mm);
    let (solid, hollow) = match request.hollow {
        Some(_) if solids(&solid).len() > 1 => {
            return Err(GeometryError::InvalidOptions(
                "hollowing needs one solid, but the rebuilt mesh has several separate parts; join them with a plinth or remove them first".into(),
            ));
        }
        Some(hollow) => {
            let (solid, report) = hollow_out(solid, hollow, usize::MAX)?;
            (solid, Some(report))
        }
        None => (solid, None),
    };
    let shells = solids(&solid).len();
    let size_mm = solid
        .bounding_box()
        .map(|bounds| bounds.dimensions())
        .ok_or_else(|| GeometryError::Kernel("the prepared solid is empty".into()))?;
    let volume_mm3 = solid.volume();
    let mesh = single_precision(&solid)?;
    drop(solid);
    let overhang = request.checks.map(|checks| {
        print_check::overhang_above(&mesh, checks.overhang_degrees, &[], PLATE_TOLERANCE_MM).into()
    });
    let thin = match request.checks.and_then(|checks| checks.min_feature_mm) {
        Some(min_mm) => Some(Finding {
            excludes_plate_faces: true,
            ..print_check::thin(&mesh, min_mm, raise, &[])?.into()
        }),
        None => None,
    };
    let report = PrintPrepReport {
        source,
        flipped_triangles,
        cavities_filled,
        cavities,
        openings_closed,
        openings_left,
        // The facets' size on the solid as written, after any rescaling.
        edge_length_mm: edge * scale,
        triangles: mesh.triangles.len(),
        shells,
        volume_mm3,
        size_mm,
        floating_parts,
        would_remove,
        smaller_parts,
        removed_by_base_cut,
        plinth,
        hollow,
        overhang,
        thin,
    };
    Ok((mesh, report))
}

/// How many separate parts of `solid` lie wholly at or below `height`.
fn parts_below(solid: &Manifold, height: f64) -> usize {
    solids(solid)
        .iter()
        .filter(|part| part.bounding_box().is_some_and(|b| b.max()[2] <= height))
        .count()
}

/// The separate solids of `solid`, without the closed cavities inside them,
/// which the kernel lists as pieces of negative volume.
fn solids(solid: &Manifold) -> Vec<Manifold> {
    solid
        .decompose()
        .into_iter()
        .filter(|piece| piece.volume() > 0.0)
        .collect()
}

/// Connected pieces of a mesh, joined through edges used by exactly two
/// triangles.
struct Pieces {
    /// The piece of each triangle, numbered from zero.
    of_triangle: Vec<u32>,
    count: usize,
}

fn pieces(triangles: &[[u32; 3]]) -> Pieces {
    // Triangles sharing an edge that only they use belong together. Bodies
    // that touch at a vertex, or at an edge that more than two triangles
    // use, stay apart, so each closed body is oriented on its own.
    let mut parent: Vec<usize> = (0..triangles.len()).collect();
    for group in undirected_edges(triangles).chunk_by(|a, b| (a.0, a.1) == (b.0, b.1)) {
        if let [first, second] = group {
            let (p, q) = (
                root(&mut parent, first.2 as usize),
                root(&mut parent, second.2 as usize),
            );
            parent[p] = q;
        }
    }
    let mut numbers = HashMap::new();
    let of_triangle = (0..triangles.len())
        .map(|face| {
            let next = numbers.len() as u32;
            *numbers.entry(root(&mut parent, face)).or_insert(next)
        })
        .collect();
    Pieces {
        of_triangle,
        count: numbers.len(),
    }
}

/// Every edge as (low vertex, high vertex, triangle), sorted.
fn undirected_edges(triangles: &[[u32; 3]]) -> Vec<(u32, u32, u32)> {
    let mut edges: Vec<(u32, u32, u32)> = triangles
        .iter()
        .enumerate()
        .flat_map(|(face, [a, b, c])| {
            [(*a, *b), (*b, *c), (*c, *a)].map(|(p, q)| (p.min(q), p.max(q), face as u32))
        })
        .collect();
    edges.sort_unstable();
    edges
}

/// Edges used by one triangle, and by more than two.
fn edge_counts(triangles: &[[u32; 3]]) -> (usize, usize) {
    let edges = undirected_edges(triangles);
    let (mut open, mut crowded) = (0, 0);
    for group in edges.chunk_by(|a, b| (a.0, a.1) == (b.0, b.1)) {
        match group.len() {
            1 => open += 1,
            2 => {}
            _ => crowded += 1,
        }
    }
    (open, crowded)
}

/// Whether `triangle` runs from `from` to `to` along one of its edges.
fn runs(triangle: [u32; 3], from: u32, to: u32) -> bool {
    let [a, b, c] = triangle;
    [(a, b), (b, c), (c, a)].contains(&(from, to))
}

/// Turn triangles so that neighbours across each edge shared by exactly two
/// triangles face the same way, keeping in each piece the facing of most of
/// its triangles. Returns how many were turned.
fn orient(triangles: &mut [[u32; 3]], pieces: &Pieces) -> usize {
    let edges = undirected_edges(triangles);
    let mut neighbours: Vec<Vec<(u32, bool)>> = vec![Vec::new(); triangles.len()];
    for group in edges.chunk_by(|a, b| (a.0, a.1) == (b.0, b.1)) {
        if let [(p, q, first), (_, _, second)] = group {
            // Consistent neighbours run along the shared edge in opposite
            // directions.
            let same = runs(triangles[*first as usize], *p, *q)
                == runs(triangles[*second as usize], *p, *q);
            neighbours[*first as usize].push((*second, same));
            neighbours[*second as usize].push((*first, same));
        }
    }
    drop(edges);
    let mut flip: Vec<Option<bool>> = vec![None; triangles.len()];
    let mut stack = Vec::new();
    for start in 0..triangles.len() {
        if flip[start].is_some() {
            continue;
        }
        flip[start] = Some(false);
        stack.push(start);
        while let Some(face) = stack.pop() {
            let flipped = flip[face].expect("visited faces are assigned");
            for &(other, same) in &neighbours[face] {
                let other = other as usize;
                if flip[other].is_none() {
                    flip[other] = Some(flipped ^ same);
                    stack.push(other);
                }
            }
        }
    }
    drop(neighbours);
    // Keep the majority facing of each piece.
    let mut counts = vec![(0usize, 0usize); pieces.count];
    for (face, flipped) in flip.iter().enumerate() {
        let count = &mut counts[pieces.of_triangle[face] as usize];
        if flipped == &Some(true) {
            count.0 += 1;
        } else {
            count.1 += 1;
        }
    }
    let mut turned = 0;
    for (face, flipped) in flip.into_iter().enumerate() {
        let (yes, no) = counts[pieces.of_triangle[face] as usize];
        if flipped == Some(true) && yes <= no || flipped == Some(false) && yes > no {
            triangles[face].swap(1, 2);
            turned += 1;
        }
    }
    turned
}

/// Close each opening whose rim is a simple loop with a fan of triangles
/// around its centre. Returns the openings closed and those left.
fn close_openings(vertices: &mut Vec<[f64; 3]>, triangles: &mut Vec<[u32; 3]>) -> (usize, usize) {
    let mut directed: Vec<(u32, u32)> = triangles
        .iter()
        .flat_map(|[a, b, c]| [(*a, *b), (*b, *c), (*c, *a)])
        .collect();
    directed.sort_unstable();
    // A rim edge has no triangle running the other way.
    let mut next: HashMap<u32, u32> = HashMap::new();
    let mut branching = Vec::new();
    for &(a, b) in &directed {
        if directed.binary_search(&(b, a)).is_err() && next.insert(a, b).is_some() {
            branching.push(a);
        }
    }
    drop(directed);
    let mut starts: Vec<u32> = next.keys().copied().collect();
    starts.sort_unstable();
    let mut visited = std::collections::HashSet::new();
    let (mut closed, mut left) = (0, 0);
    for start in starts {
        if visited.contains(&start) {
            continue;
        }
        let mut rim = Vec::new();
        let mut current = start;
        let simple = loop {
            if !visited.insert(current) {
                break current == start;
            }
            if branching.contains(&current) {
                break false;
            }
            rim.push(current);
            match next.get(&current) {
                Some(&following) => current = following,
                None => break false,
            }
        };
        if !simple || rim.len() < 3 {
            left += 1;
            continue;
        }
        let count = rim.len() as f64;
        let centre = rim
            .iter()
            .fold([0.0; 3], |sum, &index| {
                let point = vertices[index as usize];
                [sum[0] + point[0], sum[1] + point[1], sum[2] + point[2]]
            })
            .map(|value| value / count);
        let Ok(centre_index) = u32::try_from(vertices.len()) else {
            left += 1;
            continue;
        };
        vertices.push(centre);
        // Each cap triangle runs against its rim edge, so the cap faces the
        // same way as the surface around it.
        for (position, &a) in rim.iter().enumerate() {
            let b = rim[(position + 1) % rim.len()];
            triangles.push([b, a, centre_index]);
        }
        closed += 1;
    }
    (closed, left)
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// The closed pieces enclosing negative volume, which face inwards, turned
/// to face outwards when `turn` is set. A piece is closed when each of its
/// edges is used by exactly two of its triangles; an open piece, such as a
/// face two bodies share, encloses no definite volume and keeps its facing.
/// Returns how many there are.
fn inward_pieces(vertices: &[[f64; 3]], triangles: &mut [[u32; 3]], turn: bool) -> usize {
    let pieces = pieces(triangles);
    let mut closed = vec![true; pieces.count];
    for group in undirected_edges(triangles).chunk_by(|a, b| (a.0, a.1) == (b.0, b.1)) {
        // Each piece using this edge must use it exactly twice.
        let mut uses: HashMap<u32, usize> = HashMap::new();
        for &(_, _, face) in group {
            *uses.entry(pieces.of_triangle[face as usize]).or_default() += 1;
        }
        for (piece, count) in uses {
            if count != 2 {
                closed[piece as usize] = false;
            }
        }
    }
    let mut volume = vec![0.0; pieces.count];
    for (face, triangle) in triangles.iter().enumerate() {
        volume[pieces.of_triangle[face] as usize] +=
            signed_volume(vertices, std::slice::from_ref(triangle));
    }
    let inward: Vec<bool> = (0..pieces.count)
        .map(|piece| closed[piece] && volume[piece] < 0.0)
        .collect();
    if turn {
        for (face, triangle) in triangles.iter_mut().enumerate() {
            if inward[pieces.of_triangle[face] as usize] {
                triangle.swap(1, 2);
            }
        }
    }
    inward.iter().filter(|inward| **inward).count()
}

/// The area and longest side of the piece enclosing the most volume.
fn largest_piece(vertices: &[[f64; 3]], triangles: &[[u32; 3]]) -> (f64, f64) {
    let pieces = pieces(triangles);
    let mut volume = vec![0.0; pieces.count];
    let mut area = vec![0.0; pieces.count];
    for (face, triangle) in triangles.iter().enumerate() {
        let piece = pieces.of_triangle[face] as usize;
        let one = std::slice::from_ref(triangle);
        volume[piece] += signed_volume(vertices, one);
        area[piece] += surface_area(vertices, one);
    }
    let Some(largest) =
        (0..pieces.count).max_by(|a, b| volume[*a].abs().total_cmp(&volume[*b].abs()))
    else {
        return (0.0, 0.0);
    };
    let corners: Vec<[f64; 3]> = triangles
        .iter()
        .enumerate()
        .filter(|(face, _)| pieces.of_triangle[*face] as usize == largest)
        .flat_map(|(_, triangle)| triangle.map(|index| vertices[index as usize]))
        .collect();
    (area[largest], print_check::extent(&corners))
}

/// The total area of the triangles that refer to valid vertices.
fn surface_area(vertices: &[[f64; 3]], triangles: &[[u32; 3]]) -> f64 {
    triangles
        .iter()
        .filter(|triangle| {
            triangle
                .iter()
                .all(|&index| (index as usize) < vertices.len())
        })
        .map(|triangle| {
            let [a, b, c] = triangle.map(|index| vertices[index as usize]);
            let (u, v) = (
                [b[0] - a[0], b[1] - a[1], b[2] - a[2]],
                [c[0] - a[0], c[1] - a[1], c[2] - a[2]],
            );
            let normal = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt() / 2.0
        })
        .sum()
}

/// Volume enclosed by a closed surface, positive when it faces outwards.
fn signed_volume(vertices: &[[f64; 3]], triangles: &[[u32; 3]]) -> f64 {
    triangles
        .iter()
        .map(|triangle| {
            let [a, b, c] = triangle.map(|index| vertices[index as usize]);
            (a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
                + a[2] * (b[0] * c[1] - b[1] * c[0]))
                / 6.0
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    type Soup = (Vec<[f64; 3]>, Vec<[u32; 3]>);

    /// An axis-aligned box from `low` with the given size, facing outwards.
    fn cuboid(low: [f64; 3], size: [f64; 3]) -> Soup {
        let vertices = (0..8)
            .map(|corner| {
                [0, 1, 2].map(|axis| {
                    low[axis]
                        + if corner >> axis & 1 == 1 {
                            size[axis]
                        } else {
                            0.0
                        }
                })
            })
            .collect();
        let triangles = vec![
            [0, 2, 1],
            [1, 2, 3],
            [4, 5, 6],
            [5, 7, 6],
            [0, 1, 4],
            [1, 5, 4],
            [2, 6, 3],
            [3, 6, 7],
            [0, 4, 2],
            [2, 4, 6],
            [1, 3, 5],
            [3, 7, 5],
        ];
        (vertices, triangles)
    }

    fn joined(parts: &[Soup]) -> Soup {
        let mut vertices = Vec::new();
        let mut triangles = Vec::new();
        for (part_vertices, part_triangles) in parts {
            let offset = vertices.len() as u32;
            vertices.extend(part_vertices);
            triangles.extend(
                part_triangles
                    .iter()
                    .map(|triangle| triangle.map(|index| index + offset)),
            );
        }
        (vertices, triangles)
    }

    fn prepare(
        mesh: Soup,
        options: &PrintPrepOptions,
    ) -> Result<(TriangleMesh, PrintPrepReport), GeometryError> {
        prepare_print(
            TriangleMesh {
                vertices: mesh.0,
                triangles: mesh.1,
            },
            options,
        )
    }

    fn request(edge: f64) -> PrintPrepOptions {
        PrintPrepOptions {
            edge_length_mm: Some(edge),
            target_triangles: None,
            parts: KeptParts::All,
            keep_cavities: false,
            plinth: None,
            hollow: None,
            checks: None,
            longest_side_mm: None,
        }
    }

    #[test]
    fn overlapping_shells_merge_and_floating_parts_are_reported_or_removed() {
        // Two overlapping cubes on the plate, and a small cube in the air.
        let mesh = joined(&[
            cuboid([0.0; 3], [10.0; 3]),
            cuboid([5.0, 5.0, 0.0], [10.0; 3]),
            cuboid([30.0, 0.0, 20.0], [4.0; 3]),
        ]);
        let (_, kept) = prepare(mesh.clone(), &request(0.5)).unwrap();
        assert_eq!(kept.source.shells, 3);
        assert_eq!(kept.source.open_edges, 0);
        assert!((kept.source.volume_mm3.unwrap() - 2064.0).abs() < 1e-6);
        // Every part is kept by default, and the floating one reported.
        assert_eq!(kept.shells, 2);
        let floating = kept.floating_parts.unwrap();
        assert_eq!((floating.count, floating.removed), (1, false));
        assert!((floating.volume_mm3 - 64.0).abs() < 8.0, "{floating:?}");

        let mut grounded = request(0.5);
        grounded.parts = KeptParts::Grounded;
        let (prepared, report) = prepare(mesh, &grounded).unwrap();
        assert_eq!(report.shells, 1);
        assert!(report.floating_parts.unwrap().removed);
        // Two 1000 mm^3 cubes sharing 250 mm^3.
        assert!((report.volume_mm3 - 1750.0).abs() < 50.0, "{report:?}");
        assert!(report.size_mm[2] < 10.5, "{report:?}");
        let low = prepared
            .vertices
            .iter()
            .map(|vertex| vertex[2])
            .fold(f64::INFINITY, f64::min);
        assert!(low.abs() < 1e-6);
    }

    #[test]
    fn only_the_largest_part_is_kept_when_asked() {
        // A small fragment below the subject and a block beside it.
        let mesh = joined(&[
            cuboid([0.0, 0.0, 5.0], [20.0; 3]),
            cuboid([30.0, 0.0, 0.0], [2.0; 3]),
            cuboid([30.0, 10.0, 5.0], [5.0; 3]),
        ]);
        let mut largest = request(0.5);
        largest.parts = KeptParts::Largest;
        let (prepared, report) = prepare(mesh, &largest).unwrap();
        let smaller = report.smaller_parts.unwrap();
        assert_eq!((smaller.count, smaller.removed), (2, true));
        assert_eq!(report.shells, 1);
        assert!((report.volume_mm3 - 8000.0).abs() < 200.0, "{report:?}");
        let low = prepared
            .vertices
            .iter()
            .map(|vertex| vertex[2])
            .fold(f64::INFINITY, f64::min);
        assert!(low.abs() < 1e-6);
    }

    #[test]
    fn a_part_within_a_grid_step_of_the_plate_rests_on_it() {
        // At a 3 mm facet the grid step is 5 mm, more than the 3 mm gap.
        let mesh = joined(&[
            cuboid([0.0; 3], [60.0, 60.0, 30.0]),
            cuboid([80.0, 0.0, 3.0], [12.0; 3]),
        ]);
        let (_, coarse) = prepare(mesh.clone(), &request(3.0)).unwrap();
        assert!(coarse.floating_parts.is_none(), "{coarse:?}");
        let (_, fine) = prepare(mesh, &request(1.0)).unwrap();
        assert_eq!(fine.floating_parts.unwrap().count, 1, "{fine:?}");
    }

    #[test]
    fn a_fragment_below_the_largest_part_sets_the_plate_unless_floating_parts_go() {
        let mesh = joined(&[
            cuboid([0.0, 0.0, 5.0], [20.0; 3]),
            cuboid([30.0, 0.0, 0.0], [2.0; 3]),
        ]);
        // Kept, the fragment is where the STL sits, so the largest part is
        // the one floating above the plate.
        let (_, kept) = prepare(mesh.clone(), &request(0.5)).unwrap();
        let floating = kept.floating_parts.unwrap();
        assert_eq!(
            (floating.count, floating.removed, kept.shells),
            (1, false, 2)
        );
        assert!(floating.volume_mm3 > 7000.0, "{floating:?}");
        // The report also names what removal would delete: the fragment,
        // not the largest part it judges as floating.
        let would = kept.would_remove.unwrap();
        assert_eq!((would.count, would.removed), (1, false));
        assert!(would.volume_mm3 < 20.0, "{would:?}");
        // Removing floating parts puts the plate under the largest part and
        // removes the fragment below it.
        let mut grounded = request(0.5);
        grounded.parts = KeptParts::Grounded;
        let (prepared, report) = prepare(mesh, &grounded).unwrap();
        let removed = report.floating_parts.unwrap();
        assert_eq!(
            (removed.count, removed.removed, report.shells),
            (1, true, 1)
        );
        assert!((report.volume_mm3 - 8000.0).abs() < 200.0, "{report:?}");
        let low = prepared
            .vertices
            .iter()
            .map(|vertex| vertex[2])
            .fold(f64::INFINITY, f64::min);
        assert!(low.abs() < 1e-6);
    }

    #[test]
    fn a_triangle_target_sets_the_edge_length() {
        let mut target = request(1.0);
        target.edge_length_mm = None;
        target.target_triangles = Some(50_000);
        let (_, report) = prepare(cuboid([0.0; 3], [40.0; 3]), &target).unwrap();
        // Close to the target, though the rebuild's triangles are not all
        // equilateral.
        assert!((25_000..=100_000).contains(&report.triangles), "{report:?}");
    }

    #[test]
    fn bodies_touching_at_a_corner_are_oriented_on_their_own() {
        // A small inward-facing cube touches a larger one at a corner.
        let (small, small_triangles) = cuboid([10.0; 3], [4.0; 3]);
        let inward: Vec<[u32; 3]> = small_triangles
            .iter()
            .map(|[a, b, c]| [*a, *c, *b])
            .collect();
        let (mut vertices, mut triangles) = cuboid([0.0; 3], [10.0; 3]);
        // Share the corner vertex, as a welded STL does.
        let offset = vertices.len() as u32;
        vertices.extend(small);
        let corner = 7;
        triangles.extend(inward.iter().map(|triangle| {
            triangle.map(|index| if index == 0 { corner } else { index + offset })
        }));
        let (_, report) = prepare((vertices, triangles), &request(0.25)).unwrap();
        assert_eq!(report.source.shells, 2);
        assert_eq!(report.cavities_filled, 1);
        // Both survive, facing outwards.
        assert!((report.volume_mm3 - 1064.0).abs() < 30.0, "{report:?}");
    }

    /// Two boxes welded at the vertices they share, the second facing
    /// inwards.
    fn welded(second: Soup, shared: &[(u32, u32)]) -> Soup {
        let (mut vertices, mut triangles) = cuboid([0.0; 3], [10.0; 3]);
        let offset = vertices.len() as u32;
        let inward: Vec<[u32; 3]> = second.1.iter().map(|[a, b, c]| [*a, *c, *b]).collect();
        vertices.extend(second.0);
        triangles.extend(inward.iter().map(|triangle| {
            triangle.map(|index| {
                shared
                    .iter()
                    .find(|(own, _)| *own == index)
                    .map_or(index + offset, |(_, theirs)| *theirs)
            })
        }));
        (vertices, triangles)
    }

    #[test]
    fn bodies_sharing_a_welded_edge_are_oriented_on_their_own() {
        // A tall inward-facing block shares a vertical edge of a larger one,
        // so four faces meet at that edge.
        let beside = welded(
            cuboid([10.0, 10.0, 0.0], [4.0, 4.0, 10.0]),
            &[(0, 3), (4, 7)],
        );
        let (_, report) = prepare(beside, &request(0.25)).unwrap();
        assert_eq!((report.source.shells, report.cavities_filled), (2, 1));
        assert!((report.volume_mm3 - 1160.0).abs() < 40.0, "{report:?}");
        // An inward 10 x 8 x 8 box inside an outward 10 mm box at the same
        // origin, welded along their shared edge: turned outwards on its own,
        // so the overlap stays solid.
        let inside = welded(cuboid([0.0; 3], [10.0, 8.0, 8.0]), &[(0, 0), (1, 1)]);
        let (_, report) = prepare(inside, &request(0.25)).unwrap();
        assert_eq!((report.source.shells, report.cavities_filled), (2, 1));
        assert!((report.volume_mm3 - 1000.0).abs() < 30.0, "{report:?}");
    }

    #[test]
    fn a_triangle_target_counts_only_the_largest_part_when_only_it_is_kept() {
        // The second block has as much surface as the first; only the
        // first, which encloses more, is kept.
        let mut target = request(1.0);
        target.edge_length_mm = None;
        target.target_triangles = Some(50_000);
        target.parts = KeptParts::Largest;
        let mesh = joined(&[
            cuboid([0.0; 3], [40.0; 3]),
            cuboid([60.0, 0.0, 0.0], [80.0, 80.0, 0.5]),
        ]);
        let (_, report) = prepare(mesh, &target).unwrap();
        assert_eq!(report.shells, 1);
        assert!((25_000..=100_000).contains(&report.triangles), "{report:?}");
    }

    #[test]
    fn boxes_stacked_on_a_shared_welded_face_become_one_closed_solid() {
        // Two outward boxes, the upper standing on the lower's top face,
        // with that face's triangles written once for each box. The face's
        // edges are used by four triangles, so it is no closed piece and
        // keeps its facing.
        let (lower, lower_triangles) = cuboid([0.0, 0.0, 2.0], [10.0; 3]);
        let (upper, upper_triangles) = cuboid([0.0, 0.0, 12.0], [10.0; 3]);
        let mut vertices = lower;
        let mut triangles = lower_triangles;
        let offset = vertices.len() as u32;
        vertices.extend(upper);
        // The upper box's bottom corners 0 to 3 are the lower box's 4 to 7.
        triangles.extend(upper_triangles.iter().map(|triangle| {
            triangle.map(|index| if index < 4 { index + 4 } else { index + offset })
        }));
        let (prepared, report) = prepare((vertices, triangles), &request(0.25)).unwrap();
        assert_eq!(report.cavities_filled, 0);
        assert_eq!(report.shells, 1);
        assert!((report.volume_mm3 - 2000.0).abs() < 40.0, "{report:?}");
        assert!((report.size_mm[2] - 20.0).abs() < 0.5, "{report:?}");
        let analysed = crate::analyze_mesh(&prepared, crate::ValidationOptions::default()).unwrap();
        assert!(analysed.solid_geometry, "{:?}", analysed.issues);
    }

    #[test]
    fn a_long_thin_fragment_does_not_set_the_facet_size() {
        // A thin diagonal rod has the larger bounds but little volume; the
        // cube, which is kept, sets the facet size from its own area.
        let mut target = request(1.0);
        target.edge_length_mm = None;
        target.target_triangles = Some(50_000);
        target.parts = KeptParts::Largest;
        let (cube, cube_triangles) = cuboid([0.0; 3], [40.0; 3]);
        let rod: Vec<[f64; 3]> = cuboid([0.0; 3], [1.0; 3])
            .0
            .iter()
            .map(|[x, y, z]| [60.0 + 80.0 * x + y, 80.0 * x + z, 80.0 * x])
            .collect();
        let mesh = joined(&[(cube, cube_triangles.clone()), (rod, cube_triangles)]);
        let (_, report) = prepare(mesh, &target).unwrap();
        assert_eq!(report.shells, 1);
        assert!((25_000..=100_000).contains(&report.triangles), "{report:?}");
    }

    #[test]
    fn the_default_facet_size_follows_the_kept_part_and_its_final_size() {
        // Distant debris makes the source 200 mm long; the kept block is
        // 20 mm, rescaled to 40 mm.
        let mesh = joined(&[
            cuboid([0.0; 3], [20.0; 3]),
            cuboid([198.0, 0.0, 0.0], [2.0; 3]),
        ]);
        let mut sized = request(1.0);
        sized.edge_length_mm = None;
        sized.parts = KeptParts::Largest;
        sized.longest_side_mm = Some(40.0);
        let (_, report) = prepare(mesh, &sized).unwrap();
        // 0.4% of the block's 20 mm is 0.1 mm, doubled by the rescale.
        assert!((report.edge_length_mm - 0.2).abs() < 1e-9, "{report:?}");
        assert!((report.size_mm[0] - 40.0).abs() < 1e-6, "{report:?}");
    }

    #[test]
    fn separate_parts_on_the_plate_are_reported_not_removed() {
        let mesh = joined(&[
            cuboid([0.0; 3], [10.0; 3]),
            cuboid([20.0, 0.0, 0.0], [10.0; 3]),
        ]);
        let mut grounded = request(0.5);
        grounded.parts = KeptParts::Grounded;
        let (_, report) = prepare(mesh, &grounded).unwrap();
        assert_eq!(report.shells, 2);
        assert!(report.floating_parts.is_none());
    }

    #[test]
    fn openings_and_inconsistent_facing_are_repaired_before_the_rebuild() {
        let (vertices, mut triangles) = cuboid([0.0; 3], [10.0; 3]);
        // Open the top and turn one side face around.
        triangles.retain(|triangle| *triangle != [4, 5, 6] && *triangle != [5, 7, 6]);
        triangles[2].swap(1, 2);
        let (_, report) = prepare((vertices, triangles), &request(0.5)).unwrap();
        assert_eq!(report.source.open_edges, 4);
        assert!(report.source.volume_mm3.is_none());
        assert_eq!(report.flipped_triangles, 1);
        assert_eq!((report.openings_closed, report.openings_left), (1, 0));
        assert_eq!(report.shells, 1);
        assert!((report.volume_mm3 - 1000.0).abs() < 30.0, "{report:?}");
    }

    #[test]
    fn inward_pieces_face_outwards_by_default_and_stay_cavities_when_kept() {
        let (vertices, triangles) = cuboid([0.0; 3], [10.0; 3]);
        let inverted: Vec<[u32; 3]> = triangles.iter().map(|[a, b, c]| [*a, *c, *b]).collect();
        let inner: Vec<[f64; 3]> = vertices.iter().map(|v| v.map(|x| 3.0 + 0.4 * x)).collect();
        // An inside-out block is turned to face outwards.
        let (_, report) = prepare((vertices.clone(), inverted.clone()), &request(0.5)).unwrap();
        assert_eq!(report.cavities_filled, 1);
        assert!((report.volume_mm3 - 1000.0).abs() < 30.0, "{report:?}");
        // A hollow block with its cavity facing inwards is filled by default,
        // so no material is lost, and keeps its cavity when asked.
        let hollow = joined(&[
            (vertices.clone(), triangles.clone()),
            (inner.clone(), inverted.clone()),
        ]);
        let (_, filled) = prepare(hollow.clone(), &request(0.5)).unwrap();
        assert_eq!((filled.cavities_filled, filled.cavities), (1, 0));
        assert!((filled.volume_mm3 - 1000.0).abs() < 30.0, "{filled:?}");
        let mut keep = request(0.5);
        keep.keep_cavities = true;
        let (_, kept) = prepare(hollow, &keep).unwrap();
        assert_eq!((kept.cavities_filled, kept.cavities), (0, 1));
        assert!((kept.volume_mm3 - 936.0).abs() < 30.0, "{kept:?}");
        // A hollow block written wholly inside out is filled by default.
        let (_, inverted_hollow) = prepare(
            joined(&[(vertices, inverted), (inner, triangles)]),
            &request(0.5),
        )
        .unwrap();
        assert_eq!(inverted_hollow.cavities_filled, 1);
        assert!(
            (inverted_hollow.volume_mm3 - 1000.0).abs() < 30.0,
            "{inverted_hollow:?}"
        );
    }

    #[test]
    fn a_requested_longest_side_rescales_the_result_after_parts_go() {
        // The speck made the source 50 mm long; the block alone is 20 mm.
        let mesh = joined(&[
            cuboid([0.0; 3], [20.0, 10.0, 10.0]),
            cuboid([48.0, 0.0, 20.0], [2.0; 3]),
        ]);
        let mut sized = request(0.25);
        sized.parts = KeptParts::Largest;
        sized.longest_side_mm = Some(50.0);
        let (prepared, report) = prepare(mesh, &sized).unwrap();
        assert!((report.size_mm[0] - 50.0).abs() < 1e-6, "{report:?}");
        assert!((report.size_mm[1] - 25.0).abs() < 0.5, "{report:?}");
        let low = prepared
            .vertices
            .iter()
            .map(|v| v[2])
            .fold(f64::INFINITY, f64::min);
        let (west, east) = prepared
            .vertices
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(w, e), v| {
                (w.min(v[0]), e.max(v[0]))
            });
        assert!(low.abs() < 1e-6 && (west + east).abs() < 1e-4);
    }

    #[test]
    fn walls_thinner_than_the_nozzle_are_reported() {
        // A 20 mm block with a 0.3 mm fin standing on it.
        let mesh = joined(&[
            cuboid([0.0; 3], [20.0, 20.0, 5.0]),
            cuboid([5.0, 9.85, 4.0], [10.0, 0.3, 10.0]),
        ]);
        let mut fin = request(0.1);
        fin.checks = Some(PrintChecks {
            overhang_degrees: 45.0,
            min_feature_mm: Some(0.4),
        });
        let (_, report) = prepare(mesh, &fin).unwrap();
        let thin = report.thin.unwrap();
        // Both faces of the fin, 10 mm by about 9 mm, are thin; the block is not.
        assert!(thin.area_mm2 > 120.0 && thin.area_mm2 < 250.0, "{thin:?}");
        assert!(thin.regions.iter().all(|region| region.centre_mm[2] > 4.5));
        let overhang = report.overhang.unwrap();
        assert!(overhang.area_mm2 < 1.0, "{overhang:?}");
    }

    #[test]
    fn hollowing_leaves_a_drained_cavity_inside_a_plinth() {
        let mesh = cuboid([0.0; 3], [30.0, 30.0, 30.0]);
        let mut hollow = request(0.5);
        hollow.plinth = Some(Plinth {
            height_mm: 4.0,
            diameter_mm: Some(50.0),
            chamfer_mm: 0.5,
        });
        hollow.hollow = Some(Hollow {
            wall_mm: 3.0,
            drain_diameter_mm: 3.5,
        });
        let (prepared, report) = prepare(mesh, &hollow).unwrap();
        let cavity = report.hollow.as_ref().unwrap();
        assert!(!cavity.drain_holes_mm.is_empty());
        // The block, less a 3 mm wall, is 24 mm across, though its base was
        // trimmed flat by one grid step.
        assert!(cavity.cavity_volume_mm3 > 0.8 * 24f64.powi(3), "{cavity:?}");
        assert_eq!(report.shells, 1);
        assert_eq!(report.plinth.unwrap().diameter_mm, 50.0);
        assert!((report.size_mm[2] - (30.0 - 0.5 / remesh::EDGE_PER_SPACING + 4.0)).abs() < 0.6);
        let analysed = crate::analyze_mesh(&prepared, crate::ValidationOptions::default()).unwrap();
        assert!(analysed.solid_geometry, "{:?}", analysed.issues);
    }

    #[test]
    fn parts_within_the_base_cut_are_counted() {
        // A 0.5 mm tab beside a block, both on the plate: a cut 1 mm up
        // removes the tab whole.
        let solid = Manifold::compose(&[
            Manifold::cube(20.0, 20.0, 20.0, false),
            Manifold::cube(4.0, 4.0, 0.5, false).translate(0.0, 25.0, 0.0),
        ]);
        assert_eq!(parts_below(&solid, 1.0), 1);
        assert_eq!(parts_below(&solid, 0.25), 0);
    }

    #[test]
    fn hollowing_refuses_several_parts() {
        let mesh = joined(&[
            cuboid([0.0; 3], [20.0; 3]),
            cuboid([40.0, 0.0, 0.0], [20.0; 3]),
        ]);
        let mut hollow = request(1.0);
        hollow.hollow = Some(Hollow {
            wall_mm: 3.0,
            drain_diameter_mm: 3.5,
        });
        assert!(matches!(
            prepare(mesh, &hollow),
            Err(GeometryError::InvalidOptions(_))
        ));
    }
}
