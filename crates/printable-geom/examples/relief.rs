//! Turn a depth map, and optionally normals, a mask and a confidence map,
//! into a relief height field, relief map and STL solid, timing each stage.
//!
//! Inputs are raw little-endian arrays, row by row from the top-left pixel:
//! depth and confidence as 32-bit floats, normals as three 32-bit floats per
//! pixel (x right, y up, z towards the camera), and the mask as one byte per
//! pixel, non-zero for the subject. NumPy writes them with
//! `array.astype("<f4").tofile(path)` and `mask.astype("u1").tofile(path)`.
//!
//! ```sh
//! cargo run --release -p printable-geom --example relief -- \
//!     --size 1024 768 --depth depth.f32 --normals normals.f32 \
//!     --mask mask.u8 --focal 1100 --method anchored \
//!     --heights-out heights.f32 --relief-out relief.f32 --stl relief.stl
//! ```
//!
//! `--heights-out` writes the integrated height field in the depth map's
//! units, nearer being higher, as 32-bit floats; `--relief-out` writes the
//! compressed relief in millimetres. `--synthetic N` replaces the inputs with
//! a generated N x N scene, a sphere before a wall with 0.4 % depth noise,
//! for timing. `--check` runs the printability checks on the solid.

use std::error::Error;
use std::fs;
use std::io::Read;
use std::time::Instant;

use printable_geom::relief::{
    Analysis, CompressionOptions, DEFAULT_MAX_PIXELS, Grid, IntegrationMethod, IntegrationOptions,
    PrintabilityOptions, ReliefMap, SolidOptions, check_relief, compress, integrate, relief_solid,
};

#[derive(Default)]
struct Arguments {
    size: Option<(usize, usize)>,
    synthetic: Option<usize>,
    depth: Option<String>,
    normals: Option<String>,
    mask: Option<String>,
    confidence: Option<String>,
    focal: Option<f64>,
    log_depth: bool,
    method: IntegrationMethod,
    width_mm: f64,
    relief_mm: f64,
    base_mm: f64,
    mould: bool,
    max_pixels: Option<usize>,
    heights_out: Option<String>,
    relief_out: Option<String>,
    stl: Option<String>,
    check: bool,
}

fn parse() -> Result<Arguments, Box<dyn Error>> {
    let mut arguments = Arguments {
        width_mm: 100.0,
        relief_mm: 3.0,
        base_mm: 2.0,
        ..Arguments::default()
    };
    let mut values = std::env::args().skip(1);
    let mut next = |name: &str| values.next().ok_or_else(|| format!("{name} needs a value"));
    while let Ok(flag) = next("an option") {
        match flag.as_str() {
            "--size" => {
                let width = next("--size")?.parse()?;
                let height = next("--size")?.parse()?;
                arguments.size = Some((width, height));
            }
            "--synthetic" => arguments.synthetic = Some(next(&flag)?.parse()?),
            "--depth" => arguments.depth = Some(next(&flag)?),
            "--normals" => arguments.normals = Some(next(&flag)?),
            "--mask" => arguments.mask = Some(next(&flag)?),
            "--confidence" => arguments.confidence = Some(next(&flag)?),
            "--focal" => arguments.focal = Some(next(&flag)?.parse()?),
            "--log-depth" => arguments.log_depth = true,
            "--method" => {
                arguments.method = match next(&flag)?.as_str() {
                    "depth-only" => IntegrationMethod::DepthOnly,
                    "frankot-chellappa" => IntegrationMethod::FrankotChellappa,
                    "anchored" => IntegrationMethod::AnchoredNormals,
                    other => return Err(format!("unknown method {other}").into()),
                }
            }
            "--width-mm" => arguments.width_mm = next(&flag)?.parse()?,
            "--relief-mm" => arguments.relief_mm = next(&flag)?.parse()?,
            "--base-mm" => arguments.base_mm = next(&flag)?.parse()?,
            "--mould" => arguments.mould = true,
            "--max-pixels" => arguments.max_pixels = Some(next(&flag)?.parse()?),
            "--heights-out" => arguments.heights_out = Some(next(&flag)?),
            "--relief-out" => arguments.relief_out = Some(next(&flag)?),
            "--stl" => arguments.stl = Some(next(&flag)?),
            "--check" => arguments.check = true,
            other => return Err(format!("unknown option {other}").into()),
        }
    }
    Ok(arguments)
}

/// The pixels in a `width` x `height` map, refused above `limit` before
/// anything that size is allocated or read.
fn pixels_within(width: usize, height: usize, limit: usize) -> Result<usize, Box<dyn Error>> {
    match width.checked_mul(height) {
        Some(pixels) if pixels <= limit => Ok(pixels),
        _ => Err(format!("{width} x {height} pixels exceed the limit of {limit}").into()),
    }
}

/// A file's bytes, refused unless it holds exactly `expected` bytes.
fn read_exactly(path: &str, expected: usize) -> Result<Vec<u8>, Box<dyn Error>> {
    let file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    if length != expected as u64 {
        return Err(format!("{path} has {length} bytes, expected {expected}").into());
    }
    let mut bytes = Vec::with_capacity(expected);
    file.take(expected as u64).read_to_end(&mut bytes)?;
    if bytes.len() != expected {
        return Err(format!("{path} changed while it was read").into());
    }
    Ok(bytes)
}

fn read_f32(path: &str, count: usize) -> Result<Vec<f32>, Box<dyn Error>> {
    let bytes = read_exactly(path, 4 * count)?;
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

fn write_f32(path: &str, values: impl Iterator<Item = f64>) -> Result<(), Box<dyn Error>> {
    let bytes: Vec<u8> = values
        .flat_map(|value| (value as f32).to_le_bytes())
        .collect();
    fs::write(path, bytes)?;
    Ok(())
}

/// A sphere of radius 0.35 one unit before the camera, against a wall 1.6
/// units away, with exact normals and multiplicative depth noise.
fn synthetic(size: usize, limit: usize) -> Result<Analysis, Box<dyn Error>> {
    pixels_within(size, size, limit)?;
    let focal = size as f64;
    let centre = (size - 1) as f64 / 2.0;
    let (radius, distance, wall) = (0.35, 1.0, 1.6);
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut depth = Vec::with_capacity(size * size);
    let mut normals = Vec::with_capacity(size * size);
    let mut mask = Vec::with_capacity(size * size);
    for i in 0..size * size {
        let x = ((i % size) as f64 - centre) / focal;
        let y = ((i / size) as f64 - centre) / focal;
        let a = x * x + y * y + 1.0;
        let discriminant =
            4.0 * distance * distance - 4.0 * a * (distance * distance - radius * radius);
        let (z, n, inside) = if discriminant > 0.0 {
            let z = (2.0 * distance - discriminant.sqrt()) / (2.0 * a);
            (
                z,
                [x * z / radius, -y * z / radius, -(z - distance) / radius],
                true,
            )
        } else {
            (wall, [0.0, 0.0, 1.0], false)
        };
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let noise = (state >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
        depth.push((z * (1.0 + 0.004 * noise)) as f32);
        normals.push(n.map(|value| value as f32));
        mask.push(inside);
    }
    Ok(Analysis {
        depth: Grid::new(size, size, depth)?,
        normals: Some(Grid::new(size, size, normals)?),
        mask: Some(Grid::new(size, size, mask)?),
        confidence: None,
        focal_length_px: Some(focal),
    })
}

fn load(arguments: &Arguments) -> Result<Analysis, Box<dyn Error>> {
    let limit = arguments.max_pixels.unwrap_or(DEFAULT_MAX_PIXELS);
    if let Some(size) = arguments.synthetic {
        return synthetic(size, limit);
    }
    let (width, height) = arguments.size.ok_or("--size is required")?;
    let pixels = pixels_within(width, height, limit)?;
    let depth_path = arguments.depth.as_deref().ok_or("--depth is required")?;
    let mut depth = read_f32(depth_path, pixels)?;
    if arguments.log_depth {
        depth.iter_mut().for_each(|value| *value = value.exp());
    }
    let normals = match &arguments.normals {
        Some(path) => {
            let flat = read_f32(path, 3 * pixels)?;
            let normals = flat.chunks_exact(3).map(|n| [n[0], n[1], n[2]]).collect();
            Some(Grid::new(width, height, normals)?)
        }
        None => None,
    };
    let mask = match &arguments.mask {
        Some(path) => {
            let bytes = read_exactly(path, pixels)?;
            Some(Grid::new(
                width,
                height,
                bytes.iter().map(|b| *b != 0).collect(),
            )?)
        }
        None => None,
    };
    let confidence = match &arguments.confidence {
        Some(path) => Some(Grid::new(width, height, read_f32(path, pixels)?)?),
        None => None,
    };
    Ok(Analysis {
        depth: Grid::new(width, height, depth)?,
        normals,
        mask,
        confidence,
        focal_length_px: arguments.focal,
    })
}

/// Peak resident memory where the system reports it to the process.
fn peak_memory() -> Option<String> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .map(|line| line.trim_start_matches("VmHWM:").trim().to_string())
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = parse()?;
    let started = Instant::now();
    let analysis = load(&arguments)?;
    println!(
        "input: {} x {} pixels, {:.2} s",
        analysis.depth.width(),
        analysis.depth.height(),
        started.elapsed().as_secs_f64()
    );

    let stage = Instant::now();
    let mut options = IntegrationOptions {
        method: arguments.method,
        ..IntegrationOptions::default()
    };
    if let Some(max_pixels) = arguments.max_pixels {
        options.max_pixels = max_pixels;
    }
    let field = integrate(&analysis, &options)?;
    drop(analysis);
    println!(
        "integrate: {:?}, {:.2} s",
        field.report,
        stage.elapsed().as_secs_f64()
    );
    if let Some(path) = &arguments.heights_out {
        write_f32(path, field.heights.values().iter().copied())?;
    }

    let stage = Instant::now();
    let (relief, report) = compress(
        &field,
        &CompressionOptions::new(arguments.width_mm, arguments.relief_mm),
    )?;
    drop(field);
    println!(
        "compress: {report:?}, {:.2} s",
        stage.elapsed().as_secs_f64()
    );
    if let Some(path) = &arguments.relief_out {
        write_f32(path, relief.heights_mm.values().iter().copied())?;
    }

    let stage = Instant::now();
    let solid_options = if arguments.mould {
        SolidOptions::mould()
    } else {
        SolidOptions::default()
    };
    let solid = relief_solid(
        &relief,
        &SolidOptions {
            base_mm: arguments.base_mm,
            ..solid_options
        },
    )?;
    println!(
        "solid: {} vertices, {} triangles, {:.2} s",
        solid.mesh().vertices.len(),
        solid.mesh().triangles.len(),
        stage.elapsed().as_secs_f64()
    );

    if arguments.check {
        let stage = Instant::now();
        // A mould prints the relief upside down, so its ridges are the
        // relief's grooves.
        let inverted;
        let printed = if arguments.mould {
            let high = relief
                .heights_mm
                .values()
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max);
            inverted = ReliefMap {
                heights_mm: Grid::new(
                    relief.heights_mm.width(),
                    relief.heights_mm.height(),
                    relief
                        .heights_mm
                        .values()
                        .iter()
                        .map(|value| high - value)
                        .collect(),
                )?,
                spacing_mm: relief.spacing_mm,
            };
            &inverted
        } else {
            &relief
        };
        let check = check_relief(printed, &solid, &PrintabilityOptions::default())?;
        println!(
            "check: solid {}, passed {}, relief {:.3} mm, volume {:?} mm3, narrow ridges {:?}, narrow grooves {:?}, {:.2} s",
            check.solid,
            check.passed,
            check.relief_height_mm,
            check.volume_mm3,
            check.narrow_ridges,
            check.narrow_grooves,
            stage.elapsed().as_secs_f64()
        );
    }

    if let Some(path) = &arguments.stl {
        let solid = solid.mesh();
        let triangles: Vec<stl_io::Triangle> = solid
            .triangles
            .iter()
            .map(|triangle| {
                let [a, b, c] = triangle.map(|index| solid.vertices[index as usize]);
                let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
                let n = [
                    u[1] * v[2] - u[2] * v[1],
                    u[2] * v[0] - u[0] * v[2],
                    u[0] * v[1] - u[1] * v[0],
                ];
                let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2])
                    .sqrt()
                    .max(f64::MIN_POSITIVE);
                stl_io::Triangle {
                    normal: stl_io::Normal::new(n.map(|value| (value / length) as f32)),
                    vertices: [a, b, c].map(|p| stl_io::Vertex::new(p.map(|value| value as f32))),
                }
            })
            .collect();
        let mut file = fs::File::create(path)?;
        stl_io::write_stl(&mut file, triangles.iter())?;
    }
    println!("total: {:.2} s", started.elapsed().as_secs_f64());
    if let Some(peak) = peak_memory() {
        println!("peak resident memory: {peak}");
    }
    Ok(())
}
