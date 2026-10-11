//! Printable busts from an evaluated head: one open head surface plus closed
//! attached parts such as eyeballs.
//!
//! Each boundary loop is closed with a fan of triangles, so every part becomes
//! a closed solid. The parts are rotated to stand upright, scaled to the
//! requested height, smoothed and refined to the requested edge length, united,
//! and finally trimmed by a horizontal plane just above the lowest opening to
//! give a flat base.

use std::collections::{HashMap, HashSet};

use manifold_csg::Manifold;
use serde::{Deserialize, Serialize};

use crate::{GeometryError, TriangleMesh, remesh};

/// Smoothing treats every edge as smooth. The kernel emits zero-area
/// triangles when smoothing preserves sharp edges, so parts are smoothed
/// before the flat base exists and the base is cut afterwards.
const SMOOTH_EVERY_EDGE_DEGREES: f64 = 180.0;
/// Faceting is estimated where facets meet at less than this angle; sharper
/// creases are intended features such as the base rim.
const SMOOTH_REGION_RADIANS: f64 = std::f64::consts::PI / 6.0;
/// Fraction of the way toward the centre of its neighbours that each vertex
/// at a touching point moves, which is a few hundredths of a millimetre.
const PINCH_SEPARATION: f64 = 0.25;
/// The trim plane sits this fraction of the head's height above the top of the
/// lowest opening, so the cut is a full section through the neck.
const NECK_MARGIN: f64 = 0.01;
/// Separate shells smaller than this fraction of the bust's volume are
/// discarded. An eyeball on a bust is roughly a thousandth of its volume.
const FRAGMENT_VOLUME_FRACTION: f64 = 1e-5;
/// Refinement to an edge length L yields about this many triangles per unit of
/// surface area divided by L squared.
const TRIANGLES_PER_AREA: f64 = 2.6;

/// Inputs to [`prepare_bust`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BustRequest {
    /// Head vertices in millimetres, with y up and the face looking along +z.
    pub vertices: Vec<[f64; 3]>,
    pub triangles: Vec<[u32; 3]>,
    /// Height of the finished bust, base to crown.
    pub height_mm: f64,
    /// Target edge length after refinement.
    pub edge_length_mm: f64,
    /// Upper bound on the finished triangle count; the edge length grows to
    /// respect it.
    pub max_triangles: usize,
}

/// Measurements of a prepared bust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BustReport {
    pub triangles: usize,
    /// Extent along x, y and z. The base rests on z = 0 and the face looks
    /// toward -y.
    pub size_mm: [f64; 3],
    pub volume_mm3: f64,
    pub requested_edge_length_mm: f64,
    /// Edge length used, larger than requested only when the triangle limit
    /// required it.
    pub edge_length_mm: f64,
    /// Estimated distance between the facets and the smooth surface they
    /// approximate, at the 99.9th percentile. Each edge contributes L·θ/8,
    /// the gap between a chord of length L and a circular arc turning
    /// through θ, the angle between the edge's two faces. Edges where faces
    /// meet at 30 degrees or more are creases rather than faceting and are
    /// left out. Absent when there is no such edge.
    pub estimated_deviation_p999_mm: Option<f64>,
    /// Whether a lowest opening was found and the head trimmed above it.
    pub trimmed_at_neck: bool,
    /// Separate shells of negligible volume that were discarded.
    pub discarded_fragments: usize,
}

/// Build a closed, printable bust from an open head surface and its parts.
pub fn prepare_bust(request: &BustRequest) -> Result<(TriangleMesh, BustReport), GeometryError> {
    validate(request)?;
    let parts = closed_parts(&request.vertices, &request.triangles)?;
    let heights = request.vertices.iter().map(|vertex| vertex[1]);
    let span = heights.clone().fold(f64::MIN, f64::max) - heights.fold(f64::MAX, f64::min);
    // Closed parts have no openings, so the lowest opening is the neck.
    let cut = parts
        .iter()
        .flat_map(|part| &part.openings)
        .min_by(|a, b| a.mean_height.total_cmp(&b.mean_height))
        .map(|neck| neck.top_height + NECK_MARGIN * span);

    // Rotate from y up with the face toward +z to z up with the face toward -y.
    let upright = |[x, y, z]: [f64; 3]| [x, -z, y];
    let kept_height = |vertex: &[f64; 3]| cut.is_none_or(|height| vertex[1] >= height);
    let mut low = [f64::INFINITY; 3];
    let mut high = [f64::NEG_INFINITY; 3];
    for vertex in parts
        .iter()
        .flat_map(|part| &part.vertices)
        .filter(|vertex| kept_height(vertex))
    {
        let vertex = upright(*vertex);
        for axis in 0..3 {
            low[axis] = low[axis].min(vertex[axis]);
            high[axis] = high[axis].max(vertex[axis]);
        }
    }
    // Vertices are finite, so this also catches the case of none kept.
    if high[2] <= low[2] {
        return Err(GeometryError::Kernel("the trimmed bust is empty".into()));
    }
    // Place and scale before smoothing, so refinement works in final units.
    let scale = request.height_mm / (high[2] - low[2]);
    let centre = [(low[0] + high[0]) / 2.0, (low[1] + high[1]) / 2.0, low[2]];
    let smooth = parts
        .iter()
        .map(|part| {
            let flat: Vec<f64> = part
                .vertices
                .iter()
                .flat_map(|vertex| {
                    let vertex = upright(*vertex);
                    [0, 1, 2].map(|axis| (vertex[axis] - centre[axis]) * scale)
                })
                .collect();
            let indices: Vec<u64> = part
                .triangles
                .iter()
                .flatten()
                .map(|&index| u64::from(index))
                .collect();
            Manifold::from_mesh_f64(&flat, 3, &indices)
                .map(|solid| solid.smooth_out(SMOOTH_EVERY_EDGE_DEGREES, 0.0))
                .map_err(|error| {
                    GeometryError::Kernel(format!(
                        "a closed head part is not a closed surface: {error}"
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let base = cut.map(|_| 0.0);

    let limit = request.max_triangles as f64;
    let area: f64 = smooth.iter().map(Manifold::surface_area).sum();
    let mut edge = request
        .edge_length_mm
        .max((TRIANGLES_PER_AREA * area / limit).sqrt());
    // The smoothed parts overlap one another and can pass through themselves
    // where FLAME's surface folds, at the lips and ears. Remeshing their
    // union yields one valid solid; the flat base is cut afterwards.
    let build = |edge: f64| -> Result<Manifold, GeometryError> {
        let mut vertices = Vec::new();
        let mut triangles = Vec::new();
        for part in &smooth {
            let mesh = mesh_from(&part.refine_to_length(edge));
            let offset =
                u32::try_from(vertices.len()).map_err(|_| GeometryError::TooManyVertices)?;
            vertices.extend(mesh.vertices);
            triangles.extend(
                mesh.triangles
                    .iter()
                    .map(|triangle| triangle.map(|index| index + offset)),
            );
        }
        let solid = remesh::remesh_union(vertices, triangles, edge)?;
        Ok(match base {
            Some(height) => solid.trim_by_plane([0.0, 0.0, 1.0], height),
            None => solid,
        })
    };
    // Large meshes are dropped as soon as they are replaced, so that no more
    // than two copies are alive within the worker's memory limit.
    let mut refined = build(edge)?;
    if refined.num_tri() > request.max_triangles {
        // The area estimate was low; size the edge from the observed count.
        edge *= (refined.num_tri() as f64 / limit).sqrt() * 1.02;
        drop(refined);
        refined = build(edge)?;
    }
    if refined.num_tri() > request.max_triangles {
        return Err(GeometryError::Kernel(
            "the refined bust exceeds its triangle limit".into(),
        ));
    }
    refined.status().map_err(|error| {
        GeometryError::Kernel(format!("the refined bust is not a valid solid: {error}"))
    })?;
    // Thin gaps can leave specks of almost no volume as separate shells. They
    // cannot print, so drop them; anything larger means the parts did not join.
    let pieces = refined.decompose();
    drop(refined);
    let total: f64 = pieces.iter().map(Manifold::volume).sum();
    let count = pieces.len();
    let mut kept: Vec<Manifold> = pieces
        .into_iter()
        .filter(|piece| piece.volume() > FRAGMENT_VOLUME_FRACTION * total)
        .collect();
    if kept.len() != 1 {
        return Err(GeometryError::Kernel(format!(
            "the bust is {} separate solids rather than one",
            kept.len()
        )));
    }
    let fragments = count - 1;
    let piece = kept.pop().expect("one piece is kept");
    // Smoothing moves the surface slightly off the coarse mesh, so restore the
    // exact requested height and recentre.
    let bounds = piece
        .bounding_box()
        .ok_or_else(|| GeometryError::Kernel("the refined bust has no extent".into()))?;
    let (low, high) = (bounds.min(), bounds.max());
    let correction = request.height_mm / (high[2] - low[2]);
    let solid = piece
        .translate(
            -(low[0] + high[0]) / 2.0,
            -(low[1] + high[1]) / 2.0,
            -low[2],
        )
        .scale(correction, correction, correction);
    drop(piece);
    edge *= correction;
    let size = solid
        .bounding_box()
        .map(|bounds| bounds.dimensions())
        .ok_or_else(|| GeometryError::Kernel("the refined bust has no extent".into()))?;
    let volume_mm3 = solid.volume();
    let mesh = single_precision(&solid)?;
    drop(solid);
    let report = BustReport {
        triangles: mesh.triangles.len(),
        size_mm: size,
        volume_mm3,
        requested_edge_length_mm: request.edge_length_mm,
        edge_length_mm: edge,
        estimated_deviation_p999_mm: estimated_deviation_p999(&mesh),
        trimmed_at_neck: cut.is_some(),
        discarded_fragments: fragments,
    };
    Ok((mesh, report))
}

/// Encode a mesh as binary STL with facet normals.
pub fn bust_stl(mesh: &TriangleMesh) -> Vec<u8> {
    let triangles: Vec<_> = mesh
        .triangles
        .iter()
        .map(|triangle| {
            let [a, b, c] = triangle.map(|index| mesh.vertices[index as usize]);
            let normal = normalize(cross(sub(b, a), sub(c, a)));
            stl_io::Triangle {
                normal: stl_io::Normal::new(normal.map(|value| value as f32)),
                vertices: [a, b, c]
                    .map(|vertex| stl_io::Vertex::new(vertex.map(|value| value as f32))),
            }
        })
        .collect();
    let mut bytes = Vec::with_capacity(84 + 50 * triangles.len());
    stl_io::write_stl(&mut bytes, triangles.iter()).expect("writing STL to memory cannot fail");
    bytes
}

fn validate(request: &BustRequest) -> Result<(), GeometryError> {
    let invalid = |message: &str| Err(GeometryError::InvalidOptions(message.into()));
    if !(request.height_mm.is_finite() && request.height_mm > 0.0) {
        return invalid("bust height must be a positive number of millimetres");
    }
    if !(request.edge_length_mm.is_finite() && request.edge_length_mm > 0.0) {
        return invalid("edge length must be a positive number of millimetres");
    }
    if request.max_triangles == 0 {
        return invalid("the triangle limit must be positive");
    }
    if request.vertices.is_empty() || request.triangles.is_empty() {
        return Err(GeometryError::EmptyMesh);
    }
    if let Some(index) = request
        .vertices
        .iter()
        .position(|vertex| !vertex.iter().all(|value| value.is_finite()))
    {
        return Err(GeometryError::NonFiniteVertex { index });
    }
    let count = request.vertices.len();
    for (triangle, indices) in request.triangles.iter().enumerate() {
        if indices.iter().any(|&index| index as usize >= count) {
            return Err(GeometryError::InvalidIndex { triangle });
        }
        if indices[0] == indices[1] || indices[1] == indices[2] || indices[0] == indices[2] {
            return invalid("a head triangle repeats a vertex");
        }
    }
    Ok(())
}

struct Opening {
    mean_height: f64,
    top_height: f64,
}

struct ClosedPart {
    vertices: Vec<[f64; 3]>,
    triangles: Vec<[u32; 3]>,
    openings: Vec<Opening>,
}

/// Split the mesh into connected parts, largest first, closing every boundary
/// loop with a fan of triangles around its centroid.
fn closed_parts(
    vertices: &[[f64; 3]],
    triangles: &[[u32; 3]],
) -> Result<Vec<ClosedPart>, GeometryError> {
    let mut parent: Vec<usize> = (0..vertices.len()).collect();
    fn root(parent: &mut [usize], mut index: usize) -> usize {
        while parent[index] != index {
            parent[index] = parent[parent[index]];
            index = parent[index];
        }
        index
    }
    for triangle in triangles {
        for pair in [[triangle[0], triangle[1]], [triangle[1], triangle[2]]] {
            let (a, b) = (
                root(&mut parent, pair[0] as usize),
                root(&mut parent, pair[1] as usize),
            );
            parent[a] = b;
        }
    }
    let mut groups: HashMap<usize, Vec<[u32; 3]>> = HashMap::new();
    for triangle in triangles {
        groups
            .entry(root(&mut parent, triangle[0] as usize))
            .or_default()
            .push(*triangle);
    }
    let mut groups: Vec<Vec<[u32; 3]>> = groups.into_values().collect();
    // Largest first; ties broken by first triangle so the order is deterministic.
    groups.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));
    groups
        .into_iter()
        .map(|group| close_part(vertices, group))
        .collect()
}

fn close_part(
    vertices: &[[f64; 3]],
    mut triangles: Vec<[u32; 3]>,
) -> Result<ClosedPart, GeometryError> {
    let invalid = |message: &str| GeometryError::InvalidOptions(message.into());
    let mut directed = HashSet::with_capacity(triangles.len() * 3);
    for [a, b, c] in &triangles {
        for edge in [(*a, *b), (*b, *c), (*c, *a)] {
            if !directed.insert(edge) {
                return Err(invalid(
                    "the head surface is not consistently oriented and manifold",
                ));
            }
        }
    }
    let mut next: HashMap<u32, u32> = HashMap::new();
    for &(a, b) in &directed {
        if !directed.contains(&(b, a)) && next.insert(a, b).is_some() {
            return Err(invalid(
                "the head surface boundary is not a set of simple loops",
            ));
        }
    }

    let mut used: Vec<u32> = triangles.iter().flatten().copied().collect();
    used.sort_unstable();
    used.dedup();
    let local: HashMap<u32, u32> = used
        .iter()
        .enumerate()
        .map(|(position, &index)| (index, position as u32))
        .collect();
    let mut part_vertices: Vec<[f64; 3]> =
        used.iter().map(|&index| vertices[index as usize]).collect();
    for triangle in &mut triangles {
        *triangle = triangle.map(|index| local[&index]);
    }

    let mut starts: Vec<u32> = next.keys().copied().collect();
    starts.sort_unstable();
    let mut visited = HashSet::new();
    let mut openings = Vec::new();
    for start in starts {
        if visited.contains(&start) {
            continue;
        }
        let mut boundary = Vec::new();
        let mut current = start;
        while visited.insert(current) {
            boundary.push(current);
            current = *next
                .get(&current)
                .ok_or_else(|| invalid("the head surface boundary is not a set of simple loops"))?;
        }
        if current != start {
            return Err(invalid(
                "the head surface boundary is not a set of simple loops",
            ));
        }
        let points: Vec<[f64; 3]> = boundary
            .iter()
            .map(|&index| vertices[index as usize])
            .collect();
        let count = points.len() as f64;
        let centre = points
            .iter()
            .fold([0.0; 3], |sum, point| {
                [sum[0] + point[0], sum[1] + point[1], sum[2] + point[2]]
            })
            .map(|value| value / count);
        let centre_index = part_vertices.len() as u32;
        part_vertices.push(centre);
        // Each cap triangle runs against its boundary edge, keeping orientation consistent.
        for (position, &a) in boundary.iter().enumerate() {
            let b = boundary[(position + 1) % boundary.len()];
            triangles.push([local[&b], local[&a], centre_index]);
        }
        openings.push(Opening {
            mean_height: points.iter().map(|point| point[1]).sum::<f64>() / count,
            top_height: points.iter().map(|point| point[1]).fold(f64::MIN, f64::max),
        });
    }
    Ok(ClosedPart {
        vertices: part_vertices,
        triangles,
        openings,
    })
}

/// The mesh as an STL stores it, checked to still be a closed solid.
fn single_precision(manifold: &Manifold) -> Result<TriangleMesh, GeometryError> {
    let mut mesh = mesh_from(manifold);
    separate_pinches(&mut mesh);
    // An STL reader joins vertices with equal coordinates, so do the same.
    let mut welded: HashMap<[u32; 3], u32> = HashMap::with_capacity(mesh.vertices.len());
    let mut vertices = Vec::with_capacity(mesh.vertices.len());
    let remap: Vec<u32> = mesh
        .vertices
        .iter()
        .map(|vertex| {
            let rounded = vertex.map(|coordinate| coordinate as f32);
            *welded.entry(rounded.map(f32::to_bits)).or_insert_with(|| {
                vertices.push(rounded.map(f64::from));
                (vertices.len() - 1) as u32
            })
        })
        .collect();
    let mesh = TriangleMesh {
        vertices,
        triangles: mesh
            .triangles
            .iter()
            .map(|triangle| triangle.map(|index| remap[index as usize]))
            .collect(),
    };
    // Each edge of a closed, consistently oriented surface is used once in
    // each direction.
    let mut directed = HashSet::with_capacity(mesh.triangles.len() * 3);
    for [a, b, c] in &mesh.triangles {
        for edge in [(*a, *b), (*b, *c), (*c, *a)] {
            if edge.0 == edge.1 || !directed.insert(edge) {
                return Err(GeometryError::Kernel(
                    "the bust is not a consistently oriented surface at STL precision".into(),
                ));
            }
        }
    }
    if directed.iter().any(|(a, b)| !directed.contains(&(*b, *a))) {
        return Err(GeometryError::Kernel(
            "the bust is not a closed surface at STL precision".into(),
        ));
    }
    Ok(mesh)
}

/// Where two parts of the surface touch at a point, the kernel keeps a
/// separate vertex for each part at the same position. An STL cannot express
/// that, so move each such vertex part of the way toward the centre of its
/// own neighbours, which separates the parts without changing the topology.
fn separate_pinches(mesh: &mut TriangleMesh) {
    let mut positions: HashMap<[u32; 3], Vec<usize>> = HashMap::new();
    for (index, vertex) in mesh.vertices.iter().enumerate() {
        positions
            .entry(vertex.map(|coordinate| (coordinate as f32).to_bits()))
            .or_default()
            .push(index);
    }
    let pinched: HashSet<usize> = positions
        .into_values()
        .filter(|group| group.len() > 1)
        .flatten()
        .collect();
    if pinched.is_empty() {
        return;
    }
    let mut sums: HashMap<usize, ([f64; 3], usize)> = HashMap::new();
    for triangle in &mesh.triangles {
        for (corner, &index) in triangle.iter().enumerate() {
            let index = index as usize;
            if !pinched.contains(&index) {
                continue;
            }
            let entry = sums.entry(index).or_insert(([0.0; 3], 0));
            for other in [triangle[(corner + 1) % 3], triangle[(corner + 2) % 3]] {
                for (total, value) in entry.0.iter_mut().zip(mesh.vertices[other as usize]) {
                    *total += value;
                }
                entry.1 += 1;
            }
        }
    }
    for (index, (sum, count)) in sums {
        let vertex = mesh.vertices[index];
        let centre = sum.map(|value| value / count as f64);
        mesh.vertices[index] = std::array::from_fn(|axis| {
            vertex[axis] + PINCH_SEPARATION * (centre[axis] - vertex[axis])
        });
    }
}

fn mesh_from(manifold: &Manifold) -> TriangleMesh {
    let (vertices, properties, triangles) = manifold.to_mesh_f64();
    TriangleMesh {
        vertices: vertices
            .chunks_exact(properties)
            .map(|vertex| [vertex[0], vertex[1], vertex[2]])
            .collect(),
        triangles: triangles
            .chunks_exact(3)
            .map(|triangle| [triangle[0] as u32, triangle[1] as u32, triangle[2] as u32])
            .collect(),
    }
}

/// Chordal deviation of an edge between facets meeting at angle theta is about
/// length times theta over eight.
fn estimated_deviation_p999(mesh: &TriangleMesh) -> Option<f64> {
    let normals: Vec<[f64; 3]> = mesh
        .triangles
        .iter()
        .map(|triangle| {
            let [a, b, c] = triangle.map(|index| mesh.vertices[index as usize]);
            normalize(cross(sub(b, a), sub(c, a)))
        })
        .collect();
    let mut edges: Vec<(u32, u32, u32)> = mesh
        .triangles
        .iter()
        .enumerate()
        .flat_map(|(face, [a, b, c])| {
            [(*a, *b), (*b, *c), (*c, *a)].map(|(p, q)| (p.min(q), p.max(q), face as u32))
        })
        .collect();
    edges.sort_unstable();
    let mut deviations: Vec<f64> = edges
        .windows(2)
        .filter(|pair| (pair[0].0, pair[0].1) == (pair[1].0, pair[1].1))
        .filter_map(|pair| {
            let cosine =
                dot(normals[pair[0].2 as usize], normals[pair[1].2 as usize]).clamp(-1.0, 1.0);
            let angle = cosine.acos();
            (angle.is_finite() && angle < SMOOTH_REGION_RADIANS).then(|| {
                let length = norm(sub(
                    mesh.vertices[pair[0].1 as usize],
                    mesh.vertices[pair[0].0 as usize],
                ));
                length * angle / 8.0
            })
        })
        .collect();
    if deviations.is_empty() {
        return None;
    }
    let index = ((deviations.len() as f64 * 0.999) as usize).min(deviations.len() - 1);
    Some(*deviations.select_nth_unstable_by(index, f64::total_cmp).1)
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn norm(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

fn normalize(a: [f64; 3]) -> [f64; 3] {
    let length = norm(a);
    if length > 0.0 {
        a.map(|value| value / length)
    } else {
        [0.0; 3]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An open-bottomed box standing on y: a closed cube with its lowest face
    /// removed, so it has one boundary loop, plus a separate closed cube.
    fn open_box_with_part() -> (Vec<[f64; 3]>, Vec<[u32; 3]>) {
        let cube = |offset: [f64; 3], size: f64| {
            let v: Vec<[f64; 3]> = (0..8)
                .map(|i| {
                    [
                        offset[0] + size * f64::from(i & 1),
                        offset[1] + size * f64::from((i >> 1) & 1),
                        offset[2] + size * f64::from((i >> 2) & 1),
                    ]
                })
                .collect();
            // Outward-facing quads as triangles; vertex bit 0 is x, bit 1 is y, bit 2 is z.
            let quads = [
                [0, 4, 6, 2], // -x
                [1, 3, 7, 5], // +x
                [0, 1, 5, 4], // -y
                [2, 6, 7, 3], // +y
                [0, 2, 3, 1], // -z
                [4, 5, 7, 6], // +z
            ];
            (v, quads)
        };
        let (head, quads) = cube([0.0, 0.0, 0.0], 40.0);
        let mut triangles: Vec<[u32; 3]> = quads
            .iter()
            .enumerate()
            .filter(|(face, _)| *face != 2) // remove the lowest (-y) face
            .flat_map(|(_, [a, b, c, d])| [[*a, *b, *c], [*a, *c, *d]])
            .collect();
        let (part, quads) = cube([15.0, 25.0, 39.0], 10.0);
        let mut vertices = head;
        let base = vertices.len() as u32;
        vertices.extend(part);
        triangles.extend(
            quads
                .iter()
                .flat_map(|[a, b, c, d]| [[*a, *b, *c], [*a, *c, *d]])
                .map(|triangle| triangle.map(|index| index + base)),
        );
        (vertices, triangles)
    }

    fn request(edge: f64, max_triangles: usize) -> BustRequest {
        let (vertices, triangles) = open_box_with_part();
        BustRequest {
            vertices,
            triangles,
            height_mm: 50.0,
            edge_length_mm: edge,
            max_triangles,
        }
    }

    #[test]
    fn open_surface_and_part_become_one_upright_solid_on_the_build_plate() {
        let (mesh, report) = prepare_bust(&request(2.0, 1_000_000)).unwrap();
        assert!(report.trimmed_at_neck);
        assert!((report.size_mm[2] - 50.0).abs() < 1e-6, "{report:?}");
        assert!(report.volume_mm3 > 0.0);
        let lowest = mesh
            .vertices
            .iter()
            .map(|vertex| vertex[2])
            .fold(f64::MAX, f64::min);
        assert!(lowest.abs() < 1e-9, "base rests at {lowest}");
        let manifold = Manifold::from_mesh_f64(
            &mesh.vertices.iter().flatten().copied().collect::<Vec<_>>(),
            3,
            &mesh
                .triangles
                .iter()
                .flatten()
                .map(|&index| u64::from(index))
                .collect::<Vec<_>>(),
        )
        .expect("output is a valid closed solid");
        assert_eq!(manifold.genus(), 0, "the part is united with the head");
    }

    #[test]
    fn triangle_limit_coarsens_the_edge_length() {
        let (_, report) = prepare_bust(&request(0.05, 20_000)).unwrap();
        assert!(report.triangles <= 20_000, "{report:?}");
        assert!(report.edge_length_mm > report.requested_edge_length_mm);
    }

    #[test]
    fn stl_round_trips_through_the_validation_pipeline() {
        let (mesh, _) = prepare_bust(&request(2.0, 1_000_000)).unwrap();
        let report =
            crate::analyze_stl(&bust_stl(&mesh), crate::ValidationOptions::default()).unwrap();
        assert!(report.printable, "{report:?}");
        assert_eq!(report.triangles, mesh.triangles.len());
    }

    #[test]
    fn invalid_requests_are_refused() {
        let mut bad = request(2.0, 1_000);
        bad.height_mm = 0.0;
        assert!(matches!(
            prepare_bust(&bad),
            Err(GeometryError::InvalidOptions(_))
        ));
        let mut bad = request(2.0, 1_000);
        bad.triangles[0] = [0, 0, 1];
        assert!(matches!(
            prepare_bust(&bad),
            Err(GeometryError::InvalidOptions(_))
        ));
        let mut bad = request(2.0, 1_000);
        bad.triangles.push(bad.triangles[0]);
        assert!(matches!(
            prepare_bust(&bad),
            Err(GeometryError::InvalidOptions(_))
        ));
    }
}
