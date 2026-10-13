//! Pictures and measures of how a fitted head lines up with its photograph.
//!
//! A fit picture shows the photograph with the fitted head's outline and the
//! edges between its named parts drawn over it, beside the head alone, shaded
//! grey, as the photograph's camera sees it. Both halves are cropped to the
//! head. The measures compare the head's projection with the photograph's
//! labels: how much of the head lands on the person rather than the
//! background, and how much of the photographed face skin the head covers.

use crate::texture::{
    LABEL_BACKGROUND, LABEL_FACE_SKIN, Labels, Photo, Projection, edge, sample, vertex_normals,
};

/// Share of the head's projected size added on every side when cropping.
const CROP_MARGIN: f64 = 0.15;
/// Space between the two halves of a picture, in pixels.
const GAP_PIXELS: usize = 6;
const OUTLINE_RGB: [u8; 3] = [40, 230, 90];
const PART_EDGE_RGB: [u8; 3] = [250, 210, 40];
const BACKGROUND_RGB: [u8; 3] = [235, 235, 235];
/// The grey head is lit from in front of the face, a little above and to its
/// right, with this much light from everywhere.
const LIGHT: [f64; 3] = [0.3, 0.5, 1.0];
const AMBIENT: f64 = 0.25;
const NO_FACE: u32 = u32::MAX;

/// An RGB picture, three bytes per pixel, row by row from the top.
#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

/// One photograph with the head posed to match it.
pub struct FitView<'a> {
    pub photo: Photo<'a>,
    /// Posed head vertices in metres, one per model vertex.
    pub vertices: &'a [[f32; 3]],
    pub projection: Projection,
}

/// How the head's projection compares with a photograph's labels, each a
/// share from 0 to 1 within the labelled square.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitMeasures {
    /// Share of the head's projected area labelled as part of the person
    /// rather than background. Low when the head spills past the person.
    pub head_on_person: f64,
    /// Share of the pixels labelled face skin that the head covers. Low when
    /// the head is too small or misplaced.
    pub face_skin_covered: f64,
}

/// The nearest triangle at each pixel of a grid, with the barycentric
/// weights of its first two corners there.
struct Coverage {
    width: usize,
    face: Vec<u32>,
    weights: Vec<[f32; 2]>,
}

impl Coverage {
    /// Rasterize triangles whose corners are at `projected`, grid pixel
    /// coordinates with centres on integers and a depth that grows toward the
    /// camera.
    fn render(
        faces: &[[u32; 3]],
        projected: &[([f64; 2], f32)],
        width: usize,
        height: usize,
    ) -> Self {
        let mut depth = vec![f32::NEG_INFINITY; width * height];
        let mut face_at = vec![NO_FACE; width * height];
        let mut weights = vec![[0.0f32; 2]; width * height];
        for (index, face) in faces.iter().enumerate() {
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
                    if wa < 0.0 || wb < 0.0 || wc < 0.0 {
                        continue;
                    }
                    let value =
                        (wa * f64::from(da) + wb * f64::from(db) + wc * f64::from(dc)) as f32;
                    let at = row * width + column;
                    if value > depth[at] {
                        depth[at] = value;
                        face_at[at] = index as u32;
                        weights[at] = [wa as f32, wb as f32];
                    }
                }
            }
        }
        Self {
            width,
            face: face_at,
            weights,
        }
    }

    fn covered(&self, column: usize, row: usize) -> bool {
        self.face[row * self.width + column] != NO_FACE
    }
}

/// The picture of one photograph's fit, at most `max_height` pixels high.
/// `parts` names the part of the head each triangle belongs to; edges where
/// visible triangles of different parts meet are drawn, such as the rims of
/// the eyes and lips. Give every triangle the same part to draw only the
/// outline.
pub fn fit_picture(
    faces: &[[u32; 3]],
    parts: &[u16],
    view: &FitView<'_>,
    max_height: usize,
) -> Picture {
    let projected: Vec<([f64; 2], f32)> = view
        .vertices
        .iter()
        .map(|vertex| view.projection.project(*vertex))
        .collect();
    // The crop, in photograph pixels, and the scale to the picture.
    let (mut low, mut high) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for (point, _) in &projected {
        for axis in 0..2 {
            low[axis] = low[axis].min(point[axis]);
            high[axis] = high[axis].max(point[axis]);
        }
    }
    let size = (high[0] - low[0]).max(high[1] - low[1]).max(1.0);
    let origin = low.map(|value| value - CROP_MARGIN * size);
    let extent = [0, 1].map(|axis| high[axis] - low[axis] + 2.0 * CROP_MARGIN * size);
    let scale = (max_height.max(1) as f64 / extent[1]).min(1.0);
    let width = ((extent[0] * scale).ceil() as usize).max(1);
    let height = ((extent[1] * scale).ceil() as usize).max(1);
    let to_picture = |point: [f64; 2]| [0, 1].map(|axis| (point[axis] - origin[axis]) * scale);
    let placed: Vec<([f64; 2], f32)> = projected
        .iter()
        .map(|(point, depth)| (to_picture(*point), *depth))
        .collect();
    let coverage = Coverage::render(faces, &placed, width, height);
    let normals = vertex_normals(faces, view.vertices);
    let light = {
        let length = LIGHT.iter().map(|value| value * value).sum::<f64>().sqrt();
        LIGHT.map(|value| value / length)
    };

    let total = 2 * width + GAP_PIXELS;
    let mut rgb = vec![255u8; total * height * 3];
    let mut put = |column: usize, row: usize, colour: [u8; 3]| {
        let at = (row * total + column) * 3;
        rgb[at..at + 3].copy_from_slice(&colour);
    };
    for row in 0..height {
        for column in 0..width {
            let at = row * width + column;
            // Left: the photograph, with the outline and part edges.
            let photo_pixel = [0, 1].map(|axis| [column, row][axis] as f64 / scale + origin[axis]);
            let photographed = sample(&view.photo, photo_pixel).map_or(BACKGROUND_RGB, |colour| {
                colour.map(|value| value.round() as u8)
            });
            let face = coverage.face[at];
            let neighbours = [
                (column.wrapping_sub(1), row),
                (column + 1, row),
                (column, row.wrapping_sub(1)),
                (column, row + 1),
            ];
            let mut outline = false;
            let mut part_edge = false;
            if face != NO_FACE {
                for (x, y) in neighbours {
                    if x >= width || y >= height || !coverage.covered(x, y) {
                        outline = true;
                    } else if parts[coverage.face[y * width + x] as usize] != parts[face as usize] {
                        part_edge = true;
                    }
                }
            }
            put(
                column,
                row,
                if outline {
                    OUTLINE_RGB
                } else if part_edge {
                    PART_EDGE_RGB
                } else {
                    photographed
                },
            );
            // Right: the head alone, shaded grey.
            let shaded = if face == NO_FACE {
                BACKGROUND_RGB
            } else {
                let corners = faces[face as usize];
                let [wa, wb] = coverage.weights[at];
                let shares = [wa, wb, 1.0 - wa - wb];
                let mut normal = [0.0f64; 3];
                for (corner, share) in corners.iter().zip(shares) {
                    for (axis, value) in normal.iter_mut().enumerate() {
                        *value += f64::from(share) * f64::from(normals[*corner as usize][axis]);
                    }
                }
                let length = normal.iter().map(|value| value * value).sum::<f64>().sqrt();
                let facing = if length > 0.0 {
                    normal
                        .iter()
                        .zip(light)
                        .map(|(value, light)| value / length * light)
                        .sum::<f64>()
                        .max(0.0)
                } else {
                    0.0
                };
                let level = (255.0 * (AMBIENT + (1.0 - AMBIENT) * facing)).round() as u8;
                [level; 3]
            };
            put(width + GAP_PIXELS + column, row, shaded);
        }
    }
    Picture {
        width: total,
        height,
        rgb,
    }
}

/// Compare the head's projection with a photograph's labels, pixel by pixel
/// over the labelled square. `None` when the head covers none of the square
/// or the photograph shows no face skin there.
pub fn fit_measures(
    faces: &[[u32; 3]],
    vertices: &[[f32; 3]],
    projection: Projection,
    labels: &Labels<'_>,
) -> Option<FitMeasures> {
    // Label pixel p lies at photograph pixel p / step + (left, top).
    let placed: Vec<([f64; 2], f32)> = vertices
        .iter()
        .map(|vertex| {
            let (pixel, depth) = projection.project(*vertex);
            (
                [
                    (pixel[0] - labels.left) * labels.step,
                    (pixel[1] - labels.top) * labels.step,
                ],
                depth,
            )
        })
        .collect();
    let coverage = Coverage::render(faces, &placed, labels.side, labels.side);
    let (mut head, mut head_on_person, mut skin, mut skin_covered) =
        (0usize, 0usize, 0usize, 0usize);
    for (at, &label) in labels.data.iter().enumerate() {
        let covered = coverage.face[at] != NO_FACE;
        if covered {
            head += 1;
            head_on_person += usize::from(label != LABEL_BACKGROUND);
        }
        if label == LABEL_FACE_SKIN {
            skin += 1;
            skin_covered += usize::from(covered);
        }
    }
    (head > 0 && skin > 0).then(|| FitMeasures {
        head_on_person: head_on_person as f64 / head as f64,
        face_skin_covered: skin_covered as f64 / skin as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Half scale, so x and y from -1 to 1 span photograph pixels 56 to 168
    /// of a 224 pixel photograph that is the crop itself.
    const PROJECTION: Projection = Projection {
        scale: 0.5,
        offset: [0.0, 0.0],
        crop_left: 0.0,
        crop_top: 0.0,
        crop_step: 1.0,
    };

    /// Two squares facing the camera side by side, x from -1 to 0 and 0 to
    /// 1, y from -1 to 1.
    fn squares() -> (Vec<[f32; 3]>, Vec<[u32; 3]>) {
        let vertices = vec![
            [-1.0, -1.0, 0.0],
            [0.0, -1.0, 0.0],
            [0.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
        ];
        let faces = vec![[0, 1, 2], [0, 2, 3], [1, 4, 5], [1, 5, 2]];
        (vertices, faces)
    }

    /// Red rises with the column and green with the row.
    fn gradient() -> Vec<u8> {
        (0..224 * 224)
            .flat_map(|index| [(index % 224) as u8, (index / 224) as u8, 0])
            .collect()
    }

    fn pixel(picture: &Picture, column: usize, row: usize) -> [u8; 3] {
        let at = (row * picture.width + column) * 3;
        [0, 1, 2].map(|channel| picture.rgb[at + channel])
    }

    #[test]
    fn the_picture_lines_up_with_the_photograph() {
        let (vertices, faces) = squares();
        let rgb = gradient();
        let view = FitView {
            photo: Photo {
                width: 224,
                height: 224,
                rgb: &rgb,
            },
            vertices: &vertices,
            projection: PROJECTION,
        };
        let picture = fit_picture(&faces, &[0, 0, 1, 1], &view, 1000);
        // The head spans 112 pixels, with 15% more on each side: the crop
        // starts at photograph pixel 56 - 16.8 and is 145.6 pixels across, at
        // full size.
        let half = 146;
        assert_eq!(
            (picture.width, picture.height),
            (2 * half + GAP_PIXELS, half)
        );
        // Picture pixel p shows photograph pixel p + 39.2.
        let row = 72; // photograph row 111.2, inside the head
        // The head's edges lie on photograph columns 56 and 168, picture
        // columns 16.8 and 128.8: the first and last covered are 17 and 128.
        assert_eq!(pixel(&picture, 16, row), [55, 111, 0]);
        assert_eq!(pixel(&picture, 17, row), OUTLINE_RGB);
        assert_eq!(pixel(&picture, 128, row), OUTLINE_RGB);
        assert_eq!(pixel(&picture, 129, row), [168, 111, 0]);
        // Inside, the photograph shows through where it lies.
        assert_eq!(pixel(&picture, 39, row), [78, 111, 0]);
        assert_eq!(pixel(&picture, 60, row), [99, 111, 0]);
        // The parts meet at x = 0, photograph column 112, picture column 72.8.
        assert!(
            (72..=73).any(|column| pixel(&picture, column, row) == PART_EDGE_RGB),
            "{:?}",
            (71..=74)
                .map(|column| pixel(&picture, column, row))
                .collect::<Vec<_>>()
        );
        // Beside it, the head alone: lit grey where it faces the camera.
        let light = LIGHT[2] / LIGHT.iter().map(|value| value * value).sum::<f64>().sqrt();
        let level = (255.0 * (AMBIENT + (1.0 - AMBIENT) * light)).round() as u8;
        let right = half + GAP_PIXELS;
        assert_eq!(pixel(&picture, right + 60, row), [level; 3]);
        assert_eq!(pixel(&picture, right + 10, row), BACKGROUND_RGB);
        // A lower limit scales the picture down.
        let small = fit_picture(&faces, &[0; 4], &view, 73);
        assert_eq!(small.height, 73);
    }

    #[test]
    fn measures_compare_the_head_with_the_labels() {
        let (vertices, faces) = squares();
        // Labels over the whole photograph: face skin within `skin`, the
        // person within `person`, background elsewhere.
        let measure = |skin: [usize; 2], person: [usize; 2]| {
            let data: Vec<u8> = (0..224 * 224)
                .map(|index| {
                    let column = index % 224;
                    if (skin[0]..skin[1]).contains(&column) {
                        LABEL_FACE_SKIN
                    } else if (person[0]..person[1]).contains(&column) {
                        crate::texture::LABEL_HAIR
                    } else {
                        LABEL_BACKGROUND
                    }
                })
                .collect();
            let labels = Labels {
                side: 224,
                data: &data,
                left: 0.0,
                top: 0.0,
                step: 1.0,
            };
            fit_measures(&faces, &vertices, PROJECTION, &labels)
        };
        // The head covers columns 56 to 168 of rows 56 to 168.
        let exact = measure([56, 169], [0, 0]).unwrap();
        assert!(exact.head_on_person > 0.99, "{exact:?}");
        // Skin rows run the full height, so the head covers half of them.
        assert!(
            (exact.face_skin_covered - 113.0 / 224.0).abs() < 0.01,
            "{exact:?}"
        );
        // Half the head over the background.
        let shifted = measure([112, 169], [112, 224]).unwrap();
        assert!((shifted.head_on_person - 0.5).abs() < 0.02, "{shifted:?}");
        // No face skin, nothing to measure.
        assert_eq!(measure([0, 0], [0, 224]), None);
    }
}
