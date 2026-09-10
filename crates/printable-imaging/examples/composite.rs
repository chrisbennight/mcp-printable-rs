//! Generate sample composites for eyeball review.
//!
//! `cargo run -p printable-imaging --example composite` writes a tile grid and
//! a before/after panel to the temp directory and prints their paths, so the
//! label rendering, colours, and layout can be checked visually. The automated
//! tests assert geometry only, not text pixels; this is the human check.

use image::{Rgb, RgbImage};
use printable_imaging::{PanelLayout, Tile, TileLayout, side_by_side, tile_images};

fn solid(color: [u8; 3]) -> RgbImage {
    RgbImage::from_pixel(64, 64, Rgb(color))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tiles = [
        ("front", [200, 60, 60]),
        ("iso", [60, 200, 60]),
        ("right", [60, 60, 200]),
        ("top", [200, 200, 60]),
        ("back", [200, 60, 200]),
    ]
    .map(|(label, color)| Tile {
        label: label.to_string(),
        image: solid(color),
    });

    let grid = tile_images(&tiles, &TileLayout::default());
    let panel = side_by_side(
        &solid([180, 60, 60]),
        &solid([60, 180, 120]),
        &PanelLayout::default(),
    );

    let dir = std::env::temp_dir();
    let grid_path = dir.join("printable-imaging-tiles.png");
    let panel_path = dir.join("printable-imaging-before-after.png");
    grid.save(&grid_path)?;
    panel.save(&panel_path)?;

    println!("wrote tile grid:     {}", grid_path.display());
    println!("wrote before/after:  {}", panel_path.display());
    Ok(())
}
