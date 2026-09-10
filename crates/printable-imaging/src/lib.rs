//! Render compositing.
//!
//! Assembles labeled tile grids (multi-view renders, turntables,
//! cross-section galleries) and before/after panels from decoded RGB images,
//! with label text drawn from a font bundled into the binary so labels remain
//! legible and consistent in every deployment.
//!
//! The crate is pure: it composites already-decoded [`image::RgbImage`]s and
//! returns an `RgbImage`, leaving base64/PNG transport to the caller. The
//! colours, label-bar height, tile defaults, and Lanczos resampling form the
//! stable visual style for Printable composites.

use std::sync::OnceLock;

use ab_glyph::{FontRef, PxScale};
use image::RgbImage;
use image::imageops::{FilterType, replace, resize};
use imageproc::drawing::{draw_filled_rect_mut, draw_text_mut};
use imageproc::rect::Rect;

/// Stable compositing constants for visually consistent output.
pub mod constants {
    use image::Rgb;

    /// Canvas background behind the tiles.
    pub const CANVAS_BG: Rgb<u8> = Rgb([30, 30, 40]);
    /// Fill of the label bar above each tile / panel.
    pub const LABEL_BAR: Rgb<u8> = Rgb([20, 20, 35]);
    /// Height of the label bar, in pixels.
    pub const LABEL_BAR_HEIGHT: u32 = 28;
    /// Tile-grid label text colour (cyan).
    pub const TILE_LABEL: Rgb<u8> = Rgb([0, 200, 200]);
    /// `BEFORE` panel label text colour (yellow).
    pub const BEFORE_LABEL: Rgb<u8> = Rgb([200, 200, 0]);
    /// `AFTER` panel label text colour (green).
    pub const AFTER_LABEL: Rgb<u8> = Rgb([0, 200, 100]);
    /// Default number of grid columns.
    pub const DEFAULT_COLUMNS: u32 = 3;
    /// Default tile edge (width and height), in pixels.
    pub const DEFAULT_TILE: u32 = 400;
    /// Label text size, in pixels.
    pub const FONT_SIZE: f32 = 16.0;
    /// Label text inset from the tile's top-left corner, in pixels.
    pub const TEXT_INSET_X: i32 = 8;
    /// Label text inset from the tile's top-left corner, in pixels.
    pub const TEXT_INSET_Y: i32 = 4;
}

const FONT_BYTES: &[u8] = include_bytes!("../assets/FiraSans-Regular.ttf");

/// The bundled label font, parsed once. Panics only if the compiled-in asset is
/// not a valid font — a build/packaging error, never a runtime condition.
fn font() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| {
        FontRef::try_from_slice(FONT_BYTES)
            .expect("bundled FiraSans-Regular.ttf is a valid TrueType font")
    })
}

/// One labeled tile: a caption and its already-decoded image (any size — it is
/// resized to the tile dimensions during compositing).
pub struct Tile {
    pub label: String,
    pub image: RgbImage,
}

/// Grid geometry for a tile composite.
#[derive(Debug, Clone, Copy)]
pub struct TileLayout {
    pub columns: u32,
    pub tile_w: u32,
    pub tile_h: u32,
    pub label_height: u32,
}

impl Default for TileLayout {
    fn default() -> Self {
        TileLayout {
            columns: constants::DEFAULT_COLUMNS,
            tile_w: constants::DEFAULT_TILE,
            tile_h: constants::DEFAULT_TILE,
            label_height: constants::LABEL_BAR_HEIGHT,
        }
    }
}

/// Panel geometry for a before/after composite.
#[derive(Debug, Clone, Copy)]
pub struct PanelLayout {
    pub width: u32,
    pub height: u32,
    pub label_height: u32,
}

impl Default for PanelLayout {
    fn default() -> Self {
        PanelLayout {
            width: constants::DEFAULT_TILE,
            height: constants::DEFAULT_TILE,
            label_height: constants::LABEL_BAR_HEIGHT,
        }
    }
}

/// The computed geometry of a tile grid: overall canvas size, the column/row
/// count, and the top-left origin of each tile (its label bar). Pure and cheap,
/// so the layout can be asserted without rendering any pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridGeometry {
    pub width: u32,
    pub height: u32,
    pub cols: u32,
    pub rows: u32,
    pub origins: Vec<(u32, u32)>,
}

/// Compute the grid geometry for `n` tiles under `layout`. `n == 0` yields a
/// zero-sized canvas with no origins. The column count is clamped to `n` so a
/// short final row does not leave empty columns wider than the content.
pub fn grid_geometry(n: usize, layout: &TileLayout) -> GridGeometry {
    if n == 0 {
        return GridGeometry {
            width: 0,
            height: 0,
            cols: 0,
            rows: 0,
            origins: Vec::new(),
        };
    }
    let n = n as u32;
    let cols = layout.columns.min(n).max(1);
    let rows = n.div_ceil(cols);
    let cell_h = layout.tile_h + layout.label_height;
    let origins = (0..n)
        .map(|i| (i % cols * layout.tile_w, i / cols * cell_h))
        .collect();
    GridGeometry {
        width: cols * layout.tile_w,
        height: rows * cell_h,
        cols,
        rows,
        origins,
    }
}

/// Composite `tiles` into a single labeled grid: each tile's image is resized
/// (Lanczos3) to the tile dimensions and pasted below a label bar bearing its
/// caption. Returns a 0×0 image when `tiles` is empty (mirrors the service's
/// empty result).
pub fn tile_images(tiles: &[Tile], layout: &TileLayout) -> RgbImage {
    if tiles.is_empty() {
        return RgbImage::new(0, 0);
    }
    let geo = grid_geometry(tiles.len(), layout);
    let mut canvas = RgbImage::from_pixel(geo.width, geo.height, constants::CANVAS_BG);
    let scale = PxScale::from(constants::FONT_SIZE);
    for (tile, &(x, y)) in tiles.iter().zip(&geo.origins) {
        draw_filled_rect_mut(
            &mut canvas,
            Rect::at(x as i32, y as i32).of_size(layout.tile_w, layout.label_height),
            constants::LABEL_BAR,
        );
        draw_text_mut(
            &mut canvas,
            constants::TILE_LABEL,
            x as i32 + constants::TEXT_INSET_X,
            y as i32 + constants::TEXT_INSET_Y,
            scale,
            font(),
            &tile.label,
        );
        let img = resize(
            &tile.image,
            layout.tile_w,
            layout.tile_h,
            FilterType::Lanczos3,
        );
        replace(
            &mut canvas,
            &img,
            x as i64,
            (y + layout.label_height) as i64,
        );
    }
    canvas
}

/// Composite a before/after pair into a two-panel image with `BEFORE` (yellow)
/// and `AFTER` (green) labels. Both images are resized (Lanczos3) to the panel
/// dimensions and pasted below their label bars.
pub fn side_by_side(before: &RgbImage, after: &RgbImage, layout: &PanelLayout) -> RgbImage {
    let width = layout.width * 2;
    let height = layout.height + layout.label_height;
    let mut canvas = RgbImage::from_pixel(width, height, constants::CANVAS_BG);
    let scale = PxScale::from(constants::FONT_SIZE);
    let panels = [
        (before, "BEFORE", constants::BEFORE_LABEL),
        (after, "AFTER", constants::AFTER_LABEL),
    ];
    for (i, (img, label, color)) in panels.into_iter().enumerate() {
        let x = i as u32 * layout.width;
        draw_filled_rect_mut(
            &mut canvas,
            Rect::at(x as i32, 0).of_size(layout.width, layout.label_height),
            constants::LABEL_BAR,
        );
        draw_text_mut(
            &mut canvas,
            color,
            x as i32 + constants::TEXT_INSET_X,
            constants::TEXT_INSET_Y,
            scale,
            font(),
            label,
        );
        let resized = resize(img, layout.width, layout.height, FilterType::Lanczos3);
        replace(&mut canvas, &resized, x as i64, layout.label_height as i64);
    }
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn solid(w: u32, h: u32, color: Rgb<u8>) -> RgbImage {
        RgbImage::from_pixel(w, h, color)
    }

    #[test]
    fn grid_geometry_empty_is_zero_sized() {
        let geo = grid_geometry(0, &TileLayout::default());
        assert_eq!(
            geo,
            GridGeometry {
                width: 0,
                height: 0,
                cols: 0,
                rows: 0,
                origins: vec![]
            }
        );
    }

    #[test]
    fn grid_geometry_single_tile_is_one_cell() {
        // One tile: a single column, canvas is exactly one cell (tile + label).
        let geo = grid_geometry(1, &TileLayout::default());
        assert_eq!(geo.cols, 1);
        assert_eq!(geo.rows, 1);
        assert_eq!((geo.width, geo.height), (400, 428));
        assert_eq!(geo.origins, vec![(0, 0)]);
    }

    #[test]
    fn grid_geometry_four_tiles_wraps_to_two_rows() {
        // 4 tiles at 3 columns → 3×2 grid; canvas 1200×856; the 4th wraps.
        let geo = grid_geometry(4, &TileLayout::default());
        assert_eq!((geo.cols, geo.rows), (3, 2));
        assert_eq!((geo.width, geo.height), (1200, 856));
        assert_eq!(geo.origins, vec![(0, 0), (400, 0), (800, 0), (0, 428)]);
    }

    #[test]
    fn tile_images_dimensions_and_paste() {
        // Two red tiles → canvas is one row of two cells. The label bar shows
        // the bar colour; the image region below shows the (resized) tile.
        let tiles = vec![
            Tile {
                label: "front".into(),
                image: solid(8, 8, Rgb([255, 0, 0])),
            },
            Tile {
                label: "iso".into(),
                image: solid(8, 8, Rgb([255, 0, 0])),
            },
        ];
        let canvas = tile_images(&tiles, &TileLayout::default());
        assert_eq!(canvas.dimensions(), (800, 428));
        // Top-left is inside the label bar of tile 0.
        assert_eq!(*canvas.get_pixel(0, 0), constants::LABEL_BAR);
        // A point below the label bar, inside tile 0's image, is the tile colour.
        assert_eq!(*canvas.get_pixel(200, 200), Rgb([255, 0, 0]));
    }

    #[test]
    fn tile_images_empty_is_zero_sized() {
        assert_eq!(
            tile_images(&[], &TileLayout::default()).dimensions(),
            (0, 0)
        );
    }

    #[test]
    fn side_by_side_dimensions_and_panels() {
        let before = solid(8, 8, Rgb([255, 0, 0]));
        let after = solid(8, 8, Rgb([0, 0, 255]));
        let canvas = side_by_side(&before, &after, &PanelLayout::default());
        // Two panels wide, one panel tall plus the label bar.
        assert_eq!(canvas.dimensions(), (800, 428));
        assert_eq!(*canvas.get_pixel(0, 0), constants::LABEL_BAR);
        // Left panel image → red; right panel image → blue.
        assert_eq!(*canvas.get_pixel(200, 200), Rgb([255, 0, 0]));
        assert_eq!(*canvas.get_pixel(600, 200), Rgb([0, 0, 255]));
    }

    #[test]
    fn bundled_font_parses() {
        // The compiled-in font must be a valid TrueType face.
        let _ = font();
    }
}
