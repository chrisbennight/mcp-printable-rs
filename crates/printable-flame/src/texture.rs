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
//!
//! The eyeballs can be painted instead of photographed: an iris and pupil
//! around each eyeball's gaze and white elsewhere, with the iris colour
//! given or measured from what the photographs show of the iris.
//!
//! The photographs' light and shade can be removed. For each photograph, a
//! smooth shading, an ambient term plus one direction for each colour
//! channel in linear light, is fitted to the skin from the fitted head's
//! surface normals and divided out. The mean skin colour is kept, so the
//! subject's skin tone stays as photographed, and specular highlights are
//! limited.

use thiserror::Error;

use crate::UvLayout;

/// Side of the crop the camera is expressed in, in pixels.
const CROP_PIXELS: f64 = 224.0;
/// Surface turned further than this from the camera, as the cosine of the
/// angle to the viewing direction, takes no colour from that photograph.
pub(crate) const MIN_FACING: f32 = 0.35;
/// Points that land within this fraction of the crop of the head's outline in
/// a photograph take no colour from it: the fitted head's outline never
/// matches the real one exactly, and the background lies just beyond it.
const OUTLINE_MARGIN: f64 = 0.02;
/// Weight at which a texel's photographed colour is used unblended.
const FULL_WEIGHT: f32 = 0.5;
/// A point counts as visible when it is no more than this far, in metres,
/// behind the nearest surface at its pixel.
const DEPTH_TOLERANCE_M: f32 = 0.002;
/// The same for a point on an eyeball. The eyelids lie just in front of the
/// eyeballs, so the general tolerance would let them colour the eyeballs.
const EYE_DEPTH_TOLERANCE_M: f32 = 0.0005;
/// The pupil's colour in painted eyes, sRGB.
const PUPIL_RGB: [f32; 3] = [16.0, 14.0, 14.0];
/// Share of a painted iris's radius, at its edge, that darkens toward the
/// white, and how much it darkens there.
const LIMBUS_SHARE: f32 = 0.15;
const LIMBUS_DARKENING: f32 = 0.45;
/// Painted edges blend over this share of the iris's radius on each side.
const PAINT_SOFTNESS: f32 = 0.04;
/// A measured iris colour comes from photographed texels between these
/// shares of the iris's radius, clear of the pupil and of the white.
const IRIS_SAMPLE_SHARES: [f32; 2] = [0.45, 0.85];
/// The iris is looked for within this share of its radius of the gaze.
const IRIS_SEARCH_SHARE: f32 = 2.0;
/// The white of a photographed eye is taken as this share of the way up its
/// luminances, and an iris must be at least this share darker than it.
const WHITE_PERCENTILE: f32 = 0.9;
const MIN_IRIS_CONTRAST: f32 = 0.15;
/// Fewest photographed iris texels to measure an iris colour from.
const MIN_IRIS_TEXELS: usize = 16;
/// Iris colour used when it is neither given nor measured: a mid brown.
pub const DEFAULT_IRIS_RGB: [u8; 3] = [96, 66, 42];
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

/// What becomes of the light and shade in the photographs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lighting {
    /// As photographed.
    Keep,
    /// Each photograph's smooth shading over the skin is estimated and
    /// divided out.
    Remove,
}

/// Eyeballs painted rather than photographed: an iris and pupil around each
/// eyeball's gaze, which is +z with the head at rest, and white elsewhere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaintedEyes {
    /// sRGB iris colour, or `None` to measure it from the photographs.
    pub iris: Option<[u8; 3]>,
    /// sRGB colour of the white of the eye.
    pub sclera: [u8; 3],
    /// Diameters across the gaze, in the model's units.
    pub iris_diameter: f32,
    pub pupil_diameter: f32,
}

/// Where a painted iris's colour came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrisSource {
    Given,
    Measured,
    /// Neither given nor seen well enough to measure: [`DEFAULT_IRIS_RGB`].
    Default,
}

/// A texel on an eyeball, placed relative to its eyeball at rest.
#[derive(Debug, Clone, Copy)]
struct EyeTexel {
    texel: usize,
    /// Which eyeball it is on.
    eye: usize,
    /// Offset across the gaze, in x and y, from the eyeball's centre.
    offset: [f32; 2],
    /// Whether it lies on the front half, facing along the gaze.
    front: bool,
}

impl EyeTexel {
    /// Distance from the eyeball's gaze axis.
    fn across(&self) -> f32 {
        self.offset[0].hypot(self.offset[1])
    }
}

/// How a colour map treats the head's parts and the photographs' light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colouring {
    pub scalp: ScalpColour,
    pub lighting: Lighting,
}

/// The shading estimated for one photograph, in linear light: for each
/// colour channel, `ambient + direction · normal`, where the normal is in
/// the camera's frame with x right, y up and z toward the camera.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shading {
    pub ambient: [f32; 3],
    pub direction: [[f32; 3]; 3],
}

impl Shading {
    /// The same shading with its directions turned back by an axis-angle
    /// rotation. Turned back by the head's rotation in its photograph, they
    /// go from the camera's frame to the head's own.
    pub fn turned_back(self, rotation: [f32; 3]) -> Shading {
        let angle = rotation
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        if angle == 0.0 {
            return self;
        }
        let axis = rotation.map(|value| value / angle);
        let (sin, cos) = (-angle).sin_cos();
        let turn = |vector: [f32; 3]| {
            let across = [
                axis[1] * vector[2] - axis[2] * vector[1],
                axis[2] * vector[0] - axis[0] * vector[2],
                axis[0] * vector[1] - axis[1] * vector[0],
            ];
            let along = axis[0] * vector[0] + axis[1] * vector[1] + axis[2] * vector[2];
            std::array::from_fn(|index| {
                vector[index] * cos + across[index] * sin + axis[index] * along * (1.0 - cos)
            })
        };
        Shading {
            ambient: self.ambient,
            direction: self.direction.map(turn),
        }
    }

    fn at(&self, channel: usize, normal: [f32; 3]) -> f32 {
        let direction = self.direction[channel];
        self.ambient[channel]
            + direction[0] * normal[0]
            + direction[1] * normal[1]
            + direction[2] * normal[2]
    }
}

/// Fewer skin samples than this leave a photograph's lighting as it is.
const MIN_LIGHTING_SAMPLES: usize = 200;
/// Shading is not trusted below this share of its average, where dividing by
/// it would blow up noise in deep shadow.
const MIN_SHADING_SHARE: f32 = 0.25;
/// Luminance above this percentile of the skin's is treated as specular
/// highlight and brought down to it.
const HIGHLIGHT_PERCENTILE: f64 = 97.0;

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
    /// Whether `accepts` holds for every label within one photograph pixel,
    /// and at least one label pixel, of a photograph pixel position, corners
    /// included. A sample there blends the photograph's pixels within one
    /// pixel of it, and pixels at the edge of a labelled region mix what lies
    /// on either side, such as strands of hair over skin.
    fn agree(&self, pixel: [f64; 2], accepts: impl Fn(u8) -> bool) -> bool {
        let reach = (1.0 / self.step).max(1.0);
        let span = |position: f64, origin: f64| {
            [-reach, reach].map(|offset| ((position + offset - origin) * self.step).round())
        };
        let ([left, right], [top, bottom]) = (span(pixel[0], self.left), span(pixel[1], self.top));
        let side = self.side as f64;
        if left < 0.0 || top < 0.0 || right >= side || bottom >= side {
            return false;
        }
        (top as usize..=bottom as usize).all(|row| {
            (left as usize..=right as usize).all(|column| {
                self.data
                    .get(row * self.side + column)
                    .is_some_and(|&label| accepts(label))
            })
        })
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
    pub(crate) fn project(&self, point: [f32; 3]) -> ([f64; 2], f32) {
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
    /// For each photograph in the order added, the shading removed from it:
    /// `None` when lighting is kept or too little skin was seen to estimate it.
    pub shading: Vec<Option<Shading>>,
    /// For painted eyes, the iris colour used and where it came from; `None`
    /// when the eyes were photographed or the head has no eyeballs.
    pub iris: Option<([u8; 3], IrisSource)>,
    /// For each model triangle, the map's texels on it and how many of them
    /// some photograph saw directly, as counted for `seen`.
    pub face_texels: Vec<[u32; 2]>,
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
    #[error("painted eyes need a pupil narrower than the iris, both above zero")]
    EyeSizes,
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
    let colouring = Colouring {
        scalp: ScalpColour::Photo,
        lighting: Lighting::Keep,
    };
    let mut builder = ColourMapBuilder::new(faces, layout, size, &kinds, colouring)?;
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
    /// The colour of each texel from pixels labelled skin alone.
    skin: SkinSamples,
    colour: Vec<[f32; 3]>,
    size: usize,
    kinds: Vec<FaceKind>,
    colouring: Colouring,
    shading: Vec<Option<Shading>>,
    painted: Option<(PaintedEyes, Vec<EyeTexel>)>,
}

impl<'a> ColourMapBuilder<'a> {
    /// A builder for a `size` texel map. `kinds` names the part of the head
    /// each triangle belongs to, and `colouring` says how the scalp is
    /// coloured and what becomes of the photographs' light.
    pub fn new(
        faces: &'a [[u32; 3]],
        layout: &UvLayout,
        size: usize,
        kinds: &[FaceKind],
        colouring: Colouring,
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
            skin: SkinSamples::new(size * size),
            colour: vec![[0.0; 3]; size * size],
            size,
            kinds: kinds.to_vec(),
            colouring,
            shading: Vec::new(),
            painted: None,
        })
    }

    /// Paint the eyeballs instead of colouring them from the photographs.
    /// `rest` is the head at rest, one vertex per model vertex, in the
    /// model's units. Each eyeball's centre is the middle of its extent.
    pub fn paint_eyes(&mut self, rest: &[[f32; 3]], eyes: PaintedEyes) -> Result<(), TextureError> {
        if !(eyes.pupil_diameter > 0.0 && eyes.pupil_diameter < eyes.iris_diameter)
            || !eyes.iris_diameter.is_finite()
        {
            return Err(TextureError::EyeSizes);
        }
        if self
            .faces
            .iter()
            .flatten()
            .any(|&vertex| vertex as usize >= rest.len())
        {
            return Err(TextureError::Vertices);
        }
        let on_eye = |face: usize| self.kinds[face] == FaceKind::Eye;
        // Eyeballs are the connected groups of eye triangles.
        let mut parent: Vec<usize> = (0..rest.len()).collect();
        for (face, corners) in self.faces.iter().enumerate() {
            if on_eye(face) {
                for pair in [[corners[0], corners[1]], [corners[1], corners[2]]] {
                    let (a, b) = (
                        root(&mut parent, pair[0] as usize),
                        root(&mut parent, pair[1] as usize),
                    );
                    parent[a] = b;
                }
            }
        }
        let mut bounds: std::collections::HashMap<usize, ([f32; 3], [f32; 3])> =
            std::collections::HashMap::new();
        for (face, corners) in self.faces.iter().enumerate() {
            if !on_eye(face) {
                continue;
            }
            for &vertex in corners {
                let point = rest[vertex as usize];
                let (low, high) = bounds
                    .entry(root(&mut parent, vertex as usize))
                    .or_insert((point, point));
                for axis in 0..3 {
                    low[axis] = low[axis].min(point[axis]);
                    high[axis] = high[axis].max(point[axis]);
                }
            }
        }
        let mut texels = Vec::new();
        for (index, &face) in self.texels.face.iter().enumerate() {
            if face == NO_FACE || !on_eye(face as usize) {
                continue;
            }
            let corners = self.faces[face as usize];
            let eye = root(&mut parent, corners[0] as usize);
            let (low, high) = bounds[&eye];
            let [wa, wb] = self.texels.barycentric[index];
            let shares = [wa, wb, 1.0 - wa - wb];
            let offset: [f32; 3] = std::array::from_fn(|axis| {
                corners
                    .iter()
                    .zip(shares)
                    .map(|(corner, share)| share * rest[*corner as usize][axis])
                    .sum::<f32>()
                    - (low[axis] + high[axis]) / 2.0
            });
            texels.push(EyeTexel {
                texel: index,
                eye,
                offset: [offset[0], offset[1]],
                front: offset[2] > 0.0,
            });
        }
        self.painted = Some((eyes, texels));
        Ok(())
    }

    /// Add one photograph's colour to the texels it sees.
    pub fn add(&mut self, view: &View<'_>) -> Result<(), TextureError> {
        let shading = accumulate(
            self.faces,
            &self.texels,
            view,
            (&self.kinds, self.colouring),
            &mut self.weight,
            &mut self.colour,
            (&mut self.in_view, &mut self.skin),
        )?;
        self.shading.push(shading);
        Ok(())
    }

    /// Blend the photographs and fill what none of them saw.
    pub fn finish(self) -> Result<ColourMap, TextureError> {
        let Self {
            faces,
            texels,
            weight,
            in_view,
            skin,
            mut colour,
            size,
            kinds,
            colouring,
            shading,
            painted,
        } = self;
        let scalp = colouring.scalp;
        let in_view =
            in_view.iter().filter(|&&seen| seen).count() as f64 / texels.on_surface().max(1) as f64;
        if in_view < MIN_SEEN {
            return Err(TextureError::NotVisible);
        }
        let mut map = finish(
            faces,
            &texels,
            (&weight, skin),
            &mut colour,
            size,
            (&kinds, painted.as_ref()),
            scalp,
        )?;
        map.shading = shading;
        Ok(map)
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

/// The representative of a vertex's group, halving the path on the way.
fn root(parent: &mut [usize], mut index: usize) -> usize {
    while parent[index] != index {
        parent[index] = parent[parent[index]];
        index = parent[index];
    }
    index
}

fn finish(
    faces: &[[u32; 3]],
    texels: &TexelMap,
    (weight, skin): (&[f32], SkinSamples),
    colour: &mut [[f32; 3]],
    size: usize,
    (kinds, painting): (&[FaceKind], Option<&(PaintedEyes, Vec<EyeTexel>)>),
    scalp: ScalpColour,
) -> Result<ColourMap, TextureError> {
    let on_scalp = |face: u32| kinds[face as usize] == FaceKind::Scalp;
    let painted = matches!(scalp, ScalpColour::Hair(_));
    let mut on_surface = 0usize;
    let mut seen = 0usize;
    let mut face_texels = vec![[0u32; 2]; faces.len()];
    // Painted eyes show no photograph's colour.
    let painted_eyes = painting.is_some_and(|(_, texels)| !texels.is_empty());
    for (index, &face) in texels.face.iter().enumerate() {
        if face != NO_FACE {
            on_surface += 1;
            face_texels[face as usize][0] += 1;
            // Painted hair shows a photograph's colour only where it saw
            // bare scalp.
            let shown = if painted_eyes && kinds[face as usize] == FaceKind::Eye {
                0.0
            } else if painted && on_scalp(face) {
                skin.weight[index]
            } else {
                weight[index]
            };
            if shown > 0.0 {
                seen += 1;
                face_texels[face as usize][1] += 1;
            }
        }
    }
    let seen = seen as f64 / on_surface.max(1) as f64;
    for (colour, weight) in colour.iter_mut().zip(weight) {
        if *weight > 0.0 {
            *colour = colour.map(|channel| channel / weight);
        }
    }
    let iris = painting
        .filter(|(_, texels)| !texels.is_empty())
        .map(|(eyes, texels)| match eyes.iris {
            Some(given) => (given, IrisSource::Given),
            None => match measured_iris(eyes, texels, weight, colour) {
                Some(measured) => (measured, IrisSource::Measured),
                None => (DEFAULT_IRIS_RGB, IrisSource::Default),
            },
        });
    let vertex_count = faces
        .iter()
        .flatten()
        .map(|&vertex| vertex as usize + 1)
        .max()
        .unwrap_or(0);
    let on_eye = |face: u32| kinds[face as usize] == FaceKind::Eye;
    // Skin is filled only from what the photographs showed as skin. Eyes
    // are left out, and a scalp taken as photographed shows hair, so it
    // seeds the skin fill only with what was photographed there as skin.
    // Such a scalp, and eyes that were seen, keep fills of their own built
    // from everything they showed.
    let photographed_scalp = scalp == ScalpColour::Photo;
    let mut skin_weight = weight.to_vec();
    let mut skin_colour = colour.to_vec();
    let mut eye_weight = vec![0.0f32; weight.len()];
    for (index, &face) in texels.face.iter().enumerate() {
        if face == NO_FACE {
            continue;
        }
        if on_eye(face) {
            eye_weight[index] = weight[index];
            skin_weight[index] = 0.0;
        } else if photographed_scalp && on_scalp(face) {
            let seen = skin.weight[index];
            skin_weight[index] = seen;
            if seen > 0.0 {
                skin_colour[index] = skin.colour[index].map(|channel| channel / seen);
            }
        }
    }
    let fill = surface_fill(faces, vertex_count, texels, &skin_weight, &skin_colour);
    let scalp_fill =
        photographed_scalp.then(|| surface_fill(faces, vertex_count, texels, weight, colour));
    let eye_fill = eye_weight
        .iter()
        .any(|&weight| weight > 0.0)
        .then(|| surface_fill(faces, vertex_count, texels, &eye_weight, colour));
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
        let source = match (&scalp_fill, &eye_fill) {
            (Some(scalp_fill), _) if on_scalp(face) => scalp_fill,
            (_, Some(eye_fill)) if on_eye(face) => eye_fill,
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
        // scalp, and in full the colour of the pixels labelled skin wherever
        // one did. A photograph without labels cannot show bare scalp.
        if let Some(paint) = paint.filter(|_| on_scalp(face)) {
            let bare = skin.weight[index];
            if bare > 0.0 {
                colour[index] = skin.colour[index].map(|channel| channel / bare);
                keep = 1.0;
            } else {
                filled = paint;
                keep = 0.0;
            }
        }
        for channel in 0..3 {
            colour[index][channel] = keep * colour[index][channel] + (1.0 - keep) * filled[channel];
        }
        confidence[index] = 1.0;
    }
    if let (Some((eyes, texels)), Some((iris, _))) = (painting, iris) {
        for texel in texels {
            colour[texel.texel] = painted_colour(eyes, iris.map(f32::from), texel);
        }
    }
    let filled = pull_push(size, confidence, colour.to_vec());
    let rgb = filled
        .iter()
        .flat_map(|texel| texel.map(|channel| channel.round().clamp(0.0, 255.0) as u8))
        .collect();
    Ok(ColourMap {
        size,
        rgb,
        seen,
        shading: Vec::new(),
        iris,
        face_texels,
    })
}

/// The iris colour the photographs show. A subject who looked aside shows
/// the iris away from the eyeball's gaze at rest, so each eye's iris is
/// found first: the iris and pupil are darker than the white around them,
/// and their centre is where the darkness near the gaze is centred. The
/// colour is the middle half, by luminance, of the photographed texels
/// around that centre clear of the pupil and of the white, so that
/// reflections and lashes are left out. `None` when too few were seen.
fn measured_iris(
    eyes: &PaintedEyes,
    texels: &[EyeTexel],
    weight: &[f32],
    colour: &[[f32; 3]],
) -> Option<[u8; 3]> {
    let radius = eyes.iris_diameter / 2.0;
    let [inner, outer] = IRIS_SAMPLE_SHARES.map(|share| share * radius);
    let inner = inner.max(eyes.pupil_diameter / 2.0);
    let luminance = |rgb: &[f32; 3]| 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    let seen: Vec<&EyeTexel> = texels
        .iter()
        .filter(|texel| texel.front && weight[texel.texel] > 0.0)
        .collect();
    let mut eye_ids: Vec<usize> = seen.iter().map(|texel| texel.eye).collect();
    eye_ids.sort_unstable();
    eye_ids.dedup();
    let mut samples: Vec<[f32; 3]> = Vec::new();
    for eye in eye_ids {
        let near: Vec<&&EyeTexel> = seen
            .iter()
            .filter(|texel| texel.eye == eye && texel.across() <= IRIS_SEARCH_SHARE * radius)
            .collect();
        if near.is_empty() {
            continue;
        }
        let mean = near
            .iter()
            .map(|texel| luminance(&colour[texel.texel]))
            .sum::<f32>()
            / near.len() as f32;
        let mut total = 0.0f32;
        let mut sum = [0.0f32; 2];
        for texel in &near {
            let darkness = (mean - luminance(&colour[texel.texel])).max(0.0);
            total += darkness;
            for (total, offset) in sum.iter_mut().zip(texel.offset) {
                *total += darkness * offset;
            }
        }
        if total <= 0.0 {
            continue;
        }
        let centre = sum.map(|value| value / total);
        let ring: Vec<[f32; 3]> = seen
            .iter()
            .filter(|texel| {
                let distance = (texel.offset[0] - centre[0]).hypot(texel.offset[1] - centre[1]);
                texel.eye == eye && (inner..=outer).contains(&distance)
            })
            .map(|texel| colour[texel.texel])
            .collect();
        // Small variations in a white with no iris in view would also give
        // a centre, so only a ring distinctly darker than the white counts.
        let percentile = |mut values: Vec<f32>, share: f32| {
            values.sort_by(f32::total_cmp);
            values[((values.len() - 1) as f32 * share) as usize]
        };
        if ring.is_empty() {
            continue;
        }
        let white = percentile(
            near.iter()
                .map(|texel| luminance(&colour[texel.texel]))
                .collect(),
            WHITE_PERCENTILE,
        );
        let middle = percentile(ring.iter().map(luminance).collect(), 0.5);
        if middle <= (1.0 - MIN_IRIS_CONTRAST) * white {
            samples.extend(ring);
        }
    }
    let mut seen = samples;
    if seen.len() < MIN_IRIS_TEXELS {
        return None;
    }
    seen.sort_by(|a, b| luminance(a).total_cmp(&luminance(b)));
    let middle = &seen[seen.len() / 4..seen.len() - seen.len() / 4];
    let count = middle.len() as f32;
    Some(std::array::from_fn(|channel| {
        (middle.iter().map(|rgb| rgb[channel]).sum::<f32>() / count)
            .round()
            .clamp(0.0, 255.0) as u8
    }))
}

/// A painted eye's colour at a texel: pupil, iris darkening toward its edge,
/// or white, with edges blended so that they do not show the texels.
fn painted_colour(eyes: &PaintedEyes, iris: [f32; 3], texel: &EyeTexel) -> [f32; 3] {
    let sclera = eyes.sclera.map(f32::from);
    if !texel.front {
        return sclera;
    }
    let radius = eyes.iris_diameter / 2.0;
    let soft = PAINT_SOFTNESS * radius;
    let across = texel.across();
    let inside = |edge: f32| 1.0 - smoothstep(edge - soft, edge + soft, across);
    let limbus = smoothstep(radius * (1.0 - LIMBUS_SHARE), radius, across);
    let ring = iris.map(|channel| channel * (1.0 - LIMBUS_DARKENING * limbus));
    let mix = |a: [f32; 3], b: [f32; 3], share: f32| -> [f32; 3] {
        std::array::from_fn(|channel| a[channel] * (1.0 - share) + b[channel] * share)
    };
    let iris = mix(ring, PUPIL_RGB, inside(eyes.pupil_diameter / 2.0));
    mix(sclera, iris, inside(radius))
}

/// 0 below `low`, 1 above `high`, and a smooth rise between.
fn smoothstep(low: f32, high: f32, value: f32) -> f32 {
    let t = ((value - low) / (high - low)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
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

pub(crate) fn edge(a: [f64; 2], b: [f64; 2], point: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (point[1] - a[1]) - (b[1] - a[1]) * (point[0] - a[0])
}

/// Texel indices whose centres may lie between `low` and `high`.
fn texel_range(low: f64, high: f64, size: usize) -> std::ops::Range<usize> {
    let first = (low - 0.5).floor().max(0.0) as usize;
    let last = ((high - 0.5).ceil().max(-1.0) + 1.0) as usize;
    first.min(size)..last.min(size)
}

/// One photographed colour for a texel, before it is added.
struct Sample {
    texel: usize,
    share: f32,
    colour: [f32; 3],
    /// Unit surface normal in the camera's frame.
    normal: [f32; 3],
    /// Whether the texel is on skin, whose shading the lighting is fitted to.
    skin: bool,
    /// Whether the pixel was labelled skin.
    labelled_skin: bool,
}

/// Add one photograph's colour to the texels it sees, weighted by how
/// squarely it sees them, and return the shading removed from it, if any.
fn accumulate(
    faces: &[[u32; 3]],
    texels: &TexelMap,
    view: &View<'_>,
    (kinds, colouring): (&[FaceKind], Colouring),
    weight: &mut [f32],
    colour: &mut [[f32; 3]],
    (in_view, skin): (&mut [bool], &mut SkinSamples),
) -> Result<Option<Shading>, TextureError> {
    let scalp = colouring.scalp;
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
    let tolerance = |face: u32| {
        projection.scale
            * match kinds[face as usize] {
                FaceKind::Eye => EYE_DEPTH_TOLERANCE_M,
                FaceKind::Skin | FaceKind::Scalp => DEPTH_TOLERANCE_M,
            }
    };
    // One pixel more, because probes are read at the nearest pixel centre.
    let margin = OUTLINE_MARGIN * CROP_PIXELS / projection.crop_step + 1.0;
    let mut samples = Vec::new();
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
        if point_depth < nearest - tolerance(face) {
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
        let labelled_skin = match &view.labels {
            Some(labels) => {
                if !labels.agree(pixel, |label| kinds[face as usize].accepts(label, scalp)) {
                    continue;
                }
                // Skin only where its neighbours agree, as for every other
                // label, so mixed pixels at a label's edge are not taken.
                labels.agree(pixel, |label| {
                    matches!(label, LABEL_BODY_SKIN | LABEL_FACE_SKIN)
                })
            }
            None => false,
        };
        let Some(photographed) = sample(photo, pixel) else {
            continue;
        };
        samples.push(Sample {
            texel: index,
            share: ((facing - MIN_FACING) / (1.0 - MIN_FACING)).powi(2),
            colour: photographed,
            normal: normal.map(|axis| axis / length),
            skin: kinds[face as usize] == FaceKind::Skin,
            labelled_skin,
        });
    }
    let shading = match colouring.lighting {
        Lighting::Keep => None,
        Lighting::Remove => remove_lighting(&mut samples),
    };
    for sample in samples {
        weight[sample.texel] += sample.share;
        for (total, value) in colour[sample.texel].iter_mut().zip(sample.colour) {
            *total += sample.share * value;
        }
        if sample.labelled_skin {
            skin.weight[sample.texel] += sample.share;
            for (total, value) in skin.colour[sample.texel].iter_mut().zip(sample.colour) {
                *total += sample.share * value;
            }
        }
    }
    Ok(shading)
}

/// Fit a smooth shading to the skin samples and divide it out of every
/// sample, keeping the skin's mean colour and limiting highlights. Returns
/// the shading, on the corrected colours' scale, or `None` when too little
/// skin was seen to fit it.
fn remove_lighting(samples: &mut [Sample]) -> Option<Shading> {
    // Only skin the photograph's segmentation confirms, so hair or clothing
    // over the face does not enter the fit.
    let skin: Vec<usize> = (0..samples.len())
        .filter(|&at| samples[at].skin && samples[at].labelled_skin)
        .collect();
    if skin.len() < MIN_LIGHTING_SAMPLES {
        return None;
    }
    let linear: Vec<[f32; 3]> = samples
        .iter()
        .map(|sample| sample.colour.map(|value| srgb_to_linear(value / 255.0)))
        .collect();
    let basis = |normal: [f32; 3]| [1.0, normal[0], normal[1], normal[2]].map(f64::from);
    // Weighted least squares: for each channel, minimise the squared
    // difference between the colour and ambient + direction · normal.
    let mut normal_matrix = [[0.0f64; 4]; 4];
    let mut right = [[0.0f64; 4]; 3];
    for &at in &skin {
        let terms = basis(samples[at].normal);
        let share = f64::from(samples[at].share);
        for row in 0..4 {
            for column in 0..4 {
                normal_matrix[row][column] += share * terms[row] * terms[column];
            }
            for channel in 0..3 {
                right[channel][row] += share * terms[row] * f64::from(linear[at][channel]);
            }
        }
    }
    let mut shading = Shading {
        ambient: [0.0; 3],
        direction: [[0.0; 3]; 3],
    };
    for (channel, right) in right.into_iter().enumerate() {
        let solution = solve4(normal_matrix, right)?;
        shading.ambient[channel] = solution[0] as f32;
        shading.direction[channel] =
            [solution[1], solution[2], solution[3]].map(|value| value as f32);
    }
    let total: f64 = skin.iter().map(|&at| f64::from(samples[at].share)).sum();
    let average = |value: &dyn Fn(usize) -> f32| -> f64 {
        skin.iter()
            .map(|&at| f64::from(samples[at].share) * f64::from(value(at)))
            .sum::<f64>()
            / total
    };
    let floors: [f32; 3] = std::array::from_fn(|channel| {
        MIN_SHADING_SHARE * average(&|at| shading.at(channel, samples[at].normal)) as f32
    });
    if floors
        .iter()
        .any(|floor| !(floor.is_finite() && *floor > 0.0))
    {
        return None;
    }
    let corrected: Vec<[f32; 3]> = samples
        .iter()
        .zip(&linear)
        .map(|(sample, linear)| {
            std::array::from_fn(|channel| {
                linear[channel] / shading.at(channel, sample.normal).max(floors[channel])
            })
        })
        .collect();
    // Skin brighter than its 97th percentile luminance is a highlight and is
    // brought down to it. Eyes and the scalp may be brighter than skin
    // without being highlights, so they are left as they are.
    let luminance = |colour: [f32; 3]| 0.2126 * colour[0] + 0.7152 * colour[1] + 0.0722 * colour[2];
    let mut skin_luminance: Vec<f32> = skin.iter().map(|&at| luminance(corrected[at])).collect();
    skin_luminance.sort_unstable_by(f32::total_cmp);
    let rank = (HIGHLIGHT_PERCENTILE / 100.0 * (skin_luminance.len() - 1) as f64).round();
    let ceiling = skin_luminance[rank as usize];
    let limited: Vec<[f32; 3]> = samples
        .iter()
        .zip(corrected)
        .map(|(sample, colour)| {
            let level = luminance(colour);
            if sample.skin && level > ceiling && level > 0.0 {
                colour.map(|value| value * ceiling / level)
            } else {
                colour
            }
        })
        .collect();
    // Keep the skin's average colour as photographed: each channel's plain
    // mean over the fitted skin, in linear light, maps to itself. Shares,
    // which say how squarely a sample was seen, weight only the fit.
    let mean = |value: &dyn Fn(usize) -> f32| -> f64 {
        skin.iter().map(|&at| f64::from(value(at))).sum::<f64>() / skin.len() as f64
    };
    let gains: [f32; 3] = std::array::from_fn(|channel| {
        (mean(&|at| linear[at][channel]) / mean(&|at| limited[at][channel]).max(1e-9)) as f32
    });
    // Report the shading on the corrected colours' scale, as the estimate of
    // this photograph's light a caller can relight the map with.
    for (channel, gain) in gains.into_iter().enumerate() {
        shading.ambient[channel] /= gain;
        shading.direction[channel] = shading.direction[channel].map(|value| value / gain);
    }
    for (sample, colour) in samples.iter_mut().zip(limited) {
        sample.colour = std::array::from_fn(|channel| {
            linear_to_srgb((colour[channel] * gains[channel]).clamp(0.0, 1.0)) * 255.0
        });
    }
    Some(shading)
}

/// Solve a 4 by 4 linear system by Gaussian elimination with partial
/// pivoting; `None` when it is singular.
fn solve4(mut matrix: [[f64; 4]; 4], mut right: [f64; 4]) -> Option<[f64; 4]> {
    let scale = (0..4)
        .map(|index| matrix[index][index].abs())
        .fold(0.0, f64::max);
    for pivot in 0..4 {
        let best = (pivot..4)
            .max_by(|&a, &b| matrix[a][pivot].abs().total_cmp(&matrix[b][pivot].abs()))?;
        if matrix[best][pivot].abs() <= 1e-12 * scale.max(1e-300) {
            return None;
        }
        matrix.swap(pivot, best);
        right.swap(pivot, best);
        let pivot_row = matrix[pivot];
        for row in pivot + 1..4 {
            let factor = matrix[row][pivot] / pivot_row[pivot];
            for (value, above) in matrix[row].iter_mut().zip(pivot_row).skip(pivot) {
                *value -= factor * above;
            }
            right[row] -= factor * right[pivot];
        }
    }
    let mut solution = [0.0; 4];
    for row in (0..4).rev() {
        let known: f64 = (row + 1..4)
            .map(|column| matrix[row][column] * solution[column])
            .sum();
        solution[row] = (right[row] - known) / matrix[row][row];
    }
    solution
        .iter()
        .all(|value| value.is_finite())
        .then_some(solution)
}

fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f32) -> f32 {
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

/// Each texel's photographed colour from pixels labelled skin alone, as
/// weighted sums like the texel's full colour.
struct SkinSamples {
    weight: Vec<f32>,
    colour: Vec<[f32; 3]>,
}

impl SkinSamples {
    fn new(texels: usize) -> Self {
        Self {
            weight: vec![0.0; texels],
            colour: vec![[0.0; 3]; texels],
        }
    }
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
pub(crate) fn vertex_normals(faces: &[[u32; 3]], vertices: &[[f32; 3]]) -> Vec<[f32; 3]> {
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
pub(crate) fn sample(photo: &Photo<'_>, pixel: [f64; 2]) -> Option<[f32; 3]> {
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

    const KEEP: Colouring = Colouring {
        scalp: ScalpColour::Photo,
        lighting: Lighting::Keep,
    };

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
            (&[FaceKind::Skin; 2], KEEP),
            &mut weight,
            &mut colour,
            (&mut vec![false; 64 * 64], &mut SkinSamples::new(64 * 64)),
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
            (&vec![FaceKind::Skin; faces.len()], KEEP),
            &mut weight,
            &mut colour,
            (
                &mut vec![false; size * size],
                &mut SkinSamples::new(size * size),
            ),
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
            (&vec![FaceKind::Skin; faces.len()], KEEP),
            &mut weight,
            &mut colour,
            (
                &mut vec![false; size * size],
                &mut SkinSamples::new(size * size),
            ),
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
        let mut builder = ColourMapBuilder::new(
            &faces,
            &layout,
            32,
            &kinds,
            Colouring {
                scalp,
                lighting: Lighting::Keep,
            },
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
        strip_bake_views(scalp, &[([[200, 0, 0], [0, 200, 0], [0, 0, 200]], thirds)])
    }

    /// The strip of `strip_bake` photographed once for each view: the
    /// colours and labels of the photograph's three thirds.
    fn strip_bake_views(scalp: ScalpColour, views: &[([[u8; 3]; 3], [u8; 3])]) -> ColourMap {
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
        let layout = layout(coords, &faces);
        let colouring = Colouring {
            scalp,
            lighting: Lighting::Keep,
        };
        let mut builder = ColourMapBuilder::new(&faces, &layout, 48, &kinds, colouring).unwrap();
        for (colours, thirds) in views {
            let rgb: Vec<u8> = (0..224 * 224)
                .flat_map(|index| colours[third(index % 224)])
                .collect();
            let labels: Vec<u8> = (0..224 * 224)
                .map(|index| thirds[third(index % 224)])
                .collect();
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
        }
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
    fn hair_in_another_photograph_does_not_reach_the_skin_fill() {
        // The scalp is bare green skin in one photograph and black hair in
        // another; only the left skin and the bare scalp may seed unseen skin.
        let map = strip_bake_views(
            ScalpColour::Photo,
            &[
                (
                    [[0, 200, 0], [0, 200, 0], [0, 0, 200]],
                    [LABEL_FACE_SKIN, LABEL_FACE_SKIN, LABEL_BACKGROUND],
                ),
                (
                    [[0, 200, 0], [0, 0, 0], [0, 0, 200]],
                    [LABEL_FACE_SKIN, LABEL_HAIR, LABEL_BACKGROUND],
                ),
            ],
        );
        assert_eq!(texel(&map, 0.9), [0, 200, 0]);
        // The scalp itself shows both photographs.
        let scalp = texel(&map, 0.5);
        assert!(scalp[1] > 50 && scalp[1] < 150, "{scalp:?}");
    }

    #[test]
    fn unseen_skin_is_not_filled_from_the_eyes() {
        // A seen eye on the left and, apart from it, skin the photograph
        // shows only as background on the right.
        let (mut vertices, mut faces, mut coords) = square(0.0, [-1.0, 0.0], [0.0, 0.5], false);
        let (skin_vertices, skin_faces, skin_coords) = square(0.0, [0.0, 1.0], [0.5, 1.0], false);
        vertices.extend(skin_vertices);
        faces.extend(skin_faces.iter().map(|face| face.map(|vertex| vertex + 4)));
        coords.extend(skin_coords);
        let rgb: Vec<u8> = (0..224 * 224)
            .flat_map(|index| {
                if index % 224 < 112 {
                    [0, 0, 200]
                } else {
                    [200, 0, 0]
                }
            })
            .collect();
        let labels: Vec<u8> = (0..224 * 224)
            .map(|index| {
                if index % 224 < 112 {
                    LABEL_OTHER
                } else {
                    LABEL_BACKGROUND
                }
            })
            .collect();
        let layout = layout(coords, &faces);
        let kinds = [FaceKind::Eye, FaceKind::Eye, FaceKind::Skin, FaceKind::Skin];
        let mut builder = ColourMapBuilder::new(&faces, &layout, 32, &kinds, KEEP).unwrap();
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
        assert_eq!(texel(&map, 0.25), [0, 0, 200]);
        assert_eq!(texel(&map, 0.75), [128, 128, 128]);
    }

    /// A cube from -1 to 1 standing in for an eyeball, every triangle an eye:
    /// its front (+z) face covers the whole map and the others have no area
    /// in it. Front x is 2u - 1 and y is 2v - 1.
    fn eyeball() -> (Vec<[f32; 3]>, Vec<[u32; 3]>, UvLayout) {
        let vertices: Vec<[f32; 3]> = (0..8)
            .map(|corner| [0, 1, 2].map(|axis| if corner >> axis & 1 == 1 { 1.0 } else { -1.0 }))
            .collect();
        let quads = [
            [4, 5, 7, 6], // +z, the front
            [0, 2, 3, 1],
            [0, 4, 6, 2],
            [1, 3, 7, 5],
            [0, 1, 5, 4],
            [2, 6, 7, 3],
        ];
        let faces: Vec<[u32; 3]> = quads
            .iter()
            .flat_map(|[a, b, c, d]| [[*a, *b, *c], [*a, *c, *d]])
            .collect();
        // Corners 4 to 7 of the front at their u and v; everything else at the origin.
        let mut coords = vec![[0.0f32, 0.0]; 8];
        for (corner, coord) in coords.iter_mut().enumerate().skip(4) {
            *coord = [(corner & 1) as f32, (corner >> 1 & 1) as f32];
        }
        let uv_faces: Vec<[u32; 3]> = faces
            .iter()
            .enumerate()
            .map(|(index, face)| if index < 2 { *face } else { [0, 0, 0] })
            .collect();
        (vertices, faces, layout(coords, &uv_faces))
    }

    /// The eyeball cube photographed by a picture coloured and labelled by
    /// `paint` at each point of the front's x and y, painted with `iris`.
    fn painted_bake(iris: Option<[u8; 3]>, paint: impl Fn(f64, f64) -> ([u8; 3], u8)) -> ColourMap {
        let (vertices, faces, layout) = eyeball();
        // Pixel p shows x = (p - 112) / 56, and y the other way up.
        let at = |index: usize| {
            let (column, row) = ((index % 224) as f64, (index / 224) as f64);
            paint((column - 112.0) / 56.0, (112.0 - row) / 56.0)
        };
        let rgb: Vec<u8> = (0..224 * 224).flat_map(|index| at(index).0).collect();
        let labels: Vec<u8> = (0..224 * 224).map(|index| at(index).1).collect();
        let kinds = vec![FaceKind::Eye; faces.len()];
        let mut builder = ColourMapBuilder::new(&faces, &layout, 64, &kinds, KEEP).unwrap();
        builder
            .paint_eyes(
                &vertices,
                PaintedEyes {
                    iris,
                    sclera: [240, 236, 230],
                    iris_diameter: 1.0,
                    pupil_diameter: 0.4,
                },
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
        builder.finish().unwrap()
    }

    /// The map's colour at a point of the eyeball's front.
    fn front(map: &ColourMap, x: f64, y: f64) -> [u8; 3] {
        let column = ((x + 1.0) / 2.0 * map.size as f64) as usize;
        let row = ((1.0 - y) / 2.0 * map.size as f64) as usize;
        let at = row * map.size + column;
        [0, 1, 2].map(|channel| map.rgb[at * 3 + channel])
    }

    #[test]
    fn painted_eyes_show_a_pupil_an_iris_and_white() {
        let blue = [40, 80, 160];
        let map = painted_bake(Some(blue), |_, _| ([200, 150, 120], LABEL_OTHER));
        assert_eq!(map.iris, Some((blue, IrisSource::Given)));
        assert_eq!(front(&map, 0.0, 0.0), [16, 14, 14]);
        assert_eq!(front(&map, 0.33, 0.0), blue);
        assert_eq!(front(&map, 0.0, -0.33), blue);
        assert_eq!(front(&map, 0.8, 0.0), [240, 236, 230]);
        // The iris darkens toward its edge.
        let edge = front(&map, 0.0, 0.47);
        assert!(edge[2] < 130 && edge[2] > 40, "{edge:?}");
    }

    #[test]
    fn a_painted_iris_takes_the_photographed_iris_colour() {
        let green = [30, 120, 60];
        // A green iris with a white reflection in it, on a white eye.
        let map = painted_bake(None, |x, y| {
            let across = x.hypot(y);
            if (0.2..0.5).contains(&across) && !(x > 0.25 && y > 0.25) {
                (green, LABEL_OTHER)
            } else {
                ([235, 235, 235], LABEL_OTHER)
            }
        });
        assert_eq!(map.iris, Some((green, IrisSource::Measured)));
        assert_eq!(front(&map, -0.33, 0.0), green);
        // Looking aside: a dark pupil and green iris away from the gaze. The
        // iris is found where it is, and still painted on the gaze.
        let aside = painted_bake(None, |x, y| {
            let across = (x - 0.3).hypot(y - 0.1);
            if across < 0.2 {
                ([10, 10, 10], LABEL_OTHER)
            } else if across < 0.5 && !(x > 0.55 && y > 0.3) {
                (green, LABEL_OTHER)
            } else {
                ([235, 235, 235], LABEL_OTHER)
            }
        });
        assert_eq!(aside.iris, Some((green, IrisSource::Measured)));
        assert_eq!(front(&aside, 0.0, 0.0), [16, 14, 14]);
        // Hair over the iris: nothing to measure, so the default.
        let covered = painted_bake(None, |x, y| {
            if x.hypot(y) < 0.6 {
                ([20, 20, 20], LABEL_HAIR)
            } else {
                ([235, 235, 235], LABEL_OTHER)
            }
        });
        assert_eq!(covered.iris, Some((DEFAULT_IRIS_RGB, IrisSource::Default)));
        assert_eq!(front(&covered, 0.33, 0.0), DEFAULT_IRIS_RGB);
        // White with a barely darker spot is not an iris.
        let white = painted_bake(None, |x, y| {
            if x.hypot(y) < 0.05 {
                ([234, 234, 234], LABEL_OTHER)
            } else {
                ([235, 235, 235], LABEL_OTHER)
            }
        });
        assert_eq!(white.iris, Some((DEFAULT_IRIS_RGB, IrisSource::Default)));
        let unseen = {
            let (vertices, faces, layout) = eyeball();
            let kinds = vec![FaceKind::Eye; faces.len()];
            let mut builder = ColourMapBuilder::new(&faces, &layout, 64, &kinds, KEEP).unwrap();
            let eyes = PaintedEyes {
                iris: None,
                sclera: [240, 236, 230],
                iris_diameter: 1.0,
                pupil_diameter: 0.4,
            };
            builder.paint_eyes(&vertices, eyes).unwrap();
            assert_eq!(
                builder.paint_eyes(
                    &vertices,
                    PaintedEyes {
                        pupil_diameter: 1.0,
                        ..eyes
                    }
                ),
                Err(TextureError::EyeSizes)
            );
            // Seen from behind, the front is turned away.
            let behind: Vec<[f32; 3]> = vertices.iter().map(|[x, y, z]| [-*x, *y, -*z]).collect();
            let rgb = vec![120u8; 224 * 224 * 3];
            builder
                .add(&View {
                    photo: Photo {
                        width: 224,
                        height: 224,
                        rgb: &rgb,
                    },
                    vertices: &behind,
                    projection: PROJECTION,
                    labels: None,
                })
                .unwrap();
            builder.finish()
        };
        assert_eq!(unseen.unwrap_err(), TextureError::NotVisible);
    }

    #[test]
    fn texels_seen_on_each_triangle_add_up_to_seen() {
        let map = labelled_bake(FaceKind::Skin, LABEL_HAIR, ScalpColour::Photo);
        let [on_surface, seen] = map.face_texels.iter().fold([0u32; 2], |total, counts| {
            [total[0] + counts[0], total[1] + counts[1]]
        });
        assert_eq!(map.face_texels.len(), 4);
        assert!(seen > 0 && seen < on_surface, "{:?}", map.face_texels);
        assert!((f64::from(seen) / f64::from(on_surface) - map.seen).abs() < 1e-12);
        // The left square was seen and the right, under hair, was not.
        assert!(
            map.face_texels[0][1] > 0 && map.face_texels[2][1] == 0,
            "{:?}",
            map.face_texels
        );
    }

    #[test]
    fn painted_eyes_do_not_count_as_seen() {
        let painted = painted_bake(Some([40, 80, 160]), |_, _| ([200, 150, 120], LABEL_OTHER));
        assert_eq!(painted.seen, 0.0);
        let (vertices, faces, layout) = eyeball();
        let kinds = vec![FaceKind::Eye; faces.len()];
        let mut builder = ColourMapBuilder::new(&faces, &layout, 64, &kinds, KEEP).unwrap();
        let rgb = vec![120u8; 224 * 224 * 3];
        builder
            .add(&View {
                photo: Photo {
                    width: 224,
                    height: 224,
                    rgb: &rgb,
                },
                vertices: &vertices,
                projection: PROJECTION,
                labels: None,
            })
            .unwrap();
        assert!(builder.finish().unwrap().seen > 0.5);
    }

    #[test]
    fn an_eyeball_just_behind_an_eyelid_takes_no_colour_from_it() {
        // An eye square, and beside it a skin square, each half of the map,
        // with a lid square 1 mm in front of both.
        let (mut vertices, mut faces, mut coords) = square(0.0, [-1.0, 0.0], [0.0, 0.5], false);
        let (skin, skin_faces, skin_coords) = square(0.0, [0.0, 1.0], [0.5, 1.0], false);
        vertices.extend(skin);
        faces.extend(skin_faces.iter().map(|face| face.map(|vertex| vertex + 4)));
        coords.extend(skin_coords);
        let (lid, lid_faces, _) = square(0.001, [-1.0, 1.0], [0.0, 0.0], false);
        vertices.extend(lid);
        faces.extend(lid_faces.iter().map(|face| face.map(|vertex| vertex + 8)));
        coords.extend([[0.0, 0.0]; 4]);
        let kinds = [
            FaceKind::Eye,
            FaceKind::Eye,
            FaceKind::Skin,
            FaceKind::Skin,
            FaceKind::Skin,
            FaceKind::Skin,
        ];
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
        let size = 32;
        let texels = TexelMap::new(&faces, &layout(coords, &faces), size);
        let mut weight = vec![0.0; size * size];
        accumulate(
            &faces,
            &texels,
            &view,
            (&kinds, KEEP),
            &mut weight,
            &mut vec![[0.0; 3]; size * size],
            (
                &mut vec![false; size * size],
                &mut SkinSamples::new(size * size),
            ),
        )
        .unwrap();
        let at = |u: f64| (size / 2) * size + (u * size as f64) as usize;
        assert_eq!(weight[at(0.25)], 0.0, "the eyeball is hidden by the lid");
        assert!(
            weight[at(0.75)] > 0.0,
            "skin as far behind still counts as seen"
        );
    }

    #[test]
    fn a_photograph_without_labels_cannot_show_bare_scalp() {
        let (vertices, faces, coords) = square(0.0, [-1.0, 1.0], [0.0, 1.0], false);
        let rgb: Vec<u8> = (0..224 * 224).flat_map(|_| [200, 0, 0]).collect();
        let layout = layout(coords, &faces);
        let paint = [10, 200, 30];
        let mut builder = ColourMapBuilder::new(
            &faces,
            &layout,
            32,
            &[FaceKind::Scalp; 2],
            Colouring {
                scalp: ScalpColour::Hair(paint),
                lighting: Lighting::Keep,
            },
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
                labels: None,
            })
            .unwrap();
        let map = builder.finish().unwrap();
        // The paint covers everything the photograph saw.
        assert_eq!(map.seen, 0.0);
        assert_eq!(texel(&map, 0.5), paint);
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
            Colouring {
                scalp: ScalpColour::Hair([10, 200, 30]),
                lighting: Lighting::Keep,
            },
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
    fn skin_beside_hair_is_not_recorded_as_skin() {
        // A photographed scalp, skin on the left of the photograph and hair
        // on the right: every pixel colours it, but only skin clear of the
        // hair counts as skin.
        let (vertices, faces, coords) = square(0.0, [-1.0, 1.0], [0.0, 1.0], false);
        let rgb = gradient();
        let labels: Vec<u8> = (0..224 * 224)
            .map(|index| {
                if index % 224 < 112 {
                    LABEL_FACE_SKIN
                } else {
                    LABEL_HAIR
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
            labels: Some(Labels {
                side: 224,
                data: &labels,
                left: 0.0,
                top: 0.0,
                step: 1.0,
            }),
        };
        let size = 64;
        let texels = TexelMap::new(&faces, &layout(coords, &faces), size);
        let mut weight = vec![0.0; size * size];
        let mut colour = vec![[0.0; 3]; size * size];
        let mut skin = SkinSamples::new(size * size);
        accumulate(
            &faces,
            &texels,
            &view,
            (&[FaceKind::Scalp; 2], KEEP),
            &mut weight,
            &mut colour,
            (&mut vec![false; size * size], &mut skin),
        )
        .unwrap();
        // Texel column 31 lands on photograph column 111, the last of the skin.
        let (clear, edge) = (32 * size + 20, 32 * size + 31);
        assert!(weight[edge] > 0.0 && skin.weight[edge] == 0.0);
        assert!(skin.weight[clear] > 0.0);
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

    /// A dome of the unit sphere over x and y from -0.7 to 0.7, facing the
    /// camera, with texture coordinates spread over the map.
    fn dome() -> Square {
        let steps = 24;
        let mut vertices = Vec::new();
        let mut coords = Vec::new();
        for row in 0..=steps {
            for column in 0..=steps {
                let x = -0.7 + 1.4 * column as f32 / steps as f32;
                let y = -0.7 + 1.4 * row as f32 / steps as f32;
                vertices.push([x, y, (1.0 - x * x - y * y).sqrt()]);
                coords.push([(x + 0.7) / 1.4, (y + 0.7) / 1.4]);
            }
        }
        let at = |row: usize, column: usize| (row * (steps + 1) + column) as u32;
        let mut faces = Vec::new();
        for row in 0..steps {
            for column in 0..steps {
                let (a, b) = (at(row, column), at(row, column + 1));
                let (c, d) = (at(row + 1, column + 1), at(row + 1, column));
                faces.extend([[a, b, c], [a, c, d]]);
            }
        }
        (vertices, faces, coords)
    }

    /// A photograph of the sphere, of linear albedo 0.6, lit by an ambient
    /// light and one from the upper right.
    fn lit_sphere() -> Vec<u8> {
        let light = {
            let raw = [1.0f32, 0.3, 1.0];
            let length = (raw[0] * raw[0] + raw[1] * raw[1] + raw[2] * raw[2]).sqrt();
            raw.map(|axis| axis / length)
        };
        (0..224 * 224)
            .flat_map(|index| {
                let (column, row) = ((index % 224) as f32, (index / 224) as f32);
                let x = (column / 112.0 - 1.0) / 0.5;
                let y = (1.0 - row / 112.0) / 0.5;
                let squared = x * x + y * y;
                if squared >= 1.0 {
                    return [0u8; 3];
                }
                let normal = [x, y, (1.0 - squared).sqrt()];
                let lit =
                    (normal[0] * light[0] + normal[1] * light[1] + normal[2] * light[2]).max(0.0);
                let value = linear_to_srgb(0.6 * (0.25 + 0.75 * lit)) * 255.0;
                [value.round() as u8; 3]
            })
            .collect()
    }

    fn lit_bake(lighting: Lighting) -> ColourMap {
        let (vertices, faces, coords) = dome();
        let rgb = lit_sphere();
        let layout = layout(coords, &faces);
        let kinds = vec![FaceKind::Skin; faces.len()];
        let colouring = Colouring {
            scalp: ScalpColour::Photo,
            lighting,
        };
        let mut builder = ColourMapBuilder::new(&faces, &layout, 64, &kinds, colouring).unwrap();
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
                    data: &[LABEL_FACE_SKIN; 224 * 224],
                    left: 0.0,
                    top: 0.0,
                    step: 1.0,
                }),
            })
            .unwrap();
        builder.finish().unwrap()
    }

    /// The spread and mean of the red channel over the map's middle, which
    /// the photograph sees.
    fn spread_and_mean(map: &ColourMap) -> (f64, f64) {
        let values: Vec<f64> = (16..48)
            .flat_map(|row| (16..48).map(move |column| row * 64 + column))
            .map(|at| f64::from(map.rgb[at * 3]))
            .collect();
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let spread = values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>()
            / values.len() as f64;
        (spread.sqrt(), mean)
    }

    #[test]
    fn removing_lighting_evens_out_shading_and_keeps_the_skin_tone() {
        let kept = lit_bake(Lighting::Keep);
        let removed = lit_bake(Lighting::Remove);
        assert_eq!(kept.shading, vec![None]);
        let shading = removed.shading[0].expect("shading was estimated");
        // The light comes from +x and +z.
        assert!(
            shading.direction[0][0] > 0.0 && shading.direction[0][2] > 0.0,
            "{shading:?}"
        );
        let (kept_spread, kept_mean) = spread_and_mean(&kept);
        let (removed_spread, removed_mean) = spread_and_mean(&removed);
        assert!(kept_spread > 8.0, "{kept_spread}");
        assert!(
            removed_spread < 0.25 * kept_spread,
            "{removed_spread} {kept_spread}"
        );
        assert!(
            (removed_mean - kept_mean).abs() < 0.15 * kept_mean,
            "{removed_mean} {kept_mean}"
        );
    }

    /// Skin samples lit from +z: 60% facing away from the light and 40%
    /// facing it, with normals spread around each direction so that the
    /// shading can be fitted.
    fn lit_samples() -> Vec<Sample> {
        (0..500)
            .map(|texel| {
                let turn = (texel % 7) as f32 * 0.1 - 0.3;
                let normal = if texel % 5 < 3 {
                    normalize3([turn, 0.3, 0.2])
                } else {
                    normalize3([turn, -0.2, 1.0])
                };
                let light = 0.1 + 0.5 * normal[2].max(0.0);
                Sample {
                    texel,
                    // As in a photograph, surface seen at an angle counts less.
                    share: normal[2].max(0.05),
                    colour: [light; 3].map(|value| linear_to_srgb(value) * 255.0),
                    normal,
                    skin: true,
                    labelled_skin: true,
                }
            })
            .collect()
    }

    fn normalize3(vector: [f32; 3]) -> [f32; 3] {
        let length = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        vector.map(|value| value / length)
    }

    fn linear_mean(samples: &[Sample], keep: impl Fn(&Sample) -> bool) -> f32 {
        let chosen: Vec<f32> = samples
            .iter()
            .filter(|sample| keep(sample))
            .map(|sample| srgb_to_linear(sample.colour[0] / 255.0))
            .collect();
        chosen.iter().sum::<f32>() / chosen.len() as f32
    }

    #[test]
    fn removing_lighting_keeps_the_average_skin_colour() {
        let mut samples = lit_samples();
        let before = linear_mean(&samples, |_| true);
        remove_lighting(&mut samples).expect("shading was estimated");
        let after = linear_mean(&samples, |_| true);
        assert!((after - before).abs() < 0.01 * before, "{before} {after}");
        // The light is gone: every sample has about the same colour.
        let spread = samples
            .iter()
            .map(|sample| (srgb_to_linear(sample.colour[0] / 255.0) - after).abs())
            .fold(0.0, f32::max);
        assert!(spread < 0.05 * after, "{spread} {after}");
    }

    #[test]
    fn the_reported_shading_relights_the_corrected_samples() {
        let mut samples = lit_samples();
        let photographed: Vec<f32> = samples
            .iter()
            .map(|sample| srgb_to_linear(sample.colour[0] / 255.0))
            .collect();
        let shading = remove_lighting(&mut samples).expect("shading was estimated");
        // Samples brought down as highlights cannot be relit exactly.
        let relit = samples
            .iter()
            .zip(&photographed)
            .filter(|(sample, photographed)| {
                let relit = srgb_to_linear(sample.colour[0] / 255.0) * shading.at(0, sample.normal);
                (relit - **photographed).abs() < 0.02 * **photographed
            })
            .count();
        assert!(
            relit > 9 * samples.len() / 10,
            "{relit} of {}",
            samples.len()
        );
    }

    #[test]
    fn shading_turns_back_into_the_head_frame() {
        // A head turned a quarter turn about y faces the camera's +x; light
        // from the camera's +x comes from straight in front of the head.
        let shading = Shading {
            ambient: [0.2; 3],
            direction: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        };
        let back = shading.turned_back([0.0, std::f32::consts::FRAC_PI_2, 0.0]);
        let close = |a: [f32; 3], b: [f32; 3]| (0..3).all(|axis| (a[axis] - b[axis]).abs() < 1e-6);
        assert!(close(back.direction[0], [0.0, 0.0, 1.0]), "{back:?}");
        assert!(close(back.direction[1], [0.0, 1.0, 0.0]), "{back:?}");
        assert!(close(back.direction[2], [-1.0, 0.0, 0.0]), "{back:?}");
        assert_eq!(back.ambient, shading.ambient);
        assert_eq!(shading.turned_back([0.0; 3]), shading);
    }

    #[test]
    fn unsegmented_skin_leaves_the_lighting_alone() {
        let mut samples = lit_samples();
        for sample in &mut samples {
            sample.labelled_skin = false;
        }
        let colours: Vec<[f32; 3]> = samples.iter().map(|sample| sample.colour).collect();
        assert_eq!(remove_lighting(&mut samples), None);
        assert!(
            samples
                .iter()
                .zip(colours)
                .all(|(sample, colour)| sample.colour == colour)
        );
    }

    #[test]
    fn eyes_brighter_than_skin_are_not_highlights() {
        let mut samples = lit_samples();
        // A sclera three times as bright as the skin, under the same light.
        let skin = &samples[499];
        let sclera = Sample {
            texel: 500,
            share: 1.0,
            colour: skin
                .colour
                .map(|value| linear_to_srgb(3.0 * srgb_to_linear(value / 255.0)) * 255.0),
            normal: skin.normal,
            skin: false,
            labelled_skin: false,
        };
        samples.push(sclera);
        remove_lighting(&mut samples).expect("shading was estimated");
        let brightest_skin = samples[..500]
            .iter()
            .map(|sample| sample.colour[0])
            .fold(0.0, f32::max);
        let sclera = samples[500].colour;
        // Capped, it would be about as bright as the brightest skin.
        assert!(
            sclera[0] > 1.3 * brightest_skin,
            "{sclera:?} {brightest_skin}"
        );
    }

    #[test]
    fn too_little_skin_leaves_the_lighting_alone() {
        let mut samples: Vec<Sample> = (0..10)
            .map(|texel| Sample {
                texel,
                share: 1.0,
                colour: [100.0, 80.0, 60.0],
                normal: [0.0, 0.0, 1.0],
                skin: true,
                labelled_skin: true,
            })
            .collect();
        assert_eq!(remove_lighting(&mut samples), None);
        assert!(
            samples
                .iter()
                .all(|sample| sample.colour == [100.0, 80.0, 60.0])
        );
    }

    #[test]
    fn small_systems_are_solved_and_singular_ones_refused() {
        let matrix = [
            [4.0, 1.0, 0.0, 0.0],
            [1.0, 3.0, 1.0, 0.0],
            [0.0, 1.0, 2.0, 1.0],
            [0.0, 0.0, 1.0, 5.0],
        ];
        let expected = [1.0, -2.0, 0.5, 3.0];
        let right: [f64; 4] = std::array::from_fn(|row| {
            (0..4)
                .map(|column| matrix[row][column] * expected[column])
                .sum()
        });
        let solved = solve4(matrix, right).unwrap();
        for (value, wanted) in solved.iter().zip(expected) {
            assert!((value - wanted).abs() < 1e-9, "{solved:?}");
        }
        assert_eq!(solve4([[1.0; 4]; 4], [1.0; 4]), None);
    }

    #[test]
    fn a_label_at_the_corner_of_a_sample_is_checked() {
        // Labels one to one with the photograph: skin, with hair at one
        // pixel that a sample between it and its diagonal neighbour blends.
        let mut data = vec![LABEL_FACE_SKIN; 200 * 200];
        data[101 * 200 + 101] = LABEL_HAIR;
        let labels = Labels {
            side: 200,
            data: &data,
            left: 0.0,
            top: 0.0,
            step: 1.0,
        };
        let skin = |label| label == LABEL_FACE_SKIN;
        assert!(!labels.agree([100.49, 100.49], skin));
        assert!(labels.agree([50.5, 50.5], skin));
        // Labels finer than the photograph are checked across a whole
        // photograph pixel.
        let fine = Labels {
            side: 200,
            data: &data,
            left: 0.0,
            top: 0.0,
            step: 4.0,
        };
        // Label pixel 101 lies at photograph pixel 25.25.
        assert!(!fine.agree([25.1, 25.1], skin));
        assert!(fine.agree([10.5, 10.5], skin));
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
            ColourMapBuilder::new(&faces, &layout, 16, &[FaceKind::Skin], KEEP).err(),
            Some(TextureError::Kinds)
        );
        let mut builder =
            ColourMapBuilder::new(&faces, &layout, 16, &[FaceKind::Skin; 2], KEEP).unwrap();
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
