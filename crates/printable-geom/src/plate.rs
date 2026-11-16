//! Pure geometry in a 3MF document's millimetre coordinate frame.

use serde::{Deserialize, Serialize};

use crate::{GeometryError, MeshBounds, TriangleMesh, limit};

/// 3MF stores the three linear columns followed by translation. A point's X is
/// m0*x + m3*y + m6*z + m9; component transforms apply before build transforms.
pub type Transform3mf = [f64; 12];

pub const IDENTITY_3MF: Transform3mf = [1., 0., 0., 0., 1., 0., 0., 0., 1., 0., 0., 0.];

/// Canonical oriented triangles on a 0.0001 mm coordinate grid. Vertex/resource
/// numbering and triangle order do not affect this representation. Reversing
/// winding does: this evidence does not silently equate inverted surfaces.
pub fn canonical_triangles(mesh: &TriangleMesh) -> Result<Vec<[[i64; 3]; 3]>, GeometryError> {
    model_footprint(mesh)?;
    let mut triangles = Vec::with_capacity(mesh.triangles.len());
    for indices in &mesh.triangles {
        limit::check()?;
        let points = indices
            .map(|index| mesh.vertices[index as usize].map(|v| (v * 10_000.).round() as i64));
        let rotations = [
            points,
            [points[1], points[2], points[0]],
            [points[2], points[0], points[1]],
        ];
        triangles.push(*rotations.iter().min().expect("three rotations"));
    }
    triangles.sort_unstable();
    Ok(triangles)
}

fn invalid(message: &str) -> GeometryError {
    GeometryError::InvalidOptions(message.into())
}

pub fn validate_transform(transform: &Transform3mf) -> Result<(), GeometryError> {
    if !transform.iter().all(|value| value.is_finite()) {
        return Err(invalid("3MF transforms require finite coefficients"));
    }
    Ok(())
}

pub fn compose_3mf(
    parent: &Transform3mf,
    child: &Transform3mf,
) -> Result<Transform3mf, GeometryError> {
    validate_transform(parent)?;
    validate_transform(child)?;
    let result = std::array::from_fn(|index| {
        let (column, row) = (index / 3, index % 3);
        (0..3)
            .map(|k| parent[k * 3 + row] * child[column * 3 + k])
            .sum::<f64>()
            + if column == 3 { parent[9 + row] } else { 0. }
    });
    validate_transform(&result)?;
    Ok(result)
}

/// Preserve triangle indexing while changing only vertex positions.
pub fn transform_mesh(
    mesh: &TriangleMesh,
    transform: &Transform3mf,
) -> Result<TriangleMesh, GeometryError> {
    validate_transform(transform)?;
    if mesh.vertices.is_empty() || mesh.triangles.is_empty() {
        return Err(GeometryError::EmptyMesh);
    }
    for (triangle, indices) in mesh.triangles.iter().enumerate() {
        limit::check()?;
        if indices
            .iter()
            .any(|index| *index as usize >= mesh.vertices.len())
        {
            return Err(GeometryError::InvalidIndex { triangle });
        }
    }
    let mut vertices = Vec::with_capacity(mesh.vertices.len());
    for (index, point) in mesh.vertices.iter().enumerate() {
        limit::check()?;
        if !point.iter().all(|value| value.is_finite()) {
            return Err(GeometryError::NonFiniteVertex { index });
        }
        let transformed: [f64; 3] = std::array::from_fn(|row| {
            (0..3)
                .map(|column| transform[column * 3 + row] * point[column])
                .sum::<f64>()
                + transform[9 + row]
        });
        if transformed.iter().any(|value| !value.is_finite()) {
            return Err(invalid(
                "transformed 3MF geometry exceeds the numeric range",
            ));
        }
        vertices.push(transformed);
    }
    Ok(TriangleMesh {
        vertices,
        triangles: mesh.triangles.clone(),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFootprint {
    pub bounds: MeshBounds,
    /// Conservative XY outline: holes and concavities are filled. An outline
    /// overlap therefore does not establish solid interference.
    pub convex_hull_mm: Vec<[f64; 2]>,
}

/// Measure referenced triangle vertices; unused vertices add no footprint.
pub fn model_footprint(mesh: &TriangleMesh) -> Result<ModelFootprint, GeometryError> {
    if mesh.vertices.is_empty() || mesh.triangles.is_empty() {
        return Err(GeometryError::EmptyMesh);
    }
    let mut used = vec![false; mesh.vertices.len()];
    for (triangle, indices) in mesh.triangles.iter().enumerate() {
        limit::check()?;
        for index in indices {
            let Some(used) = used.get_mut(*index as usize) else {
                return Err(GeometryError::InvalidIndex { triangle });
            };
            *used = true;
        }
    }
    let mut minimum_mm = [f64::INFINITY; 3];
    let mut maximum_mm = [f64::NEG_INFINITY; 3];
    let mut points = Vec::new();
    for (_, point) in mesh
        .vertices
        .iter()
        .enumerate()
        .filter(|(index, _)| used[*index])
    {
        limit::check()?;
        if point
            .iter()
            .any(|value| !value.is_finite() || value.abs() > 1e7)
        {
            return Err(invalid(
                "plate footprint coordinates exceed the supported numeric range",
            ));
        }
        for axis in 0..3 {
            minimum_mm[axis] = minimum_mm[axis].min(point[axis]);
            maximum_mm[axis] = maximum_mm[axis].max(point[axis]);
        }
        points.push([point[0], point[1]]);
    }
    points.sort_unstable_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    points.dedup();
    let side = |a: [f64; 2], b: [f64; 2], c: [f64; 2]| {
        let coord = |p: [f64; 2]| robust::Coord { x: p[0], y: p[1] };
        robust::orient2d(coord(a), coord(b), coord(c))
    };
    let mut hull: Vec<[f64; 2]> = Vec::new();
    for direction in [false, true] {
        let start = hull.len();
        for i in 0..points.len() {
            limit::check()?;
            let point = points[if direction { points.len() - 1 - i } else { i }];
            while hull.len() >= start + 2
                && side(hull[hull.len() - 2], hull[hull.len() - 1], point) <= 0.
            {
                hull.pop();
            }
            hull.push(point);
        }
        if hull.len() > start {
            hull.pop();
        }
    }
    if hull.is_empty() {
        hull = points;
    }
    Ok(ModelFootprint {
        bounds: MeshBounds {
            minimum_mm,
            maximum_mm,
            dimensions_mm: std::array::from_fn(|axis| maximum_mm[axis] - minimum_mm[axis]),
        },
        convex_hull_mm: hull,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh() -> TriangleMesh {
        TriangleMesh {
            vertices: vec![
                [0., 0., 0.],
                [2., 0., 0.],
                [2., 1., 0.],
                [0., 1., 0.],
                [999., 999., 999.],
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
        }
    }

    #[test]
    fn component_transforms_precede_document_transforms_without_phantom_footprints() {
        let mut translation = IDENTITY_3MF;
        translation[9..].copy_from_slice(&[20., 30., 4.]);
        let rotation = [0., 1., 0., -1., 0., 0., 0., 0., 1., 0., 0., 0.];
        let result =
            transform_mesh(&mesh(), &compose_3mf(&translation, &rotation).unwrap()).unwrap();
        assert_eq!(result.vertices[1], [20., 32., 4.]);
        assert_eq!(result.triangles, mesh().triangles);
        let footprint = model_footprint(&result).unwrap();
        assert_eq!(footprint.bounds.minimum_mm, [19., 30., 4.]);
        assert_eq!(footprint.bounds.maximum_mm, [20., 32., 4.]);
        assert_eq!(footprint.convex_hull_mm.len(), 4);
    }

    #[test]
    fn invalid_geometry_and_unrepresentable_transforms_remain_errors() {
        let mut transform = IDENTITY_3MF;
        transform[0] = f64::INFINITY;
        assert!(transform_mesh(&mesh(), &transform).is_err());
        let mut bad = mesh();
        bad.triangles[0][1] = 99;
        assert!(model_footprint(&bad).is_err());
        bad = mesh();
        bad.vertices[0][0] = f64::NAN;
        assert!(model_footprint(&bad).is_err());
    }

    #[test]
    fn canonical_geometry_ignores_numbering_but_retains_winding_and_shape() {
        let original = mesh();
        let mut reordered = original.clone();
        reordered.triangles = vec![[2, 3, 0], [1, 2, 0]];
        assert_eq!(
            canonical_triangles(&original).unwrap(),
            canonical_triangles(&reordered).unwrap()
        );
        reordered.triangles[0].swap(1, 2);
        assert_ne!(
            canonical_triangles(&original).unwrap(),
            canonical_triangles(&reordered).unwrap()
        );
        reordered = original.clone();
        reordered.vertices[0][0] += 0.001;
        assert_ne!(
            canonical_triangles(&original).unwrap(),
            canonical_triangles(&reordered).unwrap()
        );
    }
}
