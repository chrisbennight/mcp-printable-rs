//! Pure triangle-mesh geometry and FDM printability analysis.
//!
//! STL decoding is byte-oriented and synchronous. Geometry validation uses
//! Parry for topology and connected components, Manifold for solid validity
//! and mass properties, and analytic triangle calculations for build-plane and
//! overhang reporting. Assembly analysis combines Manifold intersections with
//! Parry proximity queries and continuous rigid-motion analysis. The crate
//! performs no filesystem, network, or async I/O.

use std::collections::HashSet;
use std::io::Cursor;

use manifold_csg::Manifold;
use parry3d_f64::bounding_volume::Aabb;
use parry3d_f64::mass_properties::details::trimesh_signed_volume_and_center_of_mass;
use parry3d_f64::math::Vector;
use parry3d_f64::partitioning::{Bvh, BvhBuildStrategy};
use parry3d_f64::query::PointQuery;
use parry3d_f64::shape::{
    Tetrahedron, TopologyError, TriMesh, TriMeshConnectedComponents, TriMeshFlags,
};
use serde::{Deserialize, Serialize};

mod assembly;
mod assessment;

pub use assessment::{AssessmentCriterion, AssessmentStatus, CriterionStatus, MeshAssessment};

pub use assembly::{
    AssemblyOptions, AssemblyPartSummary, AssemblyRelation, AssemblyReport, AssemblyStaticReport,
    LinearMotion, LinearMotionReport, MotionBlockReason, RotationalMotion, RotationalMotionReport,
    SurfaceWitnesses, analyze_assembly, analyze_assembly_stl,
};

/// A neutral indexed triangle mesh in millimetres.
#[derive(Debug, Clone, PartialEq)]
pub struct TriangleMesh {
    pub vertices: Vec<[f64; 3]>,
    pub triangles: Vec<[u32; 3]>,
}

/// Caller-selected print analysis parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ValidationOptions {
    pub build_direction: [f64; 3],
    pub overhang_angle_degrees: f64,
    pub density_g_cm3: Option<f64>,
}

impl Default for ValidationOptions {
    fn default() -> Self {
        Self {
            build_direction: [0.0, 0.0, 1.0],
            overhang_angle_degrees: 45.0,
            density_g_cm3: None,
        }
    }
}

/// Geometry input or option failure that prevents a trustworthy report.
#[derive(Debug, thiserror::Error)]
pub enum GeometryError {
    #[error("STL could not be decoded: {0}")]
    InvalidStl(String),
    #[error("mesh must contain at least one vertex and one triangle")]
    EmptyMesh,
    #[error("mesh vertex {index} contains a non-finite coordinate")]
    NonFiniteVertex { index: usize },
    #[error("mesh triangle {triangle} references a missing vertex")]
    InvalidIndex { triangle: usize },
    #[error("mesh contains more vertices than the geometry kernel can index")]
    TooManyVertices,
    #[error("{0}")]
    InvalidOptions(String),
    #[error("geometry analysis failed: {0}")]
    Kernel(String),
    #[error("assembly {part} part is not a valid closed solid: {reason}")]
    InvalidAssemblyPart { part: &'static str, reason: String },
}

impl GeometryError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidStl(_) => "invalid_stl",
            Self::EmptyMesh => "empty_mesh",
            Self::NonFiniteVertex { .. } => "non_finite_geometry",
            Self::InvalidIndex { .. } => "invalid_mesh_index",
            Self::TooManyVertices => "mesh_too_large",
            Self::InvalidOptions(_) => "validation",
            Self::Kernel(_) => "geometry",
            Self::InvalidAssemblyPart { .. } => "invalid_assembly_part",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationIssue {
    pub severity: IssueSeverity,
    pub code: &'static str,
    pub message: String,
    pub recommendation: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshBounds {
    pub minimum_mm: [f64; 3],
    pub maximum_mm: [f64; 3],
    pub dimensions_mm: [f64; 3],
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopologyReport {
    pub watertight: bool,
    pub consistently_oriented: bool,
    pub manifold: bool,
    pub connected_components: usize,
    pub boundary_edges: Option<usize>,
    pub degenerate_triangles: usize,
    pub duplicate_triangles: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SolidProperties {
    pub volume_mm3: f64,
    pub surface_area_mm2: f64,
    pub center_of_mass_mm: [f64; 3],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub density_g_cm3: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mass_g: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FaceCategory {
    pub faces: usize,
    pub area_mm2: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OverhangReport {
    pub build_direction: [f64; 3],
    pub threshold_degrees: f64,
    pub bed_contact: FaceCategory,
    pub supported: FaceCategory,
    pub warning: FaceCategory,
    pub severe: FaceCategory,
    pub requires_support: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValidationReport {
    pub vertices: usize,
    pub triangles: usize,
    pub bounds: MeshBounds,
    pub topology: TopologyReport,
    pub surface_area_mm2: f64,
    pub signed_volume_mm3: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solid_properties: Option<SolidProperties>,
    pub overhang: OverhangReport,
    pub solid_geometry: bool,
    /// Compatibility alias for solid_geometry, not manufacturing qualification.
    pub printable: bool,
    pub assessment: MeshAssessment,
    pub issues: Vec<ValidationIssue>,
}

/// Decode an ASCII or binary STL and analyze it as an indexed mesh.
pub fn analyze_stl(
    bytes: &[u8],
    options: ValidationOptions,
) -> Result<ValidationReport, GeometryError> {
    analyze_mesh(&decode_stl(bytes)?, options)
}

fn decode_stl(bytes: &[u8]) -> Result<TriangleMesh, GeometryError> {
    let indexed = stl_io::read_stl(&mut Cursor::new(bytes))
        .map_err(|error| GeometryError::InvalidStl(error.to_string()))?;
    u32::try_from(indexed.vertices.len()).map_err(|_| GeometryError::TooManyVertices)?;
    let vertices: Vec<[f64; 3]> = indexed
        .vertices
        .into_iter()
        .map(|vertex| vertex.0.map(f64::from))
        .collect();
    let mut triangles = Vec::with_capacity(indexed.faces.len());
    for (triangle, face) in indexed.faces.into_iter().enumerate() {
        let indices = face
            .vertices
            .map(|index| u32::try_from(index).map_err(|_| GeometryError::TooManyVertices))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        triangles.push([indices[0], indices[1], indices[2]]);
        if triangles[triangle]
            .iter()
            .any(|index| *index as usize >= vertices.len())
        {
            return Err(GeometryError::InvalidIndex { triangle });
        }
    }
    Ok(TriangleMesh {
        vertices,
        triangles,
    })
}

/// Analyze a neutral triangle mesh without mutating or repairing it.
pub fn analyze_mesh(
    mesh: &TriangleMesh,
    options: ValidationOptions,
) -> Result<ValidationReport, GeometryError> {
    Ok(prepare_mesh(mesh, options)?.report)
}

struct PreparedMesh {
    report: ValidationReport,
    trimesh: TriMesh,
    manifold: Option<Manifold>,
}

fn prepare_mesh(
    mesh: &TriangleMesh,
    options: ValidationOptions,
) -> Result<PreparedMesh, GeometryError> {
    if mesh.vertices.is_empty() || mesh.triangles.is_empty() {
        return Err(GeometryError::EmptyMesh);
    }
    let build_direction = normalized_direction(options.build_direction, "build_direction")?;
    if !options.overhang_angle_degrees.is_finite()
        || !(0.0..=90.0).contains(&options.overhang_angle_degrees)
    {
        return Err(GeometryError::InvalidOptions(
            "overhang_angle_degrees must be finite and between 0 and 90".to_string(),
        ));
    }
    if options
        .density_g_cm3
        .is_some_and(|density| !density.is_finite() || density <= 0.0)
    {
        return Err(GeometryError::InvalidOptions(
            "density_g_cm3 must be a finite positive number".to_string(),
        ));
    }
    for (index, vertex) in mesh.vertices.iter().enumerate() {
        if vertex.iter().any(|coordinate| !coordinate.is_finite()) {
            return Err(GeometryError::NonFiniteVertex { index });
        }
    }
    for (triangle, indices) in mesh.triangles.iter().enumerate() {
        if indices
            .iter()
            .any(|index| *index as usize >= mesh.vertices.len())
        {
            return Err(GeometryError::InvalidIndex { triangle });
        }
    }

    let bounds = mesh_bounds(&mesh.vertices);
    let parry_vertices: Vec<_> = mesh
        .vertices
        .iter()
        .map(|vertex| Vector::new(vertex[0], vertex[1], vertex[2]))
        .collect();
    let mut trimesh = TriMesh::with_flags(
        parry_vertices.clone(),
        mesh.triangles.clone(),
        TriMeshFlags::CONNECTED_COMPONENTS,
    )
    .map_err(|_| GeometryError::EmptyMesh)?;
    let component_signed_volumes =
        trimesh
            .connected_components()
            .map_or_else(Vec::new, |components| {
                connected_component_signed_volumes(components, &parry_vertices, &mesh.triangles)
            });
    let components = component_signed_volumes.len();

    let (degenerate_triangles, duplicate_triangles) = mesh_defects(mesh, &parry_vertices);
    let topology_result = trimesh
        .set_flags(TriMeshFlags::CONNECTED_COMPONENTS.union(TriMeshFlags::HALF_EDGE_TOPOLOGY));
    let boundary_edges = topology_result.as_ref().ok().and_then(|_| {
        trimesh.topology().map(|topology| {
            topology
                .half_edges
                .iter()
                .filter(|edge| edge.twin == u32::MAX)
                .count()
        })
    });

    let (signed_volume, center_of_mass) =
        trimesh_signed_volume_and_center_of_mass(&parry_vertices, &mesh.triangles);
    let raw_surface_area = mesh_surface_area(mesh);
    let mut issues = Vec::new();
    if degenerate_triangles > 0 {
        issues.push(error_issue(
            "degenerate_triangles",
            format!("{degenerate_triangles} triangles have zero area or repeated vertices"),
            "Remove or retriangulate degenerate faces before slicing.",
        ));
    }
    if duplicate_triangles > 0 {
        issues.push(error_issue(
            "duplicate_triangles",
            format!("{duplicate_triangles} triangles duplicate an earlier face"),
            "Remove duplicate faces before slicing.",
        ));
    }
    if let Err(error) = topology_result {
        issues.push(topology_issue(error));
    } else if let Some(count) = boundary_edges
        && count > 0
    {
        issues.push(error_issue(
            "open_mesh",
            format!("mesh has {count} boundary edges and does not enclose a solid"),
            "Close the reported holes or re-export a watertight mesh.",
        ));
    }
    let watertight = topology_result.is_ok() && boundary_edges == Some(0);
    let shell_analysis = if watertight && degenerate_triangles == 0 && duplicate_triangles == 0 {
        let connected_components = trimesh.connected_components().ok_or_else(|| {
            GeometryError::Kernel("connected-component data is unavailable".to_string())
        })?;
        Some(analyze_shells(
            connected_components,
            &trimesh,
            &component_signed_volumes,
        )?)
    } else {
        None
    };
    if let Some(shells) = &shell_analysis
        && shells.orientation_errors > 0
    {
        issues.push(error_issue(
            "inconsistent_shell_orientation",
            format!(
                "{} connected surface shells have winding inconsistent with their containment depth",
                shells.orientation_errors
            ),
            "Recalculate shell normals so outer boundaries face outward and cavity boundaries face inward.",
        ));
    }
    if let Some(shells) = &shell_analysis
        && shells.solid_regions > 1
    {
        issues.push(warning_issue(
            "multiple_components",
            format!(
                "mesh contains {} disconnected solid regions across {components} surface shells",
                shells.solid_regions
            ),
            "Confirm the STL intentionally contains multiple printable parts.",
        ));
    }

    let consistently_oriented = topology_result.is_ok()
        && shell_analysis
            .as_ref()
            .is_none_or(|shells| shells.orientation_errors == 0);
    let eligible_for_manifold = watertight
        && consistently_oriented
        && degenerate_triangles == 0
        && duplicate_triangles == 0;
    let manifold = if eligible_for_manifold {
        let flat_vertices: Vec<_> = mesh.vertices.iter().flatten().copied().collect();
        let flat_indices: Vec<_> = mesh
            .triangles
            .iter()
            .flatten()
            .map(|index| u64::from(*index))
            .collect();
        match Manifold::from_mesh_f64(&flat_vertices, 3, &flat_indices) {
            Ok(manifold) => Some(manifold),
            Err(error) => {
                issues.push(error_issue(
                    "non_manifold_solid",
                    format!("Manifold rejected the mesh: {error}"),
                    "Repair self-intersections and non-manifold topology before slicing.",
                ));
                None
            }
        }
    } else {
        None
    };

    let solid_geometry = manifold.is_some();
    let solid_properties = manifold
        .as_ref()
        .map(|manifold| {
            let volume_mm3 = manifold.volume();
            let surface_area_mm2 = manifold.surface_area();
            let center_of_mass_mm = center_of_mass.to_array();
            validate_solid_properties(volume_mm3, surface_area_mm2, center_of_mass_mm)?;
            let mass_g = options
                .density_g_cm3
                .map(|density| volume_mm3 * density / 1000.0);
            if mass_g.is_some_and(|mass| !mass.is_finite()) {
                return Err(GeometryError::InvalidOptions(
                    "density_g_cm3 produces a mass outside the finite numeric range".to_string(),
                ));
            }
            Ok(SolidProperties {
                volume_mm3,
                surface_area_mm2,
                center_of_mass_mm,
                density_g_cm3: options.density_g_cm3,
                mass_g,
            })
        })
        .transpose()?;
    let overhang = analyze_overhang(
        mesh,
        build_direction,
        options.overhang_angle_degrees,
        &bounds,
    );
    if overhang.requires_support {
        issues.push(warning_issue(
            "support_recommended",
            format!(
                "{} warning and {} severe faces may require support",
                overhang.warning.faces, overhang.severe.faces
            ),
            "Review the heatmap, reorient the part, add support, or redesign the overhangs.",
        ));
    }

    let assessment = MeshAssessment::new(
        solid_geometry,
        bounds.dimensions_mm.iter().all(|value| value.is_finite()),
        overhang.requires_support,
    );
    Ok(PreparedMesh {
        report: ValidationReport {
            vertices: mesh.vertices.len(),
            triangles: mesh.triangles.len(),
            bounds,
            topology: TopologyReport {
                watertight,
                consistently_oriented,
                manifold: solid_geometry,
                connected_components: components,
                boundary_edges,
                degenerate_triangles,
                duplicate_triangles,
            },
            surface_area_mm2: raw_surface_area,
            signed_volume_mm3: signed_volume,
            solid_properties,
            assessment,
            overhang,
            solid_geometry,
            printable: solid_geometry,
            issues,
        },
        trimesh,
        manifold,
    })
}

/// Volume and validity of an origin-centred cube/sphere intersection.
pub fn cube_sphere_intersection(
    cube_edge: f64,
    sphere_radius: f64,
    sphere_segments: i32,
) -> (f64, bool) {
    let cube = Manifold::cube(cube_edge, cube_edge, cube_edge, true);
    let sphere = Manifold::sphere(sphere_radius, sphere_segments);
    let intersection = &cube ^ &sphere;
    (intersection.volume(), intersection.status().is_ok())
}

fn normalized_direction(
    direction: [f64; 3],
    field: &'static str,
) -> Result<[f64; 3], GeometryError> {
    if direction.iter().any(|component| !component.is_finite()) {
        return Err(GeometryError::InvalidOptions(format!(
            "{field} must contain three finite numbers and be non-zero"
        )));
    }
    let scale = direction
        .iter()
        .map(|component| component.abs())
        .fold(0.0_f64, f64::max);
    if scale == 0.0 {
        return Err(GeometryError::InvalidOptions(format!(
            "{field} must contain three finite numbers and be non-zero"
        )));
    }
    let scaled = direction.map(|component| component / scale);
    let magnitude = scaled
        .iter()
        .fold(0.0_f64, |length, component| length.hypot(*component));
    Ok(scaled.map(|component| component / magnitude))
}

fn mesh_bounds(vertices: &[[f64; 3]]) -> MeshBounds {
    let mut minimum = vertices[0];
    let mut maximum = vertices[0];
    for vertex in &vertices[1..] {
        for axis in 0..3 {
            minimum[axis] = minimum[axis].min(vertex[axis]);
            maximum[axis] = maximum[axis].max(vertex[axis]);
        }
    }
    MeshBounds {
        minimum_mm: minimum,
        maximum_mm: maximum,
        dimensions_mm: std::array::from_fn(|axis| maximum[axis] - minimum[axis]),
    }
}

fn mesh_defects(mesh: &TriangleMesh, vertices: &[Vector]) -> (usize, usize) {
    let mut degenerate = 0;
    let mut duplicate = 0;
    let mut seen = HashSet::with_capacity(mesh.triangles.len());
    for triangle in &mesh.triangles {
        let a = vertices[triangle[0] as usize];
        let b = vertices[triangle[1] as usize];
        let c = vertices[triangle[2] as usize];
        if (b - a).cross(c - a).length() == 0.0 {
            degenerate += 1;
        }
        let mut canonical = *triangle;
        canonical.sort_unstable();
        if !seen.insert(canonical) {
            duplicate += 1;
        }
    }
    (degenerate, duplicate)
}

fn mesh_surface_area(mesh: &TriangleMesh) -> f64 {
    mesh.triangles
        .iter()
        .map(|triangle| {
            let a = Vector::from_array(mesh.vertices[triangle[0] as usize]);
            let b = Vector::from_array(mesh.vertices[triangle[1] as usize]);
            let c = Vector::from_array(mesh.vertices[triangle[2] as usize]);
            (b - a).cross(c - a).length() * 0.5
        })
        .sum()
}

fn connected_component_signed_volumes(
    components: &TriMeshConnectedComponents,
    vertices: &[Vector],
    triangles: &[[u32; 3]],
) -> Vec<f64> {
    components
        .ranges
        .windows(2)
        .map(|range| {
            let faces = &components.grouped_faces[range[0]..range[1]];
            let reference_triangle = triangles[faces[0] as usize];
            let reference = vertices[reference_triangle[0] as usize];
            faces
                .iter()
                .map(|face| {
                    let triangle = triangles[*face as usize];
                    Tetrahedron::new(
                        reference,
                        vertices[triangle[0] as usize],
                        vertices[triangle[1] as usize],
                        vertices[triangle[2] as usize],
                    )
                    .signed_volume()
                })
                .sum()
        })
        .collect()
}

fn validate_solid_properties(
    volume_mm3: f64,
    surface_area_mm2: f64,
    center_of_mass_mm: [f64; 3],
) -> Result<(), GeometryError> {
    if !volume_mm3.is_finite()
        || !surface_area_mm2.is_finite()
        || center_of_mass_mm
            .iter()
            .any(|coordinate| !coordinate.is_finite())
    {
        return Err(GeometryError::Kernel(
            "solid properties are not finite".to_string(),
        ));
    }
    Ok(())
}

struct ShellAnalysis {
    orientation_errors: usize,
    solid_regions: usize,
}

fn analyze_shells(
    components: &TriMeshConnectedComponents,
    mesh: &TriMesh,
    signed_volumes: &[f64],
) -> Result<ShellAnalysis, GeometryError> {
    let buffers = components.to_mesh_buffers(mesh);
    if buffers.len() != signed_volumes.len() {
        return Err(GeometryError::Kernel(
            "connected-component buffers do not match component volumes".to_string(),
        ));
    }
    let shells = buffers
        .into_iter()
        .zip(signed_volumes)
        .map(|((vertices, mut triangles), volume)| {
            if *volume < 0.0 {
                for triangle in &mut triangles {
                    triangle.swap(1, 2);
                }
            }
            let sample = vertices[triangles[0][0] as usize];
            let shell = TriMesh::with_flags(vertices, triangles, TriMeshFlags::ORIENTED)
                .map_err(|error| GeometryError::Kernel(error.to_string()))?;
            Ok((sample, shell))
        })
        .collect::<Result<Vec<_>, GeometryError>>()?;
    let shell_bounds = shells
        .iter()
        .map(|(_, shell)| shell.local_aabb())
        .collect::<Vec<_>>();
    let shell_bvh = Bvh::from_leaves(BvhBuildStrategy::default(), &shell_bounds);

    let mut orientation_errors = 0;
    let mut solid_regions = 0;
    for (index, (sample, _)) in shells.iter().enumerate() {
        let point_bounds = Aabb::new(*sample, *sample);
        let containment_depth = shell_bvh
            .intersect_aabb(&point_bounds)
            .filter(|other_index| {
                *other_index as usize != index
                    && shells[*other_index as usize]
                        .1
                        .contains_local_point(*sample)
            })
            .count();
        let expected_positive = containment_depth % 2 == 0;
        if expected_positive {
            solid_regions += 1;
        }
        let volume = signed_volumes[index];
        if volume == 0.0 || (volume > 0.0) != expected_positive {
            orientation_errors += 1;
        }
    }
    Ok(ShellAnalysis {
        orientation_errors,
        solid_regions,
    })
}

fn analyze_overhang(
    mesh: &TriangleMesh,
    build_direction: [f64; 3],
    threshold: f64,
    bounds: &MeshBounds,
) -> OverhangReport {
    let direction = Vector::from_array(build_direction);
    let minimum_projection = mesh
        .vertices
        .iter()
        .map(|vertex| Vector::from_array(*vertex).dot(direction))
        .fold(f64::INFINITY, f64::min);
    let local_scale = bounds.dimensions_mm.iter().copied().fold(1.0_f64, f64::max);
    let bed_tolerance = local_scale * f64::from(f32::EPSILON) * 8.0;
    let mut report = OverhangReport {
        build_direction,
        threshold_degrees: threshold,
        bed_contact: FaceCategory {
            faces: 0,
            area_mm2: 0.0,
        },
        supported: FaceCategory {
            faces: 0,
            area_mm2: 0.0,
        },
        warning: FaceCategory {
            faces: 0,
            area_mm2: 0.0,
        },
        severe: FaceCategory {
            faces: 0,
            area_mm2: 0.0,
        },
        requires_support: false,
    };
    for triangle in &mesh.triangles {
        let points = triangle.map(|index| Vector::from_array(mesh.vertices[index as usize]));
        let cross = (points[1] - points[0]).cross(points[2] - points[0]);
        let doubled_area = cross.length();
        if doubled_area == 0.0 {
            continue;
        }
        let area = doubled_area * 0.5;
        let normal = cross / doubled_area;
        let on_bed = normal.dot(direction) < 0.0
            && points
                .iter()
                .all(|point| point.dot(direction) - minimum_projection <= bed_tolerance);
        if on_bed {
            report.bed_contact.faces += 1;
            report.bed_contact.area_mm2 += area;
            report.supported.faces += 1;
            report.supported.area_mm2 += area;
            continue;
        }
        let angle = (-normal.dot(direction)).clamp(0.0, 1.0).asin().to_degrees();
        let category = if angle <= threshold {
            &mut report.supported
        } else if angle <= (threshold + 15.0).min(90.0) {
            &mut report.warning
        } else {
            &mut report.severe
        };
        category.faces += 1;
        category.area_mm2 += area;
    }
    report.requires_support = report.warning.faces > 0 || report.severe.faces > 0;
    report
}

fn topology_issue(error: TopologyError) -> ValidationIssue {
    match error {
        TopologyError::BadTriangle(triangle) => error_issue(
            "bad_topology_triangle",
            format!("triangle {triangle} repeats a vertex"),
            "Remove or retriangulate the reported face.",
        ),
        TopologyError::BadAdjacentTrianglesOrientation {
            triangle1,
            triangle2,
            edge,
        } => error_issue(
            "inconsistent_winding",
            format!(
                "triangles {triangle1} and {triangle2} traverse shared edge {:?} in the same direction",
                edge
            ),
            "Recalculate consistent outward normals before slicing.",
        ),
    }
}

fn error_issue(
    code: &'static str,
    message: String,
    recommendation: &'static str,
) -> ValidationIssue {
    ValidationIssue {
        severity: IssueSeverity::Error,
        code,
        message,
        recommendation,
    }
}

fn warning_issue(
    code: &'static str,
    message: String,
    recommendation: &'static str,
) -> ValidationIssue {
    ValidationIssue {
        severity: IssueSeverity::Warning,
        code,
        message,
        recommendation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube(size: f64, offset: [f64; 3]) -> TriangleMesh {
        let [x, y, z] = offset;
        let vertices = vec![
            [x, y, z],
            [x + size, y, z],
            [x + size, y + size, z],
            [x, y + size, z],
            [x, y, z + size],
            [x + size, y, z + size],
            [x + size, y + size, z + size],
            [x, y + size, z + size],
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

    fn append_mesh(destination: &mut TriangleMesh, source: TriangleMesh) {
        let offset = destination.vertices.len() as u32;
        destination.vertices.extend(source.vertices);
        destination.triangles.extend(
            source
                .triangles
                .into_iter()
                .map(|triangle| triangle.map(|index| index + offset)),
        );
    }

    fn mesh_from_manifold(manifold: &Manifold) -> TriangleMesh {
        let (vertices, properties_per_vertex, triangles) = manifold.to_mesh_f64();
        TriangleMesh {
            vertices: vertices
                .chunks_exact(properties_per_vertex)
                .map(|vertex| [vertex[0], vertex[1], vertex[2]])
                .collect(),
            triangles: triangles
                .chunks_exact(3)
                .map(|triangle| {
                    [
                        u32::try_from(triangle[0]).expect("test mesh index fits u32"),
                        u32::try_from(triangle[1]).expect("test mesh index fits u32"),
                        u32::try_from(triangle[2]).expect("test mesh index fits u32"),
                    ]
                })
                .collect(),
        }
    }

    #[test]
    fn cube_report_matches_analytic_properties_and_build_plate() {
        let report = analyze_mesh(
            &cube(10.0, [0.0, 0.0, 0.0]),
            ValidationOptions {
                density_g_cm3: Some(1.24),
                ..ValidationOptions::default()
            },
        )
        .expect("cube report");

        assert!(report.solid_geometry);
        assert!(report.printable);
        assert_eq!(report.topology.connected_components, 1);
        assert_eq!(report.topology.boundary_edges, Some(0));
        assert!(report.topology.watertight);
        assert!(report.topology.consistently_oriented);
        assert_eq!(report.topology.degenerate_triangles, 0);
        assert_eq!(report.topology.duplicate_triangles, 0);
        assert_eq!(report.bounds.minimum_mm, [0.0, 0.0, 0.0]);
        assert_eq!(report.bounds.maximum_mm, [10.0, 10.0, 10.0]);
        assert_eq!(report.bounds.dimensions_mm, [10.0, 10.0, 10.0]);
        assert!((report.surface_area_mm2 - 600.0).abs() < 1e-9);
        assert!(report.issues.is_empty());
        let properties = report.solid_properties.expect("solid properties");
        assert!((properties.volume_mm3 - 1000.0).abs() < 1e-9);
        assert!((properties.surface_area_mm2 - 600.0).abs() < 1e-9);
        for coordinate in properties.center_of_mass_mm {
            assert!((coordinate - 5.0).abs() < 1e-12);
        }
        assert!((properties.mass_g.expect("mass") - 1.24).abs() < 1e-12);
        assert_eq!(report.overhang.bed_contact.faces, 2);
        assert!((report.overhang.bed_contact.area_mm2 - 100.0).abs() < 1e-9);
        assert_eq!(report.overhang.supported.faces, 12);
        assert!((report.overhang.supported.area_mm2 - 600.0).abs() < 1e-9);
        assert_eq!(report.overhang.warning.faces, 0);
        assert_eq!(report.overhang.severe.faces, 0);
        assert!(!report.overhang.requires_support);
    }

    #[test]
    fn thin_valid_solid_does_not_imply_qualified_walls_or_physical_performance() {
        let mut mesh = cube(10.0, [0.0; 3]);
        for vertex in &mut mesh.vertices {
            vertex[2] *= 0.01;
        }
        let report = analyze_mesh(&mesh, ValidationOptions::default()).unwrap();
        assert!(report.solid_geometry);
        assert!(report.printable);
        assert_eq!(report.assessment.status, AssessmentStatus::Incomplete);
        assert_eq!(
            report.assessment.criteria["solid_topology"].status,
            CriterionStatus::Passed
        );
        assert_eq!(
            report.assessment.criteria["finite_dimensions"].status,
            CriterionStatus::Passed
        );
        assert_eq!(
            report.assessment.criteria["wall_thickness"].status,
            CriterionStatus::Unmeasured
        );
        assert_eq!(
            report.assessment.criteria["physical_performance"].status,
            CriterionStatus::PhysicalTestRequired
        );
        assert_eq!(report.assessment.criteria["wall_thickness"].evidence, None);
    }

    #[test]
    fn valid_tilted_solid_retains_overhang_failure_and_warning() {
        let mut mesh = cube(10.0, [0.0; 3]);
        let (sin, cos) = 60.0_f64.to_radians().sin_cos();
        for vertex in &mut mesh.vertices {
            let [x, y, z] = *vertex;
            *vertex = [cos * x - sin * z, y, sin * x + cos * z];
        }
        let report = analyze_mesh(&mesh, ValidationOptions::default()).unwrap();
        assert!(report.solid_geometry);
        assert!(report.printable);
        assert!(report.overhang.requires_support);
        assert_eq!(report.assessment.status, AssessmentStatus::Failed);
        assert_eq!(
            report.assessment.criteria["support_free_orientation"].status,
            CriterionStatus::Failed
        );
        assert_eq!(
            report.assessment.criteria["support_free_orientation"].evidence,
            Some("/overhang")
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "support_recommended")
        );
    }

    #[test]
    fn open_cube_reports_boundary_edges_and_no_solid_properties() {
        let mut mesh = cube(10.0, [0.0, 0.0, 0.0]);
        mesh.triangles.drain(2..4);
        let report = analyze_mesh(&mesh, ValidationOptions::default()).expect("open report");

        assert!(!report.solid_geometry);
        assert_eq!(report.assessment.status, AssessmentStatus::Failed);
        assert_eq!(
            report.assessment.criteria["solid_topology"].status,
            CriterionStatus::Failed
        );
        assert!(!report.topology.watertight);
        assert!(report.topology.consistently_oriented);
        assert_eq!(report.topology.boundary_edges, Some(4));
        assert!(report.solid_properties.is_none());
        assert!(report.issues.iter().any(|issue| issue.code == "open_mesh"));
        assert!(
            !report
                .issues
                .iter()
                .any(|issue| issue.code == "non_positive_volume")
        );
    }

    #[test]
    fn inward_winding_is_actionable() {
        let mut mesh = cube(2.0, [0.0, 0.0, 0.0]);
        for triangle in &mut mesh.triangles {
            triangle.swap(1, 2);
        }
        let report = analyze_mesh(&mesh, ValidationOptions::default()).expect("inward report");

        assert!(report.signed_volume_mm3 < 0.0);
        assert!(!report.solid_geometry);
        assert!(!report.topology.consistently_oriented);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "inconsistent_shell_orientation")
        );
    }

    #[test]
    fn disconnected_solids_are_valid_with_a_warning() {
        let mut mesh = cube(1.0, [0.0, 0.0, 0.0]);
        let second = cube(1.0, [3.0, 0.0, 0.0]);
        let offset = mesh.vertices.len() as u32;
        mesh.vertices.extend(second.vertices);
        mesh.triangles.extend(
            second
                .triangles
                .into_iter()
                .map(|triangle| triangle.map(|index| index + offset)),
        );
        let report = analyze_mesh(&mesh, ValidationOptions::default()).expect("multipart report");

        assert!(report.solid_geometry);
        assert_eq!(report.topology.connected_components, 2);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "multiple_components")
        );
    }

    #[test]
    fn inward_component_is_not_hidden_by_larger_outward_component() {
        let mut mesh = cube(2.0, [0.0, 0.0, 0.0]);
        let mut inward = cube(1.0, [4.0, 0.0, 0.0]);
        for triangle in &mut inward.triangles {
            triangle.swap(1, 2);
        }
        let offset = mesh.vertices.len() as u32;
        mesh.vertices.extend(inward.vertices);
        mesh.triangles.extend(
            inward
                .triangles
                .into_iter()
                .map(|triangle| triangle.map(|index| index + offset)),
        );

        let report = analyze_mesh(&mesh, ValidationOptions::default()).expect("mixed winding");

        assert!((report.signed_volume_mm3 - 7.0).abs() < 1e-12);
        assert_eq!(report.topology.connected_components, 2);
        assert!(!report.topology.consistently_oriented);
        assert!(!report.solid_geometry);
        assert!(report.issues.iter().any(|issue| {
            issue.code == "inconsistent_shell_orientation"
                && issue.message.contains("1 connected surface shells")
        }));
    }

    #[test]
    fn hollow_solid_accepts_an_inward_cavity_shell() {
        let hollow = &Manifold::cube(3.0, 3.0, 3.0, true) - &Manifold::cube(1.0, 1.0, 1.0, true);
        let mesh = mesh_from_manifold(&hollow);

        let report = analyze_mesh(&mesh, ValidationOptions::default()).expect("hollow report");

        assert_eq!(report.topology.connected_components, 2);
        assert!(report.topology.consistently_oriented);
        assert!(report.solid_geometry);
        assert!(report.printable);
        assert!((report.solid_properties.expect("properties").volume_mm3 - 26.0).abs() < 1e-9);
        assert!(!report.issues.iter().any(|issue| {
            issue.code == "inconsistent_shell_orientation" || issue.code == "multiple_components"
        }));
    }

    #[test]
    fn nested_material_island_uses_alternating_shell_orientation() {
        let mut mesh = cube(4.0, [0.0, 0.0, 0.0]);
        let mut cavity = cube(2.0, [1.0, 1.0, 1.0]);
        for triangle in &mut cavity.triangles {
            triangle.swap(1, 2);
        }
        append_mesh(&mut mesh, cavity);
        append_mesh(&mut mesh, cube(0.5, [1.75, 1.75, 1.75]));

        let report = analyze_mesh(&mesh, ValidationOptions::default()).expect("nested report");

        assert_eq!(report.topology.connected_components, 3);
        assert!(report.topology.consistently_oriented);
        assert!(report.solid_geometry);
        assert!((report.solid_properties.expect("properties").volume_mm3 - 56.125).abs() < 1e-9);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "multiple_components")
        );
    }

    #[test]
    fn translated_build_plate_is_still_bed_contact() {
        let report = analyze_mesh(&cube(1.0, [3.0, -4.0, 2.0]), ValidationOptions::default())
            .expect("translated cube");

        assert_eq!(report.overhang.bed_contact.faces, 2);
        assert!(!report.overhang.requires_support);
    }

    #[test]
    fn translation_and_scale_obey_mass_property_invariants() {
        let base = analyze_mesh(&cube(1.0, [0.0, 0.0, 0.0]), ValidationOptions::default())
            .expect("base")
            .solid_properties
            .expect("base properties");
        let translated = analyze_mesh(&cube(1.0, [1e6, -2e6, 3e6]), ValidationOptions::default())
            .expect("translated")
            .solid_properties
            .expect("translated properties");
        let scaled = analyze_mesh(&cube(2.0, [0.0, 0.0, 0.0]), ValidationOptions::default())
            .expect("scaled")
            .solid_properties
            .expect("scaled properties");

        assert!((translated.volume_mm3 - base.volume_mm3).abs() < 1e-9);
        assert!((translated.surface_area_mm2 - base.surface_area_mm2).abs() < 1e-9);
        assert!((scaled.volume_mm3 - base.volume_mm3 * 8.0).abs() < 1e-9);
        assert!((scaled.surface_area_mm2 - base.surface_area_mm2 * 4.0).abs() < 1e-9);
    }

    #[test]
    fn floating_downward_face_requires_support() {
        let mesh = TriangleMesh {
            vertices: vec![
                [0.0, 0.0, 1.0],
                [0.0, 1.0, 1.0],
                [1.0, 1.0, 1.0],
                [1.0, 0.0, 1.0],
                [0.0, 0.0, 0.0],
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3], [0, 4, 3]],
        };
        let report = analyze_mesh(&mesh, ValidationOptions::default()).expect("overhang report");

        assert!(report.overhang.requires_support);
        assert_eq!(report.overhang.severe.faces, 2);
        assert!((report.overhang.severe.area_mm2 - 1.0).abs() < 1e-12);
        assert_eq!(report.overhang.warning.faces, 0);
        assert_eq!(report.overhang.bed_contact.faces, 0);
    }

    #[test]
    fn overhang_classification_is_translation_invariant() {
        fn floating_face(z: f64) -> TriangleMesh {
            TriangleMesh {
                vertices: vec![
                    [0.0, 0.0, z],
                    [0.0, 0.0, z + 0.5],
                    [0.0, 1.0, z + 0.5],
                    [1.0, 1.0, z + 0.5],
                ],
                triangles: vec![[1, 2, 3]],
            }
        }

        let origin = analyze_mesh(&floating_face(0.0), ValidationOptions::default())
            .expect("origin report")
            .overhang;
        let translated = analyze_mesh(&floating_face(1_000_000.0), ValidationOptions::default())
            .expect("translated report")
            .overhang;

        assert_eq!(translated.bed_contact, origin.bed_contact);
        assert_eq!(translated.supported, origin.supported);
        assert_eq!(translated.warning, origin.warning);
        assert_eq!(translated.severe, origin.severe);
        assert_eq!(translated.requires_support, origin.requires_support);
        assert_eq!(translated.bed_contact.faces, 0);
        assert_eq!(translated.severe.faces, 1);
        assert!(translated.requires_support);
    }

    #[test]
    fn maximum_and_tiny_finite_build_directions_normalize() {
        for direction in [[f64::MAX; 3], [1e-300, 0.0, 0.0]] {
            let report = analyze_mesh(
                &cube(1.0, [0.0, 0.0, 0.0]),
                ValidationOptions {
                    build_direction: direction,
                    ..ValidationOptions::default()
                },
            )
            .expect("finite direction");
            let length = report
                .overhang
                .build_direction
                .iter()
                .map(|component| component * component)
                .sum::<f64>()
                .sqrt();
            assert!((length - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn geometry_error_codes_are_stable() {
        let errors = [
            (GeometryError::InvalidStl("bad".to_string()), "invalid_stl"),
            (GeometryError::EmptyMesh, "empty_mesh"),
            (
                GeometryError::NonFiniteVertex { index: 3 },
                "non_finite_geometry",
            ),
            (
                GeometryError::InvalidIndex { triangle: 2 },
                "invalid_mesh_index",
            ),
            (GeometryError::TooManyVertices, "mesh_too_large"),
            (
                GeometryError::InvalidOptions("bad".to_string()),
                "validation",
            ),
            (GeometryError::Kernel("bad".to_string()), "geometry"),
            (
                GeometryError::InvalidAssemblyPart {
                    part: "fixed",
                    reason: "bad".to_string(),
                },
                "invalid_assembly_part",
            ),
        ];

        for (error, code) in errors {
            assert_eq!(error.code(), code);
        }
    }

    #[test]
    fn invalid_mesh_shape_and_options_fail_before_analysis() {
        let empty_vertices = TriangleMesh {
            vertices: Vec::new(),
            triangles: vec![[0, 0, 0]],
        };
        let empty_triangles = TriangleMesh {
            vertices: vec![[0.0, 0.0, 0.0]],
            triangles: Vec::new(),
        };
        for mesh in [&empty_vertices, &empty_triangles] {
            assert_eq!(
                analyze_mesh(mesh, ValidationOptions::default())
                    .expect_err("empty mesh")
                    .code(),
                "empty_mesh"
            );
        }

        let mesh = cube(1.0, [0.0, 0.0, 0.0]);
        for build_direction in [
            [0.0, 0.0, 0.0],
            [f64::NAN, 0.0, 1.0],
            [0.0, f64::INFINITY, 1.0],
        ] {
            let error = analyze_mesh(
                &mesh,
                ValidationOptions {
                    build_direction,
                    ..ValidationOptions::default()
                },
            )
            .expect_err("invalid direction");
            assert_eq!(error.code(), "validation");
        }
        for overhang_angle_degrees in [f64::NAN, -0.1, 90.1] {
            let error = analyze_mesh(
                &mesh,
                ValidationOptions {
                    overhang_angle_degrees,
                    ..ValidationOptions::default()
                },
            )
            .expect_err("invalid angle");
            assert_eq!(error.code(), "validation");
        }
        for density_g_cm3 in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let error = analyze_mesh(
                &mesh,
                ValidationOptions {
                    density_g_cm3: Some(density_g_cm3),
                    ..ValidationOptions::default()
                },
            )
            .expect_err("invalid density");
            assert_eq!(error.code(), "validation");
        }
    }

    #[test]
    fn invalid_vertices_and_indices_report_their_location() {
        for (vertex_index, coordinate) in [
            (0, [f64::NAN, 0.0, 0.0]),
            (2, [0.0, f64::INFINITY, 0.0]),
            (5, [0.0, 0.0, f64::NEG_INFINITY]),
        ] {
            let mut mesh = cube(1.0, [0.0, 0.0, 0.0]);
            mesh.vertices[vertex_index] = coordinate;
            assert!(matches!(
                analyze_mesh(&mesh, ValidationOptions::default()),
                Err(GeometryError::NonFiniteVertex { index }) if index == vertex_index
            ));
        }
        for (position, invalid_index) in [(0, 8), (1, 9), (2, u32::MAX)] {
            let mut mesh = cube(1.0, [0.0, 0.0, 0.0]);
            mesh.triangles[3][position] = invalid_index;
            assert!(matches!(
                analyze_mesh(&mesh, ValidationOptions::default()),
                Err(GeometryError::InvalidIndex { triangle: 3 })
            ));
        }
    }

    #[test]
    fn bounds_surface_area_and_defects_match_direct_geometry() {
        let mesh = TriangleMesh {
            vertices: vec![
                [-3.0, 5.0, 2.0],
                [1.0, -1.0, 2.0],
                [1.0, 2.0, 8.0],
                [2.0, 0.0, 0.0],
                [5.0, 0.0, 0.0],
                [2.0, 4.0, 0.0],
            ],
            triangles: vec![[3, 4, 5]],
        };
        let bounds = mesh_bounds(&mesh.vertices);
        assert_eq!(bounds.minimum_mm, [-3.0, -1.0, 0.0]);
        assert_eq!(bounds.maximum_mm, [5.0, 5.0, 8.0]);
        assert_eq!(bounds.dimensions_mm, [8.0, 6.0, 8.0]);
        assert!((mesh_surface_area(&mesh) - 6.0).abs() < 1e-12);

        let defect_mesh = TriangleMesh {
            vertices: vec![
                [1.0, 1.0, 0.0],
                [2.0, 1.0, 0.0],
                [3.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 2.0, 0.0],
            ],
            triangles: vec![
                [0, 0, 1],
                [0, 1, 0],
                [0, 1, 1],
                [0, 1, 2],
                [3, 4, 5],
                [5, 3, 4],
                [4, 5, 3],
            ],
        };
        let vertices = defect_mesh
            .vertices
            .iter()
            .map(|vertex| Vector::from_array(*vertex))
            .collect::<Vec<_>>();
        assert_eq!(mesh_defects(&defect_mesh, &vertices), (4, 3));
    }

    #[test]
    fn overhang_categories_preserve_face_counts_and_areas() {
        fn face(angle_degrees: f64, x_offset: f64) -> ([[f64; 3]; 3], [u32; 3]) {
            let radians = angle_degrees.to_radians();
            let points = [
                [x_offset, 0.0, 1.0],
                [x_offset, 2.0, 1.0],
                [x_offset + radians.sin(), 0.0, 1.0 + radians.cos()],
            ];
            (points, [0, 1, 2])
        }

        let mut mesh = TriangleMesh {
            vertices: vec![[0.0, 0.0, 0.0]],
            triangles: Vec::new(),
        };
        for (angle, offset) in [(30.0, 0.0), (50.0, 2.0), (70.0, 4.0)] {
            let (points, triangle) = face(angle, offset);
            let base = mesh.vertices.len() as u32;
            mesh.vertices.extend(points);
            mesh.triangles
                .push(triangle.map(|index| index.saturating_add(base)));
        }
        let bounds = mesh_bounds(&mesh.vertices);
        let report = analyze_overhang(&mesh, [0.0, 0.0, 1.0], 45.0, &bounds);

        assert_eq!(report.bed_contact.faces, 0);
        assert_eq!(report.supported.faces, 1);
        assert_eq!(report.warning.faces, 1);
        assert_eq!(report.severe.faces, 1);
        for category in [report.supported, report.warning, report.severe] {
            assert!((category.area_mm2 - 1.0).abs() < 1e-12);
        }
        assert!(report.requires_support);
    }

    #[test]
    fn warning_faces_alone_require_support() {
        let angle = 50.0_f64.to_radians();
        let mesh = TriangleMesh {
            vertices: vec![
                [0.0, 0.0, 0.0],
                [0.0, 0.0, 1.0],
                [0.0, 2.0, 1.0],
                [angle.sin(), 0.0, 1.0 + angle.cos()],
            ],
            triangles: vec![[1, 2, 3]],
        };
        let bounds = mesh_bounds(&mesh.vertices);
        let report = analyze_overhang(&mesh, [0.0, 0.0, 1.0], 45.0, &bounds);

        assert_eq!(report.warning.faces, 1);
        assert_eq!(report.severe.faces, 0);
        assert!(report.requires_support);
    }

    #[test]
    fn vertical_face_on_build_plane_is_not_bed_contact() {
        let mesh = TriangleMesh {
            vertices: vec![[0.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            triangles: vec![[0, 1, 2]],
        };
        let bounds = mesh_bounds(&mesh.vertices);
        let report = analyze_overhang(&mesh, [0.0, 0.0, 1.0], 45.0, &bounds);

        assert_eq!(report.bed_contact.faces, 0);
        assert_eq!(report.supported.faces, 1);
    }

    #[test]
    fn bed_tolerance_absorbs_f32_export_noise_without_hiding_floating_faces() {
        let near_bed = TriangleMesh {
            vertices: vec![
                [0.0, 0.0, 0.0],
                [0.0, 0.0, 5e-7],
                [0.0, 1.0, 5e-7],
                [1.0, 1.0, 5e-7],
            ],
            triangles: vec![[1, 2, 3]],
        };
        let bounds = mesh_bounds(&near_bed.vertices);
        let report = analyze_overhang(&near_bed, [0.0, 0.0, 1.0], 45.0, &bounds);

        assert_eq!(report.bed_contact.faces, 1);
        assert_eq!(report.severe.faces, 0);
    }

    #[test]
    fn degenerate_and_duplicate_faces_are_actionable() {
        let mut mesh = cube(1.0, [0.0, 0.0, 0.0]);
        mesh.triangles.push([0, 0, 1]);
        mesh.triangles.push(mesh.triangles[2]);
        let report = analyze_mesh(&mesh, ValidationOptions::default()).expect("defect report");

        assert_eq!(report.topology.degenerate_triangles, 1);
        assert_eq!(report.topology.duplicate_triangles, 1);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "degenerate_triangles")
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "duplicate_triangles")
        );
        assert!(!report.solid_geometry);
    }

    #[test]
    fn open_surface_does_not_invent_a_global_winding_diagnosis() {
        let mesh = TriangleMesh {
            vertices: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            triangles: vec![[0, 1, 2]],
        };
        let report = analyze_mesh(&mesh, ValidationOptions::default()).expect("flat report");

        assert!(report.topology.consistently_oriented);
        assert!(!report.solid_geometry);
        assert!(report.issues.iter().any(|issue| issue.code == "open_mesh"));
        assert!(!report.issues.iter().any(|issue| {
            issue.code == "non_positive_volume" || issue.code == "inconsistent_shell_orientation"
        }));
    }

    #[test]
    fn density_that_overflows_mass_is_rejected() {
        let error = analyze_mesh(
            &cube(10.0, [0.0, 0.0, 0.0]),
            ValidationOptions {
                density_g_cm3: Some(f64::MAX),
                ..ValidationOptions::default()
            },
        )
        .expect_err("non-finite mass must fail");

        assert_eq!(error.code(), "validation");
        assert!(
            error
                .to_string()
                .contains("mass outside the finite numeric range")
        );
    }

    #[test]
    fn non_finite_kernel_properties_are_rejected_independently() {
        for (volume, surface, center) in [
            (f64::INFINITY, 1.0, [0.0; 3]),
            (1.0, f64::NAN, [0.0; 3]),
            (1.0, 1.0, [0.0, f64::NEG_INFINITY, 0.0]),
        ] {
            let error = validate_solid_properties(volume, surface, center)
                .expect_err("non-finite property must fail");
            assert_eq!(error.code(), "geometry");
        }
    }

    #[test]
    fn invalid_stl_uses_machine_readable_error() {
        let error = analyze_stl(b"not an STL", ValidationOptions::default())
            .expect_err("invalid STL must fail");
        assert_eq!(error.code(), "invalid_stl");
    }

    #[test]
    fn binary_stl_decodes_into_the_validation_pipeline() {
        let mesh = cube(1.0, [0.0, 0.0, 0.0]);
        let triangles: Vec<_> = mesh
            .triangles
            .iter()
            .map(|triangle| stl_io::Triangle {
                normal: stl_io::Normal::new([0.0, 0.0, 0.0]),
                vertices: triangle.map(|index| {
                    stl_io::Vertex::new(mesh.vertices[index as usize].map(|value| value as f32))
                }),
            })
            .collect();
        let mut bytes = Vec::new();
        stl_io::write_stl(&mut bytes, triangles.iter()).expect("write STL");

        let report = analyze_stl(&bytes, ValidationOptions::default()).expect("analyze STL");
        assert_eq!(report.vertices, 8);
        assert_eq!(report.triangles, 12);
        assert!((report.signed_volume_mm3 - 1.0).abs() < 1e-12);
        assert!(report.printable);
    }

    #[test]
    fn inscribed_sphere_intersection_matches_analytic_volume() {
        let (volume, valid) = cube_sphere_intersection(2.0, 1.0, 256);
        assert!(valid);
        assert!((volume - 4.0 / 3.0 * std::f64::consts::PI).abs() < 0.1);
    }

    #[test]
    fn partial_overlap_is_clipped() {
        let (volume, valid) = cube_sphere_intersection(2.0, 1.5, 128);
        assert!(valid);
        assert!(volume > 4.0 && volume < 8.0);
    }
}
