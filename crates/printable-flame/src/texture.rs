//! Colour maps in FLAME's texture layout, projected from photographs.
//!
//! Each photograph comes with a head posed to match it and the orthographic
//! camera that places that head over the face. Every texel of the map is a
//! point on the head's surface. It takes the photograph's colour where that
//! point faces the camera and is not hidden by nearer surface. Photographs
//! are blended by how squarely they see each point. What no photograph sees
//! is filled smoothly across the head's surface, so the fill follows the
//! head rather than the texture layout and has no seams where the layout is
//! cut apart.
//!
//! When a photograph comes with labels saying what each pixel shows, skin
//! takes colour only from pixels of skin, so hair and clothing in front of
//! it are left out and unseen skin is filled from skin alone. The scalp can
//! take the photographs' colour, be treated as skin, or be painted a hair
//! colour where no photograph shows bare skin there.

use thiserror::Error;

use crate::UvLayout;

/// Side of the crop the camera is expressed in, in pixels.
const CROP_PIXELS: f64 = 224.0;
/// Surface turned further than this from the camera, as the cosine of the
/// angle to the viewing direction, takes no colour from that photograph.
const MIN_FACING: f32 = 0.35;
/// Points that land within this fraction of the crop of the head's outline in
/// a photograph take no colour from it: the fitted head's outline never
/// matches the real one exactly, and the background lies just beyond it.
const OUTLINE_MARGIN: f64 = 0.02;
/// Weight at which a texel's photographed colour is used unblended.
const FULL_WEIGHT: f32 = 0.5;
/// A point counts as visible when it is no more than this far, in metres,
/// behind the nearest surface at its pixel.
const DEPTH_TOLERANCE_M: f32 = 0.002;
/// Below this share of the map in view of some photograph, the projection
/// is taken to have failed. A texel is in view whether or not its colour is
/// used, so hair in front of skin does not count against the projection.
const MIN_SEEN: f64 = 0.02;
/// Sweeps of the surface fill. Over-relaxation makes each sweep carry colour
/// several edges, so this reaches across the whole head.
const FILL_SWEEPS: usize = 3000;
const OVER_RELAXATION: f32 = 1.8;
const NO_FACE: u32 = u32::MAX;

/// Pixel labels, as MediaPipe's multiclass selfie segmenter numbers them.
pub const LABEL_BACKGROUND: u8 = 0;
pub const LABEL_HAIR: u8 = 1;
pub const LABEL_BODY_SKIN: u8 = 2;
pub const LABEL_FACE_SKIN: u8 = 3;
pub const LABEL_CLOTHES: u8 = 4;
pub const LABEL_OTHER: u8 = 5;

/// Which part of the head a triangle belongs to, which decides the pixels
/// it may take colour from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaceKind {
    Skin,
    Scalp,
    Eye,
}

/// How the scalp is coloured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScalpColour {
    /// From whatever the photographs show there, hair included.
    Photo,
    /// As skin: from pixels of skin only, and filled from skin.
    Skin,
    /// From pixels of skin where the photographs show bare scalp, and this
    /// sRGB colour elsewhere.
    Hair([u8; 3]),
}

/// A photograph's pixel labels over a square: label pixel p lies at
/// photograph pixel `p / step + (left, top)`.
pub struct Labels<'a> {
    pub side: usize,
    pub data: &'a [u8],
    pub left: f64,
    pub top: f64,
    pub step: f64,
}

impl Labels<'_> {
    /// Whether `accepts` holds at a photograph pixel position and one label
    /// pixel to each side of it. Pixels at the edge of a labelled region mix
    /// what lies on either side, such as strands of hair over skin.
    fn agree(&self, pixel: [f64; 2], accepts: impl Fn(u8) -> bool) -> bool {
        let reach = 1.0 / self.step;
        [
            [0.0, 0.0],
            [reach, 0.0],
            [-reach, 0.0],
            [0.0, reach],
            [0.0, -reach],
        ]
        .iter()
        .all(|offset| {
            self.at([pixel[0] + offset[0], pixel[1] + offset[1]])
                .is_some_and(&accepts)
        })
    }

    /// The label at a photograph pixel position, or `None` outside the square.
    fn at(&self, pixel: [f64; 2]) -> Option<u8> {
        let column = ((pixel[0] - self.left) * self.step).round();
        let row = ((pixel[1] - self.top) * self.step).round();
        let side = self.side as f64;
        if !(0.0..side).contains(&column) || !(0.0..side).contains(&row) {
            return None;
        }
        self.data
            .get(row as usize * self.side + column as usize)
            .copied()
    }
}

impl FaceKind {
    /// Whether a pixel with this label may colour this part of the head.
    fn accepts(self, label: u8, scalp: ScalpColour) -> bool {
        let skin = matches!(label, LABEL_BODY_SKIN | LABEL_FACE_SKIN);
        match self {
            Self::Skin => skin,
            Self::Scalp => matches!(scalp, ScalpColour::Photo) || skin,
            Self::Eye => !matches!(label, LABEL_BACKGROUND | LABEL_HAIR | LABEL_CLOTHES),
        }
    }
}

/// An RGB photograph, three bytes per pixel, row by row from the top.
pub struct Photo<'a> {
    pub width: usize,
    pub height: usize,
    pub rgb: &'a [u8],
}

/// How a head fitted to a photograph lands in it. A point (x, y, z) in metres
/// is at `scale × (x + offset[0], y + offset[1])` in a 224 pixel crop whose
/// coordinates run from -1 to 1 with y up. Crop pixel p is photograph pixel
/// `p / crop_step + (crop_left, crop_top)`.
#[derive(Debug, Clone, Copy)]
pub struct Projection {
    pub scale: f32,
    pub offset: [f32; 2],
    pub crop_left: f64,
    pub crop_top: f64,
    pub crop_step: f64,
}

impl Projection {
    /// Photograph pixel coordinates, with pixel centres on integers, and a
    /// depth that grows toward the camera.
    fn project(&self, point: [f32; 3]) -> ([f64; 2], f32) {
        let x = f64::from(self.scale) * f64::from(point[0] + self.offset[0]);
        let y = f64::from(self.scale) * f64::from(point[1] + self.offset[1]);
        let crop = [(x + 1.0) * CROP_PIXELS / 2.0, (1.0 - y) * CROP_PIXELS / 2.0];
        (
            [
                crop[0] / self.crop_step + self.crop_left,
                crop[1] / self.crop_step + self.crop_top,
            ],
            self.scale * point[2],
        )
    }
}

/// One photograph with the head posed to match it.
pub struct View<'a> {
    pub photo: Photo<'a>,
    /// Posed head vertices in metres, one per model vertex.
    pub vertices: &'a [[f32; 3]],
    pub projection: Projection,
    /// What each pixel around the face shows. Without labels every visible
    /// pixel may colour the head.
    pub labels: Option<Labels<'a>>,
}

/// A square RGB colour map, row 0 at v = 1 and column 0 at u = 0.
#[derive(Debug, Clone)]
pub struct ColourMap {
    pub size: usize,
    pub rgb: Vec<u8>,
    /// Share of the map's texels on the head's surface that some photograph
    /// saw directly.
    pub seen: f64,
}

#[derive(Debug, Error, PartialEq)]
pub enum TextureError {
    #[error("the colour map size must be at least 2 texels")]
    Size,
    #[error("a view does not have one vertex per model vertex")]
    Vertices,
    #[error("a photograph's pixels do not match its size")]
    Photo,
    #[error("the camera for a photograph is not usable")]
    Projection,
    #[error("too little of the head was visible in the photographs to colour it")]
    NotVisible,
    #[error("the head's parts do not match its triangles")]
    Kinds,
    #[error("a photograph's labels do not match their size")]
    Labels,
}

/// Project the photographs onto the head and fill what they do not show.
pub fn bake(
    faces: &[[u32; 3]],
    layout: &UvLayout,
    views: &[View<'_>],
    size: usize,
) -> Result<ColourMap, TextureError> {
    let kinds = vec![FaceKind::Skin; faces.len()];
    let mut builder = ColourMapBuilder::new(faces, layout, size, &kinds, ScalpColour::Photo)?;
    for view in views {
        builder.add(view)?;
    }
    builder.finish()
}

/// A colour map built one photograph at a time, so that only one photograph
/// needs to be held at once.
pub struct ColourMapBuilder<'a> {
    faces: &'a [[u32; 3]],
    texels: TexelMap,
    weight: Vec<f32>,
    /// Texels some photograph had in view, whether or not it coloured them.
    in_view: Vec<bool>,
    /// Texels that took colour from a pixel labelled skin.
    skin_seen: Vec<bool>,
    colour: Vec<[f32; 3]>,
    size: usize,
    kinds: Vec<FaceKind>,
    scalp: ScalpColour,
}

impl<'a> ColourMapBuilder<'a> {
    /// A builder for a `size` texel map. `kinds` names the part of the head
    /// each triangle belongs to, and `scalp` says how the scalp is coloured.
    pub fn new(
        faces: &'a [[u32; 3]],
        layout: &UvLayout,
        size: usize,
        kinds: &[FaceKind],
        scalp: ScalpColour,
    ) -> Result<Self, TextureError> {
        if size < 2 {
            return Err(TextureError::Size);
        }
        if kinds.len() != faces.len() {
            return Err(TextureError::Kinds);
        }
        Ok(Self {
            faces,
            texels: TexelMap::new(faces, layout, size),
            weight: vec![0.0; size * size],
            in_view: vec![false; size * size],
            skin_seen: vec![false; size * size],
            colour: vec![[0.0; 3]; size * size],
            size,
            kinds: kinds.to_vec(),
            scalp,
        })
    }

    /// Add one photograph's colour to the texels it sees.
    pub fn add(&mut self, view: &View<'_>) -> Result<(), TextureError> {
        accumulate(
            self.faces,
            &self.texels,
            view,
            (&self.kinds, self.scalp),
            &mut self.weight,
            &mut self.colour,
            (&mut self.in_view, &mut self.skin_seen),
        )
    }

    /// Blend the photographs and fill what none of them saw.
    pub fn finish(self) -> Result<ColourMap, TextureError> {
        let Self {
            faces,
            texels,
            weight,
            in_view,
            skin_seen,
            mut colour,
            size,
            kinds,
            scalp,
        } = self;
        let in_view =
            in_view.iter().filter(|&&seen| seen).count() as f64 / texels.on_surface().max(1) as f64;
        if in_view < MIN_SEEN {
            return Err(TextureError::NotVisible);
        }
        finish(
            faces,
            &texels,
            (&weight, &skin_seen),
            &mut colour,
            size,
            &kinds,
            scalp,
        )
    }
}

/// Parts of a mesh separate from its largest connected part, triangle by
/// triangle. FLAME's eyeballs are separate from its head.
pub fn minor_parts(faces: &[[u32; 3]]) -> Vec<bool> {
    let vertex_count = faces
        .iter()
        .flatten()
        .map(|&vertex| vertex as usize + 1)
        .max()
        .unwrap_or(0);
    let mut parent: Vec<usize> = (0..vertex_count).collect();
    fn root(parent: &mut [usize], mut index: usize) -> usize {
        while parent[index] != index {
            parent[index] = parent[parent[index]];
            index = parent[index];
        }
        index
    }
    for face in faces {
        for pair in [[face[0], face[1]], [face[1], face[2]]] {
            let (a, b) = (
                root(&mut parent, pair[0] as usize),
                root(&mut parent, pair[1] as usize),
            );
            parent[a] = b;
        }
    }
    let roots: Vec<usize> = faces
        .iter()
        .map(|face| root(&mut parent, face[0] as usize))
        .collect();
    let mut sizes = std::collections::HashMap::new();
    for &part in &roots {
        *sizes.entry(part).or_insert(0usize) += 1;
    }
    let largest = sizes
        .iter()
        .max_by_key(|(part, size)| (**size, std::cmp::Reverse(**part)))
        .map(|(part, _)| *part);
    roots.iter().map(|part| Some(*part) != largest).collect()
}

fn finish(
    faces: &[[u32; 3]],
    texels: &TexelMap,
    (weight, skin_seen): (&[f32], &[bool]),
    colour: &mut [[f32; 3]],
    size: usize,
    kinds: &[FaceKind],
    scalp: ScalpColour,
) -> Result<ColourMap, TextureError> {
    let mut on_surface = 0usize;
    let mut seen = 0usize;
    for (index, face) in texels.face.iter().enumerate() {
        if *face != NO_FACE {
            on_surface += 1;
            if weight[index] > 0.0 {
                seen += 1;
            }
        }
    }
    let seen = seen as f64 / on_surface.max(1) as f64;
    for (colour, weight) in colour.iter_mut().zip(weight) {
        if *weight > 0.0 {
            *colour = colour.map(|channel| channel / weight);
        }
    }
    let vertex_count = faces
        .iter()
        .flatten()
        .map(|&vertex| vertex as usize + 1)
        .max()
        .unwrap_or(0);
    let on_scalp = |face: u32| kinds[face as usize] == FaceKind::Scalp;
    // Skin and eyes are filled from what the photographs showed of them.
    // A scalp taken as photographed shows hair, so it seeds its own fill
    // instead and does not carry hair into unseen skin, except where it was
    // photographed as bare skin.
    let photographed_scalp = scalp == ScalpColour::Photo;
    let skin_weight: Vec<f32> = texels
        .face
        .iter()
        .zip(weight)
        .zip(skin_seen)
        .map(|((&face, &weight), &skin)| {
            if face != NO_FACE && photographed_scalp && on_scalp(face) && !skin {
                0.0
            } else {
                weight
            }
        })
        .collect();
    let fill = surface_fill(faces, vertex_count, texels, &skin_weight, colour);
    let scalp_fill =
        photographed_scalp.then(|| surface_fill(faces, vertex_count, texels, weight, colour));
    let paint = match scalp {
        ScalpColour::Hair(hair) => Some(hair.map(f32::from)),
        ScalpColour::Photo | ScalpColour::Skin => None,
    };
    // On the surface, each texel keeps its photographed colour in proportion
    // to how well it was seen; texels off the surface border the islands so
    // that filtering near their edges picks up matching colour.
    let mut confidence = vec![0.0f32; size * size];
    for (index, &face) in texels.face.iter().enumerate() {
        if face == NO_FACE {
            continue;
        }
        let corners = faces[face as usize];
        let [wa, wb] = texels.barycentric[index];
        let shares = [wa, wb, 1.0 - wa - wb];
        let source = match &scalp_fill {
            Some(scalp_fill) if on_scalp(face) => scalp_fill,
            _ => &fill,
        };
        let mut filled = [0.0f32; 3];
        for (corner, share) in corners.iter().zip(shares) {
            for channel in 0..3 {
                filled[channel] += share * source[*corner as usize][channel];
            }
        }
        let mut keep = (weight[index] / FULL_WEIGHT).min(1.0);
        // A painted scalp shows its paint wherever no photograph saw bare
        // scalp, and the photographed colour in full wherever one did.
        if let Some(paint) = paint.filter(|_| on_scalp(face)) {
            filled = paint;
            if weight[index] > 0.0 {
                keep = 1.0;
            }
        }
        for channel in 0..3 {
            colour[index][channel] = keep * colour[index][channel] + (1.0 - keep) * filled[channel];
        }
        confidence[index] = 1.0;
    }
    let filled = pull_push(size, confidence, colour.to_vec());
    let rgb = filled
        .iter()
        .flat_map(|texel| texel.map(|channel| channel.round().clamp(0.0, 255.0) as u8))
        .collect();
    Ok(ColourMap { size, rgb, seen })
}

/// A colour for every vertex: photographed colour where the vertex was seen
/// well, and elsewhere the smooth blend of its neighbours, so unseen surface
/// takes the colours around it.
fn surface_fill(
    faces: &[[u32; 3]],
    vertex_count: usize,
    texels: &TexelMap,
    weight: &[f32],
    colour: &[[f32; 3]],
) -> Vec<[f32; 3]> {
    // Each texel shares its colour among its triangle's corners.
    let mut seen_weight = vec![0.0f32; vertex_count];
    let mut seen_colour = vec![[0.0f32; 3]; vertex_count];
    let mut reach = vec![0.0f32; vertex_count];
    for (index, &face) in texels.face.iter().enumerate() {
        if face == NO_FACE {
            continue;
        }
        let [wa, wb] = texels.barycentric[index];
        for (corner, share) in faces[face as usize].iter().zip([wa, wb, 1.0 - wa - wb]) {
            let vertex = *corner as usize;
            reach[vertex] += share;
            let amount = share * weight[index];
            seen_weight[vertex] += amount;
            for channel in 0..3 {
                seen_colour[vertex][channel] += amount * colour[index][channel];
            }
        }
    }
    let mut neighbours = vec![Vec::new(); vertex_count];
    for face in faces {
        for (a, b) in [(face[0], face[1]), (face[1], face[2]), (face[2], face[0])] {
            let (a, b) = (a as usize, b as usize);
            if !neighbours[a].contains(&b) {
                neighbours[a].push(b);
                neighbours[b].push(a);
            }
        }
    }
    let mut confidence = vec![0.0f32; vertex_count];
    let mut total = [0.0f64; 3];
    let mut known = 0usize;
    for vertex in 0..vertex_count {
        if seen_weight[vertex] > 0.0 {
            seen_colour[vertex] = seen_colour[vertex].map(|channel| channel / seen_weight[vertex]);
            confidence[vertex] =
                (seen_weight[vertex] / reach[vertex].max(1e-6) / FULL_WEIGHT).min(1.0);
            for channel in 0..3 {
                total[channel] += f64::from(seen_colour[vertex][channel]);
            }
            known += 1;
        }
    }
    let mean = if known == 0 {
        [128.0; 3]
    } else {
        total.map(|channel| (channel / known as f64) as f32)
    };
    let mut fill: Vec<[f32; 3]> = (0..vertex_count)
        .map(|vertex| {
            if confidence[vertex] > 0.0 {
                seen_colour[vertex]
            } else {
                mean
            }
        })
        .collect();
    for _ in 0..FILL_SWEEPS {
        for vertex in 0..vertex_count {
            let keep = confidence[vertex];
            if keep >= 1.0 || neighbours[vertex].is_empty() {
                continue;
            }
            let mut around = [0.0f32; 3];
            for &neighbour in &neighbours[vertex] {
                for channel in 0..3 {
                    around[channel] += fill[neighbour][channel];
                }
            }
            let count = neighbours[vertex].len() as f32;
            for channel in 0..3 {
                let target =
                    keep * seen_colour[vertex][channel] + (1.0 - keep) * around[channel] / count;
                fill[vertex][channel] += OVER_RELAXATION * (target - fill[vertex][channel]);
            }
        }
    }
    fill
}

/// For each texel, the triangle whose texture coordinates cover its centre
/// and its barycentric coordinates there.
struct TexelMap {
    face: Vec<u32>,
    barycentric: Vec<[f32; 2]>,
}

impl TexelMap {
    /// Texels on the head's surface.
    fn on_surface(&self) -> usize {
        self.face.iter().filter(|&&face| face != NO_FACE).count()
    }

    fn new(faces: &[[u32; 3]], layout: &UvLayout, size: usize) -> Self {
        let mut face_of = vec![NO_FACE; size * size];
        let mut barycentric = vec![[0.0f32; 2]; size * size];
        let side = size as f64;
        for (index, corners) in layout.faces.iter().enumerate().take(faces.len()) {
            let [a, b, c] = corners.map(|corner| {
                let [u, v] = layout.coords[corner as usize];
                [f64::from(u) * side, (1.0 - f64::from(v)) * side]
            });
            let area = edge(a, b, c);
            if area.abs() < 1e-12 {
                continue;
            }
            let low = [a[0].min(b[0]).min(c[0]), a[1].min(b[1]).min(c[1])];
            let high = [a[0].max(b[0]).max(c[0]), a[1].max(b[1]).max(c[1])];
            let columns = texel_range(low[0], high[0], size);
            let rows = texel_range(low[1], high[1], size);
            for row in rows {
                for column in columns.clone() {
                    let centre = [column as f64 + 0.5, row as f64 + 0.5];
                    let wa = edge(b, c, centre) / area;
                    let wb = edge(c, a, centre) / area;
                    let wc = 1.0 - wa - wb;
                    if wa >= -1e-9 && wb >= -1e-9 && wc >= -1e-9 {
                        let at = row * size + column;
                        face_of[at] = index as u32;
                        barycentric[at] = [wa as f32, wb as f32];
                    }
                }
            }
        }
        Self {
            face: face_of,
            barycentric,
        }
    }
}

fn edge(a: [f64; 2], b: [f64; 2], point: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (point[1] - a[1]) - (b[1] - a[1]) * (point[0] - a[0])
}

/// Texel indices whose centres may lie between `low` and `high`.
fn texel_range(low: f64, high: f64, size: usize) -> std::ops::Range<usize> {
    let first = (low - 0.5).floor().max(0.0) as usize;
    let last = ((high - 0.5).ceil().max(-1.0) + 1.0) as usize;
    first.min(size)..last.min(size)
}

/// Add one photograph's colour to the texels it sees, weighted by how squarely
/// it sees them.
fn accumulate(
    faces: &[[u32; 3]],
    texels: &TexelMap,
    view: &View<'_>,
    (kinds, scalp): (&[FaceKind], ScalpColour),
    weight: &mut [f32],
    colour: &mut [[f32; 3]],
    (in_view, skin_seen): (&mut [bool], &mut [bool]),
) -> Result<(), TextureError> {
    let photo = &view.photo;
    if photo.rgb.len() != photo.width * photo.height * 3 || photo.width == 0 || photo.height == 0 {
        return Err(TextureError::Photo);
    }
    if let Some(labels) = &view.labels
        && (labels.data.len() != labels.side * labels.side
            || !(labels.step.is_finite()
                && labels.step > 0.0
                && labels.left.is_finite()
                && labels.top.is_finite()))
    {
        return Err(TextureError::Labels);
    }
    let projection = view.projection;
    if !(projection.scale.is_finite()
        && projection.scale > 0.0
        && projection.offset.iter().all(|value| value.is_finite())
        && projection.crop_step.is_finite()
        && projection.crop_step > 0.0
        && projection.crop_left.is_finite()
        && projection.crop_top.is_finite())
    {
        return Err(TextureError::Projection);
    }
    let vertex_count = view.vertices.len();
    if faces
        .iter()
        .flatten()
        .any(|&index| index as usize >= vertex_count)
    {
        return Err(TextureError::Vertices);
    }
    let projected: Vec<([f64; 2], f32)> = view
        .vertices
        .iter()
        .map(|vertex| projection.project(*vertex))
        .collect();
    let depth = DepthBuffer::render(faces, &projected, photo.width, photo.height);
    let normals = vertex_normals(faces, view.vertices);
    let tolerance = projection.scale * DEPTH_TOLERANCE_M;
    // One pixel more, because probes are read at the nearest pixel centre.
    let margin = OUTLINE_MARGIN * CROP_PIXELS / projection.crop_step + 1.0;
    for (index, &face) in texels.face.iter().enumerate() {
        if face == NO_FACE {
            continue;
        }
        let corners = faces[face as usize].map(|vertex| vertex as usize);
        let [wa, wb] = texels.barycentric[index];
        let weights = [wa, wb, 1.0 - wa - wb];
        let blend = |values: &dyn Fn(usize) -> [f32; 3]| -> [f32; 3] {
            let mut sum = [0.0f32; 3];
            for (corner, share) in corners.iter().zip(weights) {
                let value = values(*corner);
                for axis in 0..3 {
                    sum[axis] += share * value[axis];
                }
            }
            sum
        };
        let normal = blend(&|vertex| normals[vertex]);
        let length = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
        if length == 0.0 {
            continue;
        }
        // The camera looks along -z, so surface facing it has a positive z normal.
        let facing = normal[2] / length;
        if facing <= MIN_FACING {
            continue;
        }
        let point = blend(&|vertex| view.vertices[vertex]);
        let (pixel, point_depth) = projection.project(point);
        let Some(nearest) = depth.at(pixel) else {
            continue;
        };
        if point_depth < nearest - tolerance {
            continue;
        }
        // Eight probes around a circle of the margin's radius; the octagon
        // they span reaches within 8% of the circle in every direction.
        let near_outline = (0..8).any(|step| {
            let angle = f64::from(step) * std::f64::consts::FRAC_PI_4;
            depth
                .at([
                    pixel[0] + margin * angle.cos(),
                    pixel[1] + margin * angle.sin(),
                ])
                .is_none_or(|depth| depth == f32::NEG_INFINITY)
        });
        if near_outline {
            continue;
        }
        in_view[index] = true;
        // Outside the labelled square nothing says what a pixel shows, so
        // it is not trusted either.
        if let Some(labels) = &view.labels {
            if !labels.agree(pixel, |label| kinds[face as usize].accepts(label, scalp)) {
                continue;
            }
            if labels
                .at(pixel)
                .is_some_and(|label| matches!(label, LABEL_BODY_SKIN | LABEL_FACE_SKIN))
            {
                skin_seen[index] = true;
            }
        }
        let Some(sample) = sample(photo, pixel) else {
            continue;
        };
        let share = ((facing - MIN_FACING) / (1.0 - MIN_FACING)).powi(2);
        weight[index] += share;
        for channel in 0..3 {
            colour[index][channel] += share * sample[channel];
        }
    }
    Ok(())
}

/// The nearest surface depth at each pixel of a photograph.
struct DepthBuffer {
    width: usize,
    height: usize,
    depth: Vec<f32>,
}

impl DepthBuffer {
    fn render(
        faces: &[[u32; 3]],
        projected: &[([f64; 2], f32)],
        width: usize,
        height: usize,
    ) -> Self {
        let mut depth = vec![f32::NEG_INFINITY; width * height];
        for face in faces {
            let [(a, da), (b, db), (c, dc)] = face.map(|vertex| projected[vertex as usize]);
            let area = edge(a, b, c);
            if area.abs() < 1e-12 {
                continue;
            }
            let low = [a[0].min(b[0]).min(c[0]), a[1].min(b[1]).min(c[1])];
            let high = [a[0].max(b[0]).max(c[0]), a[1].max(b[1]).max(c[1])];
            if high[0] < 0.0 || high[1] < 0.0 || low[0] > width as f64 || low[1] > height as f64 {
                continue;
            }
            let columns = (low[0].ceil().max(0.0) as usize)
                ..((high[0].floor() + 1.0).max(0.0) as usize).min(width);
            let rows = (low[1].ceil().max(0.0) as usize)
                ..((high[1].floor() + 1.0).max(0.0) as usize).min(height);
            for row in rows {
                for column in columns.clone() {
                    let pixel = [column as f64, row as f64];
                    let wa = edge(b, c, pixel) / area;
                    let wb = edge(c, a, pixel) / area;
                    let wc = 1.0 - wa - wb;
                    if wa >= 0.0 && wb >= 0.0 && wc >= 0.0 {
                        let value =
                            (wa * f64::from(da) + wb * f64::from(db) + wc * f64::from(dc)) as f32;
                        let slot = &mut depth[row * width + column];
                        *slot = slot.max(value);
                    }
                }
            }
        }
        Self {
            width,
            height,
            depth,
        }
    }

    /// The nearest depth at the pixel containing `pixel`, or `None` outside
    /// the photograph.
    fn at(&self, pixel: [f64; 2]) -> Option<f32> {
        let column = pixel[0].round();
        let row = pixel[1].round();
        if column < 0.0 || row < 0.0 || column >= self.width as f64 || row >= self.height as f64 {
            return None;
        }
        Some(self.depth[row as usize * self.width + column as usize])
    }
}

/// Area-weighted vertex normals.
fn vertex_normals(faces: &[[u32; 3]], vertices: &[[f32; 3]]) -> Vec<[f32; 3]> {
    let mut normals = vec![[0.0f32; 3]; vertices.len()];
    for face in faces {
        let [a, b, c] = face.map(|vertex| vertices[vertex as usize]);
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let normal = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        for vertex in face {
            for axis in 0..3 {
                normals[*vertex as usize][axis] += normal[axis];
            }
        }
    }
    normals
}

/// Bilinear colour at a pixel position, or `None` outside the photograph.
fn sample(photo: &Photo<'_>, pixel: [f64; 2]) -> Option<[f32; 3]> {
    let [x, y] = pixel;
    if x < 0.0 || y < 0.0 || x > (photo.width - 1) as f64 || y > (photo.height - 1) as f64 {
        return None;
    }
    let column = (x as usize).min(photo.width.saturating_sub(2));
    let row = (y as usize).min(photo.height.saturating_sub(2));
    let (across, down) = ((x - column as f64) as f32, (y - row as f64) as f32);
    let at = |row: usize, column: usize| {
        let start = (row.min(photo.height - 1) * photo.width + column.min(photo.width - 1)) * 3;
        [0, 1, 2].map(|channel| f32::from(photo.rgb[start + channel]))
    };
    let corners = [
        at(row, column),
        at(row, column + 1),
        at(row + 1, column),
        at(row + 1, column + 1),
    ];
    let value: [f32; 3] = std::array::from_fn(|channel| {
        let top = corners[0][channel] * (1.0 - across) + corners[1][channel] * across;
        let bottom = corners[2][channel] * (1.0 - across) + corners[3][channel] * across;
        top * (1.0 - down) + bottom * down
    });
    Some(value)
}

/// Fill texels of low confidence from coarser averages of their surroundings.
/// Each level halves the resolution, averaging colour by confidence; on the
/// way back up, each texel keeps its own colour in proportion to its
/// confidence and takes the rest from the level above.
fn pull_push(size: usize, confidence: Vec<f32>, colour: Vec<[f32; 3]>) -> Vec<[f32; 3]> {
    let mut levels = vec![(size, confidence, colour)];
    while levels.last().expect("one level").0 > 1 {
        let (side, confidence, colour) = levels.last().expect("one level");
        let coarse = side.div_ceil(2);
        let mut next_confidence = vec![0.0f32; coarse * coarse];
        let mut next_colour = vec![[0.0f32; 3]; coarse * coarse];
        for row in 0..coarse {
            for column in 0..coarse {
                let mut total = 0.0f32;
                let mut sum = [0.0f32; 3];
                for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    let (fine_row, fine_column) = (2 * row + dy, 2 * column + dx);
                    if fine_row < *side && fine_column < *side {
                        let at = fine_row * side + fine_column;
                        total += confidence[at];
                        for channel in 0..3 {
                            sum[channel] += confidence[at] * colour[at][channel];
                        }
                    }
                }
                let at = row * coarse + column;
                next_confidence[at] = total.min(1.0);
                if total > 0.0 {
                    next_colour[at] = sum.map(|channel| channel / total);
                }
            }
        }
        levels.push((coarse, next_confidence, next_colour));
    }
    // A map with no colour anywhere is mid grey.
    {
        let top = levels.last_mut().expect("one level");
        if top.1[0] == 0.0 {
            top.2[0] = [128.0; 3];
        }
    }
    for level in (0..levels.len() - 1).rev() {
        let (coarse_side, _, coarse_colour) = levels[level + 1].clone();
        let (side, confidence, colour) = &mut levels[level];
        for row in 0..*side {
            for column in 0..*side {
                let at = row * *side + column;
                let keep = confidence[at];
                if keep >= 1.0 {
                    continue;
                }
                let above = bilinear(
                    &coarse_colour,
                    coarse_side,
                    (column as f32 + 0.5) / 2.0 - 0.5,
                    (row as f32 + 0.5) / 2.0 - 0.5,
                );
                for channel in 0..3 {
                    colour[at][channel] =
                        keep * colour[at][channel] + (1.0 - keep) * above[channel];
                }
            }
        }
    }
    levels.swap_remove(0).2
}

fn bilinear(colour: &[[f32; 3]], side: usize, x: f32, y: f32) -> [f32; 3] {
    let x = x.clamp(0.0, (side - 1) as f32);
    let y = y.clamp(0.0, (side - 1) as f32);
    let column = (x as usize).min(side.saturating_sub(2));
    let row = (y as usize).min(side.saturating_sub(2));
    let (across, down) = (x - column as f32, y - row as f32);
    let at = |row: usize, column: usize| colour[row.min(side - 1) * side + column.min(side - 1)];
    let corners = [
        at(row, column),
        at(row, column + 1),
        at(row + 1, column),
        at(row + 1, column + 1),
    ];
    let value: [f32; 3] = std::array::from_fn(|channel| {
        let top = corners[0][channel] * (1.0 - across) + corners[1][channel] * across;
        let bottom = corners[2][channel] * (1.0 - across) + corners[3][channel] * across;
        top * (1.0 - down) + bottom * down
    });
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vertices, triangles and per-vertex texture coordinates.
    type Square = (Vec<[f32; 3]>, Vec<[u32; 3]>, Vec<[f32; 2]>);

    /// A rectangle spanning `x` across and -1 to 1 up at height `z`, facing
    /// +z unless `flipped`, with texture coordinates `u` across and 0 to 1 up.
    fn square(z: f32, x: [f32; 2], u: [f32; 2], flipped: bool) -> Square {
        let vertices = vec![
            [x[0], -1.0, z],
            [x[1], -1.0, z],
            [x[1], 1.0, z],
            [x[0], 1.0, z],
        ];
        let faces = if flipped {
            vec![[0, 2, 1], [0, 3, 2]]
        } else {
            vec![[0, 1, 2], [0, 2, 3]]
        };
        let coords = vec![[u[0], 0.0], [u[1], 0.0], [u[1], 1.0], [u[0], 1.0]];
        (vertices, faces, coords)
    }

    fn layout(coords: Vec<[f32; 2]>, faces: &[[u32; 3]]) -> UvLayout {
        UvLayout {
            coords,
            faces: faces.to_vec(),
        }
    }

    /// Red rises with the column and green with the row.
    fn gradient() -> Vec<u8> {
        (0..224 * 224)
            .flat_map(|index| [(index % 224) as u8, (index / 224) as u8, 0])
            .collect()
    }

    /// Half scale, so the square spans crop pixels 56 to 168 of a 224 pixel
    /// photograph that is the crop itself.
    const PROJECTION: Projection = Projection {
        scale: 0.5,
        offset: [0.0, 0.0],
        crop_left: 0.0,
        crop_top: 0.0,
        crop_step: 1.0,
    };

    #[test]
    fn a_facing_square_takes_the_photographs_colours() {
        let (vertices, faces, coords) = square(0.0, [-1.0, 1.0], [0.0, 1.0], false);
        let rgb = gradient();
        let views = [View {
            photo: Photo {
                width: 224,
                height: 224,
                rgb: &rgb,
            },
            vertices: &vertices,
            projection: PROJECTION,
            labels: None,
        }];
        let layout = layout(coords, &faces);
        let map = bake(&faces, &layout, &views, 64).unwrap();
        // A band along the square's outline is left for the fill.
        assert!(map.seen > 0.8 && map.seen < 0.95, "{}", map.seen);
        let texels = TexelMap::new(&faces, &layout, 64);
        let mut weight = vec![0.0; 64 * 64];
        let mut colour = vec![[0.0; 3]; 64 * 64];
        accumulate(
            &faces,
            &texels,
            &views[0],
            (&[FaceKind::Skin; 2], ScalpColour::Photo),
            &mut weight,
            &mut colour,
            (&mut vec![false; 64 * 64], &mut vec![false; 64 * 64]),
        )
        .unwrap();
        assert_eq!(weight[32 * 64], 0.0, "the outline takes no colour");
        assert!(weight[32 * 64 + 32] > 0.0);
        for (column, row) in [(10usize, 50usize), (32, 16), (55, 40)] {
            let u = (column as f64 + 0.5) / 64.0;
            let v = 1.0 - (row as f64 + 0.5) / 64.0;
            // Texture coordinate u is x = 2u - 1, which lands on photograph
            // column (u + 0.5) * 112; v lands on row (1.5 - v) * 112.
            let expected = [(u + 0.5) * 112.0, (1.5 - v) * 112.0];
            let texel = &map.rgb[(row * 64 + column) * 3..][..3];
            assert!(
                (f64::from(texel[0]) - expected[0]).abs() <= 2.0,
                "{texel:?} {expected:?}"
            );
            assert!(
                (f64::from(texel[1]) - expected[1]).abs() <= 2.0,
                "{texel:?} {expected:?}"
            );
        }
    }

    #[test]
    fn hidden_surface_takes_no_colour_from_the_photograph() {
        // The back square fills the left half of the map; a nearer square
        // covering its right half in the picture fills the map's right half.
        let (mut vertices, mut faces, mut coords) = square(0.0, [-1.0, 1.0], [0.0, 0.5], false);
        let (front, front_faces, front_coords) = square(0.1, [0.0, 1.0], [0.5, 1.0], false);
        vertices.extend(front);
        faces.extend(front_faces.iter().map(|face| face.map(|vertex| vertex + 4)));
        coords.extend(front_coords);
        let uv_faces: Vec<[u32; 3]> = faces.clone();
        let rgb: Vec<u8> = (0..224 * 224)
            .flat_map(|index| {
                if index % 224 < 112 {
                    [0, 0, 255]
                } else {
                    [255, 0, 0]
                }
            })
            .collect();
        let view = View {
            photo: Photo {
                width: 224,
                height: 224,
                rgb: &rgb,
            },
            vertices: &vertices,
            projection: PROJECTION,
            labels: None,
        };
        let size = 64;
        let texels = TexelMap::new(&faces, &layout(coords, &uv_faces), size);
        let mut weight = vec![0.0; size * size];
        let mut colour = vec![[0.0; 3]; size * size];
        accumulate(
            &faces,
            &texels,
            &view,
            (&vec![FaceKind::Skin; faces.len()], ScalpColour::Photo),
            &mut weight,
            &mut colour,
            (&mut vec![false; size * size], &mut vec![false; size * size]),
        )
        .unwrap();
        let at = |u: f64| (size / 2) * size + (u * size as f64) as usize;
        // Back square: x = 4u - 1, visible for x < 0 and hidden for x > 0.
        assert!(weight[at(0.1)] > 0.0);
        assert!(colour[at(0.1)][2] > 200.0 * weight[at(0.1)]);
        assert_eq!(weight[at(0.4)], 0.0, "hidden texels take no colour");
        // Front square: red.
        assert!(colour[at(0.75)][0] > 200.0 * weight[at(0.75)]);
        assert!(weight[at(0.75)] > 0.0);
    }

    #[test]
    fn the_outline_band_holds_along_diagonal_edges() {
        // A square turned 45 degrees: its edges run diagonally in the photograph.
        let vertices = vec![
            [0.0, -1.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [-1.0, 0.0, 0.0],
        ];
        let faces = vec![[0, 1, 2], [0, 2, 3]];
        let coords = vertices
            .iter()
            .map(|[x, y, _]| [(x + 1.0) / 2.0, (y + 1.0) / 2.0])
            .collect();
        let layout = layout(coords, &faces);
        let rgb = gradient();
        let view = View {
            photo: Photo {
                width: 224,
                height: 224,
                rgb: &rgb,
            },
            vertices: &vertices,
            projection: PROJECTION,
            labels: None,
        };
        let size = 512;
        let texels = TexelMap::new(&faces, &layout, size);
        let mut weight = vec![0.0; size * size];
        let mut colour = vec![[0.0; 3]; size * size];
        accumulate(
            &faces,
            &texels,
            &view,
            (&vec![FaceKind::Skin; faces.len()], ScalpColour::Photo),
            &mut weight,
            &mut colour,
            (&mut vec![false; size * size], &mut vec![false; size * size]),
        )
        .unwrap();
        // The margin is 2% of the 224 pixel crop at 56 pixels per metre, so
        // 0.08 m. A point 0.07 m inside the edge x + y = 1 is within it.
        let texel = |distance: f64| {
            let along = 0.5 - distance / std::f64::consts::SQRT_2;
            let u = (along + 1.0) / 2.0;
            let row = ((1.0 - u) * size as f64) as usize;
            let column = (u * size as f64) as usize;
            row * size + column
        };
        assert_eq!(
            weight[texel(0.07)],
            0.0,
            "inside the band along a diagonal edge"
        );
        assert!(weight[texel(0.2)] > 0.0);
    }

    #[test]
    fn surface_turned_away_from_the_camera_takes_no_colour() {
        let (vertices, faces, coords) = square(0.0, [-1.0, 1.0], [0.0, 1.0], true);
        let rgb = gradient();
        let views = [View {
            photo: Photo {
                width: 224,
                height: 224,
                rgb: &rgb,
            },
            vertices: &vertices,
            projection: PROJECTION,
            labels: None,
        }];
        assert_eq!(
            bake(&faces, &layout(coords, &faces), &views, 32).unwrap_err(),
            TextureError::NotVisible
        );
    }

    #[test]
    fn unseen_texels_are_filled_from_their_surroundings() {
        let size = 8;
        let mut confidence = vec![0.0; size * size];
        let mut colour = vec![[0.0; 3]; size * size];
        confidence[3 * size + 3] = 1.0;
        colour[3 * size + 3] = [200.0, 10.0, 50.0];
        let filled = pull_push(size, confidence, colour);
        for texel in &filled {
            assert!(
                (texel[0] - 200.0).abs() < 1.0 && (texel[2] - 50.0).abs() < 1.0,
                "{texel:?}"
            );
        }
    }

    /// Two side-by-side squares facing the camera, each half of the map:
    /// the left one skin and the right one `right`. The photograph is red;
    /// its labels say face skin on the left half and `label` on the right.
    fn labelled_bake(right: FaceKind, label: u8, scalp: ScalpColour) -> ColourMap {
        let (mut vertices, mut faces, mut coords) = square(0.0, [-1.0, 0.0], [0.0, 0.5], false);
        let (more, more_faces, more_coords) = square(0.0, [0.0, 1.0], [0.5, 1.0], false);
        vertices.extend(more);
        faces.extend(more_faces.iter().map(|face| face.map(|vertex| vertex + 4)));
        coords.extend(more_coords);
        let kinds = [FaceKind::Skin, FaceKind::Skin, right, right];
        let rgb: Vec<u8> = (0..224 * 224).flat_map(|_| [200, 0, 0]).collect();
        let labels: Vec<u8> = (0..224 * 224)
            .map(|index| {
                if index % 224 < 112 {
                    LABEL_FACE_SKIN
                } else {
                    label
                }
            })
            .collect();
        let layout = layout(coords, &faces);
        let mut builder = ColourMapBuilder::new(&faces, &layout, 32, &kinds, scalp).unwrap();
        builder
            .add(&View {
                photo: Photo {
                    width: 224,
                    height: 224,
                    rgb: &rgb,
                },
                vertices: &vertices,
                projection: PROJECTION,
                labels: Some(Labels {
                    side: 224,
                    data: &labels,
                    left: 0.0,
                    top: 0.0,
                    step: 1.0,
                }),
            })
            .unwrap();
        builder.finish().unwrap()
    }

    fn texel(map: &ColourMap, u: f64) -> [u8; 3] {
        let at = (map.size / 2) * map.size + (u * map.size as f64) as usize;
        [0, 1, 2].map(|channel| map.rgb[at * 3 + channel])
    }

    #[test]
    fn skin_takes_no_colour_from_hair_in_front_of_it() {
        let photographed = labelled_bake(FaceKind::Skin, LABEL_FACE_SKIN, ScalpColour::Photo);
        let covered = labelled_bake(FaceKind::Skin, LABEL_HAIR, ScalpColour::Photo);
        assert!(
            covered.seen < photographed.seen * 0.7,
            "{} {}",
            covered.seen,
            photographed.seen
        );
        // The covered half is filled from the skin beside it.
        assert_eq!(texel(&covered, 0.75), [200, 0, 0]);
    }

    #[test]
    fn the_scalp_follows_its_colouring() {
        let green = [10, 200, 30];
        // Hair over the scalp: painted, filled as skin, or photographed.
        let painted = labelled_bake(FaceKind::Scalp, LABEL_HAIR, ScalpColour::Hair(green));
        assert_eq!(texel(&painted, 0.75), green);
        assert_eq!(texel(&painted, 0.25), [200, 0, 0]);
        let skin = labelled_bake(FaceKind::Scalp, LABEL_HAIR, ScalpColour::Skin);
        assert_eq!(texel(&skin, 0.75), [200, 0, 0]);
        let photo = labelled_bake(FaceKind::Scalp, LABEL_HAIR, ScalpColour::Photo);
        assert!(photo.seen > skin.seen);
        // Bare scalp keeps its photographed colour even when painting hair.
        let bare = labelled_bake(FaceKind::Scalp, LABEL_FACE_SKIN, ScalpColour::Hair(green));
        assert_eq!(texel(&bare, 0.75), [200, 0, 0]);
    }

    #[test]
    fn eyes_take_colour_from_anything_but_hair_clothes_and_background() {
        for (label, seen) in [
            (LABEL_OTHER, true),
            (LABEL_FACE_SKIN, true),
            (LABEL_HAIR, false),
            (LABEL_CLOTHES, false),
            (LABEL_BACKGROUND, false),
        ] {
            let map = labelled_bake(FaceKind::Eye, label, ScalpColour::Photo);
            let skin_only = labelled_bake(FaceKind::Eye, LABEL_HAIR, ScalpColour::Photo);
            assert_eq!(map.seen > skin_only.seen, seen, "label {label}");
        }
    }

    /// Three squares in a row sharing their edges: skin on the left, scalp
    /// in the middle and skin on the right, each a third of the map. The
    /// photograph shows red face skin on the left, green hair in the middle
    /// and background on the right, so the right square is not coloured.
    fn strip_bake(scalp: ScalpColour) -> ColourMap {
        strip_bake_labelled(scalp, [LABEL_FACE_SKIN, LABEL_HAIR, LABEL_BACKGROUND])
    }

    /// As `strip_bake`, with the labels of the photograph's three thirds.
    fn strip_bake_labelled(scalp: ScalpColour, thirds: [u8; 3]) -> ColourMap {
        let xs = [-1.0f32, -1.0 / 3.0, 1.0 / 3.0, 1.0];
        let mut vertices = Vec::new();
        let mut coords = Vec::new();
        for y in [-1.0f32, 1.0] {
            for (column, x) in xs.iter().enumerate() {
                vertices.push([*x, y, 0.0]);
                coords.push([column as f32 / 3.0, (y + 1.0) / 2.0]);
            }
        }
        let mut faces = Vec::new();
        for column in 0..3u32 {
            let (a, b, c, d) = (column, column + 1, column + 5, column + 4);
            faces.extend([[a, b, c], [a, c, d]]);
        }
        let kinds = [
            FaceKind::Skin,
            FaceKind::Skin,
            FaceKind::Scalp,
            FaceKind::Scalp,
            FaceKind::Skin,
            FaceKind::Skin,
        ];
        // Photograph columns 56 to 168 span the strip; thirds of it.
        let third = |column: usize| (column.saturating_sub(56) * 3 / 112).min(2);
        let rgb: Vec<u8> = (0..224 * 224)
            .flat_map(|index| match third(index % 224) {
                0 => [200, 0, 0],
                1 => [0, 200, 0],
                _ => [0, 0, 200],
            })
            .collect();
        let labels: Vec<u8> = (0..224 * 224)
            .map(|index| thirds[third(index % 224)])
            .collect();
        let layout = layout(coords, &faces);
        let mut builder = ColourMapBuilder::new(&faces, &layout, 48, &kinds, scalp).unwrap();
        builder
            .add(&View {
                photo: Photo {
                    width: 224,
                    height: 224,
                    rgb: &rgb,
                },
                vertices: &vertices,
                projection: PROJECTION,
                labels: Some(Labels {
                    side: 224,
                    data: &labels,
                    left: 0.0,
                    top: 0.0,
                    step: 1.0,
                }),
            })
            .unwrap();
        builder.finish().unwrap()
    }

    #[test]
    fn a_photographed_scalp_does_not_colour_unseen_skin() {
        let map = strip_bake(ScalpColour::Photo);
        // The right square, beyond the scalp, takes the skin's red, not hair.
        assert_eq!(texel(&map, 0.9), [200, 0, 0]);
        // The scalp keeps its photographed hair.
        assert_eq!(texel(&map, 0.5), [0, 200, 0]);
    }

    #[test]
    fn bare_scalp_photographed_as_skin_still_fills_unseen_skin() {
        // Only the scalp, in the middle, is seen, and it shows bare skin.
        let map = strip_bake_labelled(
            ScalpColour::Photo,
            [LABEL_BACKGROUND, LABEL_FACE_SKIN, LABEL_BACKGROUND],
        );
        assert_eq!(texel(&map, 0.9), [0, 200, 0]);
        assert_eq!(texel(&map, 0.1), [0, 200, 0]);
    }

    #[test]
    fn bare_scalp_seen_at_an_angle_keeps_its_colour_under_painted_hair() {
        // A square turned 60 degrees from the camera, all scalp, seen as skin:
        // each texel's weight is far below a full view.
        let tilt = 60f32.to_radians();
        let (mut vertices, faces, coords) = square(0.0, [-1.0, 1.0], [0.0, 1.0], false);
        for vertex in &mut vertices {
            let x = vertex[0];
            vertex[0] = x * tilt.cos();
            vertex[2] = -x * tilt.sin();
        }
        let rgb: Vec<u8> = (0..224 * 224).flat_map(|_| [200, 0, 0]).collect();
        let labels = vec![LABEL_FACE_SKIN; 224 * 224];
        let layout = layout(coords, &faces);
        let kinds = [FaceKind::Scalp; 2];
        let mut builder = ColourMapBuilder::new(
            &faces,
            &layout,
            32,
            &kinds,
            ScalpColour::Hair([10, 200, 30]),
        )
        .unwrap();
        builder
            .add(&View {
                photo: Photo {
                    width: 224,
                    height: 224,
                    rgb: &rgb,
                },
                vertices: &vertices,
                projection: PROJECTION,
                labels: Some(Labels {
                    side: 224,
                    data: &labels,
                    left: 0.0,
                    top: 0.0,
                    step: 1.0,
                }),
            })
            .unwrap();
        let map = builder.finish().unwrap();
        assert!(map.seen > 0.2, "{}", map.seen);
        assert_eq!(texel(&map, 0.5), [200, 0, 0]);
    }

    #[test]
    fn pixels_at_the_edge_of_a_label_are_not_trusted() {
        let data: Vec<u8> = (0..16 * 16)
            .map(|index| {
                if index % 16 < 8 {
                    LABEL_FACE_SKIN
                } else {
                    LABEL_HAIR
                }
            })
            .collect();
        // Each label pixel covers ten photograph pixels.
        let labels = Labels {
            side: 16,
            data: &data,
            left: 0.0,
            top: 0.0,
            step: 0.1,
        };
        let skin = |label| label == LABEL_FACE_SKIN;
        assert!(labels.agree([40.0, 80.0], skin));
        // Label column 7 borders the hair at column 8.
        assert!(!labels.agree([70.0, 80.0], skin));
        assert!(
            !labels.agree([0.0, 80.0], skin),
            "the square's edge is not trusted"
        );
    }

    #[test]
    fn eyeballs_are_the_parts_beside_the_head() {
        let faces = [[0, 1, 2], [0, 2, 3], [4, 5, 6], [7, 8, 9]];
        assert_eq!(minor_parts(&faces), [false, false, true, true]);
    }

    #[test]
    fn unusable_inputs_are_refused() {
        let (vertices, faces, coords) = square(0.0, [-1.0, 1.0], [0.0, 1.0], false);
        let layout = layout(coords, &faces);
        let rgb = gradient();
        let bake_with = |vertices: &[[f32; 3]], rgb: &[u8], projection: Projection| {
            let views = [View {
                photo: Photo {
                    width: 224,
                    height: 224,
                    rgb,
                },
                vertices,
                projection,
                labels: None,
            }];
            bake(&faces, &layout, &views, 16)
        };
        assert_eq!(
            bake_with(&vertices, &rgb[3..], PROJECTION).unwrap_err(),
            TextureError::Photo
        );
        assert_eq!(
            bake_with(&vertices[..3], &rgb, PROJECTION).unwrap_err(),
            TextureError::Vertices
        );
        let mut flat = PROJECTION;
        flat.scale = 0.0;
        assert_eq!(
            bake_with(&vertices, &rgb, flat).unwrap_err(),
            TextureError::Projection
        );
        assert_eq!(
            bake(&faces, &layout, &[], 1).unwrap_err(),
            TextureError::Size
        );
        assert_eq!(
            ColourMapBuilder::new(&faces, &layout, 16, &[FaceKind::Skin], ScalpColour::Photo).err(),
            Some(TextureError::Kinds)
        );
        let mut builder = ColourMapBuilder::new(
            &faces,
            &layout,
            16,
            &[FaceKind::Skin; 2],
            ScalpColour::Photo,
        )
        .unwrap();
        let short = vec![LABEL_FACE_SKIN; 10];
        assert_eq!(
            builder
                .add(&View {
                    photo: Photo {
                        width: 224,
                        height: 224,
                        rgb: &rgb,
                    },
                    vertices: &vertices,
                    projection: PROJECTION,
                    labels: Some(Labels {
                        side: 224,
                        data: &short,
                        left: 0.0,
                        top: 0.0,
                        step: 1.0,
                    }),
                })
                .unwrap_err(),
            TextureError::Labels
        );
    }
}
