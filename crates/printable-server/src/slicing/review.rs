use super::{SliceHandle, invalid, slice_error};
use crate::error::ToolError;
use image::{Rgb, RgbImage};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    path::Path,
};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewParams {
    pub slice: SliceHandle,
    /// Exact G-code filename from the completed slice's artifacts.
    pub toolpath: String,
    /// One-based inclusive layer range. A narrow range gives the clearest image.
    pub first_layer: u32,
    pub last_layer: u32,
    #[serde(default)]
    pub features: Vec<String>,
    pub material: Option<u16>,
    #[serde(default)]
    pub include_travel: bool,
    #[serde(default = "default_size")]
    pub size: u16,
}
fn default_size() -> u16 {
    768
}

#[derive(Clone)]
struct Segment {
    from: [f64; 3],
    to: [f64; 3],
    feature: String,
    material: u16,
    travel: bool,
}

struct Parser {
    position: [f64; 4],
    relative: bool,
    relative_e: bool,
    layer: u32,
    feature: String,
    material: u16,
    layer_count: u32,
    segments: Vec<Segment>,
    lengths: BTreeMap<String, f64>,
    estimated_time: Option<String>,
    estimated_filament: Option<String>,
}

impl Parser {
    fn new() -> Self {
        Self {
            position: [0.; 4],
            relative: false,
            relative_e: false,
            layer: 0,
            feature: "unknown".into(),
            material: 0,
            layer_count: 0,
            segments: Vec::new(),
            lengths: BTreeMap::new(),
            estimated_time: None,
            estimated_filament: None,
        }
    }
    fn line(&mut self, text: &str, request: &ReviewParams) -> Result<(), ToolError> {
        let trimmed = text.trim();
        if trimmed == "; CHANGE_LAYER" || trimmed == ";LAYER_CHANGE" {
            self.layer += 1;
            self.layer_count = self.layer;
        }
        for marker in ["; FEATURE:", ";TYPE:"] {
            if let Some(feature) = trimmed.strip_prefix(marker) {
                self.feature = feature.trim().chars().take(128).collect();
            }
        }
        if let Some(value) = trimmed.strip_prefix("; estimated printing time (normal mode) =") {
            self.estimated_time = Some(value.trim().chars().take(128).collect());
        }
        if let Some(value) = trimmed.strip_prefix("; filament used [g] =") {
            self.estimated_filament = Some(value.trim().chars().take(128).collect());
        }
        let code = trimmed.split(';').next().unwrap_or("");
        let mut words = code.split_ascii_whitespace();
        let Some(command) = words.next() else {
            return Ok(());
        };
        match command {
            "G90" => self.relative = false,
            "G91" => self.relative = true,
            "M82" => self.relative_e = false,
            "M83" => self.relative_e = true,
            "G20" => {
                return Err(slice_error(
                    "inch G-code is unsupported for toolpath review",
                ));
            }
            "G18" | "G19" => {
                return Err(slice_error(
                    "non-XY arc planes are unsupported for toolpath review",
                ));
            }
            _ if command.starts_with('T') => {
                if let Ok(tool) = command[1..].parse::<u16>()
                    && tool < 16
                {
                    self.material = tool;
                }
            }
            "G0" | "G00" | "G1" | "G01" | "G2" | "G02" | "G3" | "G03" | "G92" => {
                let mut values = BTreeMap::new();
                for word in words {
                    if word.len() < 2 {
                        continue;
                    }
                    let axis = word.as_bytes()[0];
                    if !b"XYZEIJR".contains(&axis) {
                        continue;
                    }
                    let value = word[1..]
                        .parse::<f64>()
                        .map_err(|_| slice_error("invalid numeric toolpath coordinate"))?;
                    if !value.is_finite() || value.abs() > 1e7 {
                        return Err(slice_error(
                            "toolpath coordinate is outside the review range",
                        ));
                    }
                    values.insert(axis, value);
                }
                let before = self.position;
                for (index, axis) in b"XYZE".iter().enumerate() {
                    if let Some(value) = values.get(axis) {
                        self.position[index] = if command == "G92" {
                            *value
                        } else if if index == 3 {
                            self.relative_e
                        } else {
                            self.relative
                        } {
                            before[index] + value
                        } else {
                            *value
                        };
                    }
                }
                if command == "G92" {
                    return Ok(());
                }
                let travel = self.position[3] <= before[3];
                let from = [before[0], before[1], before[2]];
                let to = [self.position[0], self.position[1], self.position[2]];
                let arc = matches!(command, "G2" | "G02" | "G3" | "G03");
                if arc {
                    if values.contains_key(&b'R')
                        || (!values.contains_key(&b'I') && !values.contains_key(&b'J'))
                    {
                        return Err(slice_error(
                            "toolpath review requires XY arcs expressed using I/J centers",
                        ));
                    }
                    let center = [
                        from[0] + values.get(&b'I').copied().unwrap_or(0.),
                        from[1] + values.get(&b'J').copied().unwrap_or(0.),
                    ];
                    let radius = (from[0] - center[0]).hypot(from[1] - center[1]);
                    let end_radius = (to[0] - center[0]).hypot(to[1] - center[1]);
                    if radius == 0. || (radius - end_radius).abs() > 0.1 {
                        return Err(slice_error("arc geometry is inconsistent"));
                    }
                    let start = (from[1] - center[1]).atan2(from[0] - center[0]);
                    let end = (to[1] - center[1]).atan2(to[0] - center[0]);
                    let ccw = matches!(command, "G3" | "G03");
                    let mut sweep = if ccw {
                        (end - start).rem_euclid(std::f64::consts::TAU)
                    } else {
                        -(start - end).rem_euclid(std::f64::consts::TAU)
                    };
                    if sweep.abs() < 1e-9 {
                        sweep = if ccw {
                            std::f64::consts::TAU
                        } else {
                            -std::f64::consts::TAU
                        };
                    }
                    let steps = (radius * sweep.abs() / 0.2).ceil().max(1.) as usize;
                    if steps > 20000 {
                        return Err(slice_error("arc exceeds bounded review complexity"));
                    }
                    let mut previous = from;
                    for step in 1..=steps {
                        let fraction = step as f64 / steps as f64;
                        let angle = start + sweep * fraction;
                        let next = if step == steps {
                            to
                        } else {
                            [
                                center[0] + radius * angle.cos(),
                                center[1] + radius * angle.sin(),
                                from[2] + (to[2] - from[2]) * fraction,
                            ]
                        };
                        self.segment(previous, next, travel, request)?;
                        previous = next;
                    }
                } else {
                    self.segment(from, to, travel, request)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn segment(
        &mut self,
        from: [f64; 3],
        to: [f64; 3],
        travel: bool,
        request: &ReviewParams,
    ) -> Result<(), ToolError> {
        if self.layer < request.first_layer
            || self.layer > request.last_layer
            || (travel && !request.include_travel)
            || request.material.is_some_and(|x| x != self.material)
            || (!request.features.is_empty() && !request.features.contains(&self.feature))
        {
            return Ok(());
        }
        if from == to {
            return Ok(());
        }
        if self.segments.len() >= 2_000_000 {
            return Err(slice_error(
                "review selection is too large; select fewer layers or features",
            ));
        }
        let length = (0..3)
            .map(|i| (to[i] - from[i]).powi(2))
            .sum::<f64>()
            .sqrt();
        *self
            .lengths
            .entry(if travel {
                "travel".into()
            } else {
                self.feature.clone()
            })
            .or_default() += length;
        self.segments.push(Segment {
            from,
            to,
            feature: self.feature.clone(),
            material: self.material,
            travel,
        });
        Ok(())
    }
}

pub fn render(source: &Path, request: &ReviewParams) -> Result<(Vec<u8>, Value), ToolError> {
    if request.first_layer == 0
        || request.last_layer < request.first_layer
        || request.last_layer - request.first_layer > 1000
        || !(128..=2048).contains(&request.size)
        || request.features.len() > 32
    {
        return Err(invalid(
            "review requires a one-based range of at most 1001 layers, size 128–2048, and at most 32 feature filters",
        ));
    }
    let mut parser = Parser::new();
    let mut reader = BufReader::new(std::fs::File::open(source)?);
    let mut line = String::new();
    loop {
        line.clear();
        let n =
            std::io::Read::take(std::io::Read::by_ref(&mut reader), 65537).read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if n > 65536 {
            return Err(slice_error("toolpath line exceeds review limit"));
        }
        parser.line(&line, request)?;
    }
    if parser.segments.is_empty() {
        return Err(slice_error(
            "no toolpath segments match the selected layers, features and material",
        ));
    }
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for segment in &parser.segments {
        for point in [segment.from, segment.to] {
            for i in 0..3 {
                min[i] = min[i].min(point[i]);
                max[i] = max[i].max(point[i]);
            }
        }
    }
    let size = request.size as u32;
    let scale = (size as f64 - 32.) / (max[0] - min[0]).max(max[1] - min[1]).max(0.01);
    let map = |point: [f64; 3]| {
        [
            (16. + (point[0] - min[0]) * scale).round() as i32,
            (size as f64 - 17. - (point[1] - min[1]) * scale).round() as i32,
        ]
    };
    let mut image = RgbImage::from_pixel(size, size, Rgb([20, 24, 30]));
    let palette = [
        [61, 174, 233],
        [255, 179, 71],
        [137, 218, 132],
        [215, 130, 233],
        [242, 112, 112],
        [99, 219, 207],
    ];
    let mut legend = BTreeMap::new();
    for segment in &parser.segments {
        let key = if segment.travel {
            "travel".into()
        } else {
            format!("{} / material {}", segment.feature, segment.material)
        };
        let index = legend.len();
        let color = *legend.entry(key).or_insert(if segment.travel {
            [85, 90, 99]
        } else {
            palette[index % palette.len()]
        });
        draw_line(&mut image, map(segment.from), map(segment.to), Rgb(color));
    }
    let mut output = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(|_| slice_error("cannot encode toolpath review"))?;
    Ok((
        output.into_inner(),
        json!({"kind":"actual_toolpath","projection":"top_xy","units":"mm","layers":parser.layer_count,
        "selected_layers":[request.first_layer,request.last_layer],"segments":parser.segments.len(),"bounds_mm":{"min":min,"max":max},
        "path_length_mm_by_feature":parser.lengths,"legend_rgb":legend,"estimates":{"time":parser.estimated_time,"filament_grams":parser.estimated_filament},
        "arc_sampling_max_path_mm":0.2,"physical_result":"not_observed"}),
    ))
}

fn draw_line(image: &mut RgbImage, from: [i32; 2], to: [i32; 2], color: Rgb<u8>) {
    let [mut x, mut y] = from;
    let dx = (to[0] - x).abs();
    let sx = if x < to[0] { 1 } else { -1 };
    let dy = -(to[1] - y).abs();
    let sy = if y < to[1] { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        if x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
            image.put_pixel(x as u32, y as u32, color);
        }
        if x == to[0] && y == to[1] {
            break;
        }
        let twice = 2 * error;
        if twice >= dy {
            error += dy;
            x += sx;
        }
        if twice <= dx {
            error += dx;
            y += sy;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review_tracks_relative_extrusion_resets_arcs_layers_and_features() {
        let request = ReviewParams {
            slice: SliceHandle {
                project_id: "part".into(),
                output_dir: "slice".into(),
            },
            toolpath: "plate_1.gcode".into(),
            first_layer: 1,
            last_layer: 1,
            features: vec![],
            material: None,
            include_travel: false,
            size: 128,
        };
        let mut parser = Parser::new();
        for line in [
            "G90",
            "M83",
            "G1 X0 Y0 Z0.2",
            "; CHANGE_LAYER",
            "; FEATURE: Outer wall",
            "G1 X10 E1",
            "G3 X0 Y10 I-10 J0 E1",
            "G92 E0",
            "; CHANGE_LAYER",
            "G1 X20 E1",
        ] {
            parser.line(line, &request).unwrap();
        }
        assert_eq!(parser.layer_count, 2);
        assert!((parser.lengths["Outer wall"] - (10. + 5. * std::f64::consts::PI)).abs() < 0.01);
        assert!(
            parser
                .segments
                .iter()
                .all(|s| s.to[0] <= 10. && s.to[1] <= 10.)
        );
    }
}
