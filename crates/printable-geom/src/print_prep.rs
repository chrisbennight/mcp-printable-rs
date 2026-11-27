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
use parry3d_f64::math::Vector;
use serde::{Deserialize, Serialize};

use crate::bust::{add_plinth, hollow_out, mesh_from, single_precision};
use crate::print_check::{self, PrintChecks, SurfaceFinding, root};
use crate::{GeometryError, Hollow, HollowReport, Plinth, PlinthReport, TriangleMesh, remesh};

/// A part whose lowest point in the source lies farther than this from the
/// build plate, about one printed layer, does not rest on it. The same band
/// above the plate counts as resting on it in the overhang check, since the
/// first layer covers it.
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
    /// Keep parts that do not rest on the build plate.
    #[serde(default)]
    pub keep_floating_parts: bool,
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

/// The mesh as it was given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceSummary {
    pub triangles: usize,
    /// Pieces connected through shared vertices.
    pub shells: usize,
    /// Edges used by only one triangle: the rims of holes.
    pub open_edges: usize,
    /// Absent when the surface has openings, since it then encloses no
    /// definite volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_mm3: Option<f64>,
}

/// Parts of the rebuilt solid that do not rest on the build plate, which
/// lies under the largest part: those above it, and fragments below it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FloatingParts {
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
        }
    }
}

/// What the preparation found and changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrintPrepReport {
    pub source: SourceSummary,
    /// Triangles turned to face the same way as most of their piece.
    pub flipped_triangles: usize,
    /// Closed pieces turned around: those that faced inwards with nothing
    /// around them, and the pieces nested inside those.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub shells_turned: usize,
    /// Openings closed before the rebuild.
    pub openings_closed: usize,
    /// Openings whose rim crosses itself or meets another, left for the
    /// rebuild to resolve.
    pub openings_left: usize,
    pub edge_length_mm: f64,
    pub triangles: usize,
    /// Separate solids in the result, each printable on its own.
    pub shells: usize,
    pub volume_mm3: f64,
    /// Extent along x, y and z; the solid rests on z = 0.
    pub size_mm: [f64; 3],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating_parts: Option<FloatingParts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plinth: Option<PlinthReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hollow: Option<HollowReport>,
    /// Surface facing down more steeply than the checked angle, measured
    /// before any plinth was added.
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
    let edge = request.edge_length_mm.unwrap_or_else(|| {
        (DEFAULT_EDGE_SHARE * print_check::extent(&vertices)).max(MIN_DEFAULT_EDGE_MM)
    });
    if !(edge.is_finite() && edge > 0.0) {
        return Err(GeometryError::InvalidOptions(
            "edge length must be a positive number of millimetres".into(),
        ));
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
    let pieces = pieces(vertices.len(), &triangles);
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
    let shells_turned = turn_inside_out_shells(&vertices, &mut triangles);
    // Where each source piece reaches lowest. Floating parts are judged from
    // the source, since the rebuild moves surfaces by up to a grid step.
    let bottoms = piece_bottoms(&vertices, &triangles);

    let solid = remesh::remesh_union(vertices, triangles, edge)?;
    let spacing = edge / remesh::EDGE_PER_SPACING;
    let parts = solids(&solid);
    // A rebuilt part reaches as low as the lowest source piece within it.
    let base_of = |part: &Manifold| {
        let Some(bounds) = part.bounding_box() else {
            return f64::INFINITY;
        };
        let (low, high) = (bounds.min(), bounds.max());
        bottoms
            .iter()
            .filter(|point| {
                (0..3).all(|axis| {
                    low[axis] - spacing <= point[axis] && point[axis] <= high[axis] + spacing
                })
            })
            .map(|point| point[2])
            .fold(low[2], f64::min)
    };
    // The build plate is under the largest part, so that a small fragment
    // lower down cannot take its place.
    let plate = parts
        .iter()
        .max_by(|a, b| a.volume().total_cmp(&b.volume()))
        .map_or(0.0, base_of);
    let floating: Vec<Manifold> = parts
        .into_iter()
        .filter(|part| (base_of(part) - plate).abs() > PLATE_TOLERANCE_MM)
        .collect();
    let floating_parts = (!floating.is_empty()).then(|| FloatingParts {
        count: floating.len(),
        volume_mm3: floating.iter().map(Manifold::volume).sum(),
        removed: !request.keep_floating_parts,
    });
    // Subtracting the floating parts keeps any cavity inside the others.
    let solid = if request.keep_floating_parts || floating.is_empty() {
        solid
    } else {
        Manifold::batch_difference(&[vec![solid], floating].concat())
    };
    let bounds = solid
        .bounding_box()
        .ok_or_else(|| GeometryError::Kernel("the prepared solid is empty".into()))?;
    let (low, high) = (bounds.min(), bounds.max());
    let solid = match request.longest_side_mm {
        Some(longest) => {
            let scale = longest / bounds.dimensions().into_iter().fold(0.0, f64::max);
            solid
                .translate(
                    -(low[0] + high[0]) / 2.0,
                    -(low[1] + high[1]) / 2.0,
                    -low[2],
                )
                .scale(scale, scale, scale)
        }
        None => solid.translate(0.0, 0.0, -low[2]),
    };

    // The overhang check sees the solid before a plinth, whose chamfer is
    // meant to slope.
    let surface = mesh_from(&solid);
    let mut overhang = request.checks.map(|checks| {
        print_check::overhang_above(&surface, checks.overhang_degrees, &[], PLATE_TOLERANCE_MM)
    });
    drop(surface);
    let (solid, plinth) = match request.plinth {
        Some(plinth) => {
            // A flat base one grid step above the lowest point, so the solid
            // meets the plinth over a whole section rather than at points.
            // Cutting at exactly zero puts the base exactly on the plinth.
            let base = solid
                .translate(0.0, 0.0, -spacing)
                .trim_by_plane([0.0, 0.0, 1.0], 0.0);
            drop(solid);
            let (solid, report) = add_plinth(base, plinth)?;
            (solid, Some(report))
        }
        None => (solid, None),
    };
    let raise = plinth.map_or(0.0, |plinth| plinth.height_mm);
    for region in overhang.iter_mut().flat_map(|found| &mut found.regions) {
        region.centre_mm[2] += raise;
    }
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
    let thin = match request.checks.and_then(|checks| checks.min_feature_mm) {
        Some(min_mm) => Some(print_check::thin(&mesh, min_mm, raise, &[])?.into()),
        None => None,
    };
    let report = PrintPrepReport {
        source,
        flipped_triangles,
        shells_turned,
        openings_closed,
        openings_left,
        edge_length_mm: edge,
        triangles: mesh.triangles.len(),
        shells,
        volume_mm3,
        size_mm,
        floating_parts,
        plinth,
        hollow,
        overhang: overhang.map(Finding::from),
        thin,
    };
    Ok((mesh, report))
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

/// The lowest vertex of each connected piece.
fn piece_bottoms(vertices: &[[f64; 3]], triangles: &[[u32; 3]]) -> Vec<[f64; 3]> {
    let pieces = pieces(vertices.len(), triangles);
    let mut bottoms = vec![[0.0, 0.0, f64::INFINITY]; pieces.count];
    for (face, triangle) in triangles.iter().enumerate() {
        let bottom = &mut bottoms[pieces.of_triangle[face] as usize];
        for &index in triangle {
            if vertices[index as usize][2] < bottom[2] {
                *bottom = vertices[index as usize];
            }
        }
    }
    bottoms
}

/// Connected pieces of a mesh, joined through shared vertices.
struct Pieces {
    /// The piece of each triangle, numbered from zero.
    of_triangle: Vec<u32>,
    count: usize,
}

fn pieces(vertices: usize, triangles: &[[u32; 3]]) -> Pieces {
    let mut parent: Vec<usize> = (0..vertices).collect();
    for [a, b, c] in triangles {
        for (p, q) in [(*a, *b), (*b, *c)] {
            let (p, q) = (root(&mut parent, p as usize), root(&mut parent, q as usize));
            parent[p] = q;
        }
    }
    let mut numbers = HashMap::new();
    let of_triangle = triangles
        .iter()
        .map(|triangle| {
            let next = numbers.len() as u32;
            *numbers
                .entry(root(&mut parent, triangle[0] as usize))
                .or_insert(next)
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

/// Turn around each closed piece that faces inwards, as some exporters
/// write a whole part, together with everything nested inside it, so that a
/// cavity inside it stays a cavity. A piece inside another keeps its facing
/// relative to the piece around it. Returns how many were turned.
fn turn_inside_out_shells(vertices: &[[f64; 3]], triangles: &mut [[u32; 3]]) -> usize {
    let pieces = pieces(vertices.len(), triangles);
    let mut volume = vec![0.0; pieces.count];
    let mut bounds = vec![([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]); pieces.count];
    let mut members: Vec<Vec<[u32; 3]>> = vec![Vec::new(); pieces.count];
    for (face, triangle) in triangles.iter().enumerate() {
        let piece = pieces.of_triangle[face] as usize;
        volume[piece] += signed_volume(vertices, std::slice::from_ref(triangle));
        members[piece].push(*triangle);
        for &index in triangle {
            let (low, high) = &mut bounds[piece];
            for (axis, value) in vertices[index as usize].into_iter().enumerate() {
                low[axis] = low[axis].min(value);
                high[axis] = high[axis].max(value);
            }
        }
    }
    let size = |piece: usize| {
        (0..3)
            .map(|axis| bounds[piece].1[axis] - bounds[piece].0[axis])
            .product::<f64>()
    };
    let within = |inner: usize, outer: usize| {
        inner != outer
            && (0..3).all(|axis| {
                bounds[outer].0[axis] <= bounds[inner].0[axis]
                    && bounds[inner].1[axis] <= bounds[outer].1[axis]
            })
    };
    // The piece directly around each piece: the smallest that contains every
    // one of its vertices, by the winding count of a ray from each, so that a
    // piece reaching out through the surface of another is not nested in it.
    let points: Vec<Vector> = vertices
        .iter()
        .map(|vertex| Vector::from_array(*vertex))
        .collect();
    let mut columns: HashMap<usize, remesh::Columns> = HashMap::new();
    let mut parent: Vec<Option<usize>> = vec![None; pieces.count];
    for inner in 0..pieces.count {
        let mut probes: Vec<u32> = members[inner].iter().flatten().copied().collect();
        probes.sort_unstable();
        probes.dedup();
        for outer in (0..pieces.count).filter(|&outer| within(inner, outer)) {
            let (low, high) = bounds[outer];
            let cell = (0..2)
                .map(|axis| high[axis] - low[axis])
                .fold(0.0, f64::max)
                / (members[outer].len() as f64).sqrt().max(1.0);
            let Some(grid) = (match columns.entry(outer) {
                std::collections::hash_map::Entry::Occupied(found) => Some(found.into_mut()),
                std::collections::hash_map::Entry::Vacant(slot) => remesh::Columns::new(
                    &points,
                    &members[outer],
                    low,
                    high,
                    cell.max(f64::MIN_POSITIVE),
                    u32::MAX as usize,
                )
                .map(|grid| slot.insert(grid)),
            }) else {
                continue;
            };
            let inside = |&index: &u32| {
                let probe = vertices[index as usize];
                let crossing = grid.candidates(probe).iter().map(|&face| {
                    members[outer][face as usize].map(|index| vertices[index as usize])
                });
                remesh::winding(probe, crossing) != 0
            };
            if probes.iter().all(inside)
                && parent[inner].is_none_or(|current| size(outer) < size(current))
            {
                parent[inner] = Some(outer);
            }
        }
    }
    drop(columns);
    // A piece with nothing around it is turned when it faces inwards; a
    // nested piece is turned with the piece around it.
    let mut turned: Vec<Option<bool>> = vec![None; pieces.count];
    fn decide(
        piece: usize,
        parent: &[Option<usize>],
        volume: &[f64],
        turned: &mut [Option<bool>],
        depth: usize,
    ) -> bool {
        if let Some(known) = turned[piece] {
            return known;
        }
        let known = match parent[piece] {
            // A containment cycle can only come from coincident pieces.
            Some(outer) if depth < parent.len() => decide(outer, parent, volume, turned, depth + 1),
            _ => volume[piece] < 0.0,
        };
        turned[piece] = Some(known);
        known
    }
    for piece in 0..pieces.count {
        decide(piece, &parent, &volume, &mut turned, 0);
    }
    for (face, triangle) in triangles.iter_mut().enumerate() {
        if turned[pieces.of_triangle[face] as usize] == Some(true) {
            triangle.swap(1, 2);
        }
    }
    turned
        .iter()
        .filter(|turned| **turned == Some(true))
        .count()
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
            keep_floating_parts: false,
            plinth: None,
            hollow: None,
            checks: None,
            longest_side_mm: None,
        }
    }

    #[test]
    fn overlapping_shells_merge_and_floating_parts_are_removed() {
        // Two overlapping cubes on the plate, and a small cube in the air.
        let mesh = joined(&[
            cuboid([0.0; 3], [10.0; 3]),
            cuboid([5.0, 5.0, 0.0], [10.0; 3]),
            cuboid([30.0, 0.0, 20.0], [4.0; 3]),
        ]);
        let (prepared, report) = prepare(mesh.clone(), &request(0.5)).unwrap();
        assert_eq!(report.source.shells, 3);
        assert_eq!(report.source.open_edges, 0);
        assert!((report.source.volume_mm3.unwrap() - 2064.0).abs() < 1e-6);
        assert_eq!(report.shells, 1);
        let floating = report.floating_parts.unwrap();
        assert_eq!((floating.count, floating.removed), (1, true));
        assert!((floating.volume_mm3 - 64.0).abs() < 8.0, "{floating:?}");
        // Two 1000 mm^3 cubes sharing 250 mm^3.
        assert!((report.volume_mm3 - 1750.0).abs() < 50.0, "{report:?}");
        assert!(report.size_mm[2] < 10.5, "{report:?}");
        let low = prepared
            .vertices
            .iter()
            .map(|vertex| vertex[2])
            .fold(f64::INFINITY, f64::min);
        assert!(low.abs() < 1e-6);

        let mut keep = request(0.5);
        keep.keep_floating_parts = true;
        let (_, kept) = prepare(mesh, &keep).unwrap();
        assert_eq!(kept.shells, 2);
        assert!(!kept.floating_parts.unwrap().removed);
    }

    #[test]
    fn a_small_fragment_below_the_subject_does_not_become_the_plate() {
        let mesh = joined(&[
            cuboid([0.0, 0.0, 5.0], [20.0; 3]),
            cuboid([30.0, 0.0, 0.0], [2.0; 3]),
        ]);
        let (prepared, report) = prepare(mesh, &request(0.5)).unwrap();
        let floating = report.floating_parts.unwrap();
        assert_eq!((floating.count, floating.removed), (1, true));
        assert!(floating.volume_mm3 < 20.0, "{floating:?}");
        assert!((report.volume_mm3 - 8000.0).abs() < 200.0, "{report:?}");
        let low = prepared
            .vertices
            .iter()
            .map(|vertex| vertex[2])
            .fold(f64::INFINITY, f64::min);
        assert!(low.abs() < 1e-6);
    }

    #[test]
    fn separate_parts_on_the_plate_are_reported_not_removed() {
        let mesh = joined(&[
            cuboid([0.0; 3], [10.0; 3]),
            cuboid([20.0, 0.0, 0.0], [10.0; 3]),
        ]);
        let (_, report) = prepare(mesh, &request(0.5)).unwrap();
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
    fn a_shell_facing_inwards_is_turned_around_but_a_cavity_is_kept() {
        let (vertices, triangles) = cuboid([0.0; 3], [10.0; 3]);
        let inverted: Vec<[u32; 3]> = triangles.iter().map(|[a, b, c]| [*a, *c, *b]).collect();
        let (_, report) = prepare((vertices.clone(), inverted.clone()), &request(0.5)).unwrap();
        assert_eq!(report.shells_turned, 1);
        // Beside a block facing outwards, only the inward one is turned.
        let beside = joined(&[
            (vertices.clone(), inverted.clone()),
            cuboid([20.0, 0.0, 0.0], [10.0; 3]),
        ]);
        let (_, report) = prepare(beside, &request(0.5)).unwrap();
        assert_eq!((report.shells_turned, report.shells), (1, 2));
        assert!((report.volume_mm3 - 2000.0).abs() < 60.0, "{report:?}");
        // A 4 mm cavity facing inwards inside the block stays empty.
        let cavity: Soup = (
            vertices.iter().map(|v| v.map(|x| 3.0 + 0.4 * x)).collect(),
            inverted,
        );
        let (_, report) = prepare(joined(&[(vertices, triangles), cavity]), &request(0.5)).unwrap();
        assert_eq!(report.shells_turned, 0);
        assert!((report.volume_mm3 - 936.0).abs() < 30.0, "{report:?}");
    }

    #[test]
    fn an_inverted_hollow_part_is_turned_around_with_its_cavity() {
        let (vertices, triangles) = cuboid([0.0; 3], [10.0; 3]);
        let inner: Vec<[f64; 3]> = vertices.iter().map(|v| v.map(|x| 3.0 + 0.4 * x)).collect();
        let inward: Vec<[u32; 3]> = triangles.iter().map(|[a, b, c]| [*a, *c, *b]).collect();
        // The whole part faces the wrong way: its outside inwards and its
        // cavity outwards.
        let mesh = joined(&[(vertices, inward), (inner, triangles)]);
        let (_, report) = prepare(mesh, &request(0.5)).unwrap();
        assert_eq!(report.shells_turned, 2);
        assert!((report.volume_mm3 - 936.0).abs() < 30.0, "{report:?}");
    }

    /// An L-shaped prism 10 mm tall: a 10 mm square less the 6 mm square
    /// notch at its far corner.
    fn notched() -> Soup {
        let outline = [
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 4.0],
            [4.0, 4.0],
            [4.0, 10.0],
            [0.0, 10.0],
        ];
        let mut vertices: Vec<[f64; 3]> = outline.iter().map(|[x, y]| [*x, *y, 0.0]).collect();
        vertices.extend(outline.iter().map(|[x, y]| [*x, *y, 10.0]));
        let mut triangles = Vec::new();
        for k in 1..5u32 {
            triangles.push([0, k + 1, k]);
            triangles.push([6, 6 + k, 7 + k]);
        }
        for k in 0..6u32 {
            let next = (k + 1) % 6;
            triangles.push([k, next, next + 6]);
            triangles.push([k, next + 6, k + 6]);
        }
        (vertices, triangles)
    }

    #[test]
    fn a_piece_reaching_out_of_another_is_not_nested_in_it() {
        // An inward-facing cube lies partly in the L's material and partly in
        // its notch, within the L's bounds. It is not a cavity of the L, so
        // it is turned around on its own and joins it.
        let (cube, cube_triangles) = cuboid([2.0, 2.0, 2.0], [4.0, 4.0, 3.0]);
        let inward: Vec<[u32; 3]> = cube_triangles
            .iter()
            .map(|[a, b, c]| [*a, *c, *b])
            .collect();
        let (_, alone) = prepare(notched(), &request(0.25)).unwrap();
        assert!((alone.volume_mm3 - 640.0).abs() < 15.0, "{alone:?}");
        let (_, report) = prepare(joined(&[notched(), (cube, inward)]), &request(0.25)).unwrap();
        assert_eq!((report.shells_turned, report.shells), (1, 1));
        // The L's 640 mm^3 and the 12 mm^3 of cube in the notch.
        assert!((report.volume_mm3 - 652.0).abs() < 15.0, "{report:?}");
    }

    #[test]
    fn a_part_a_few_millimetres_above_the_plate_floats_at_a_coarse_facet_size() {
        // At a 3 mm facet the grid step is 5 mm, more than the 3 mm gap.
        let mesh = joined(&[
            cuboid([0.0; 3], [60.0, 60.0, 30.0]),
            cuboid([80.0, 0.0, 3.0], [12.0; 3]),
        ]);
        let (_, report) = prepare(mesh, &request(3.0)).unwrap();
        assert_eq!(report.floating_parts.unwrap().count, 1, "{report:?}");
        assert_eq!(report.shells, 1);
    }

    #[test]
    fn a_requested_longest_side_rescales_the_result_after_floating_parts_go() {
        // The speck made the source 50 mm long; the block alone is 20 mm.
        let mesh = joined(&[
            cuboid([0.0; 3], [20.0, 10.0, 10.0]),
            cuboid([48.0, 0.0, 20.0], [2.0; 3]),
        ]);
        let mut sized = request(0.25);
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
