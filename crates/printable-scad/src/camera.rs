//! The named-view camera table for `openscad --camera`.
//!
//! `--camera` takes `tx,ty,tz,rx,ry,rz,distance`. Renders use `--autocenter
//! --viewall`, which auto-scales the distance, so only the rotation selects the
//! view and the distance is left at `0`. These names and angles are the stable
//! Printable camera presets.

/// The `--camera` string for each named view, in the service's order.
pub const VIEW_CAMERAS: &[(&str, &str)] = &[
    ("iso", "0,0,0,55,0,25,0"),
    ("front", "0,0,0,90,0,0,0"),
    ("back", "0,0,0,90,0,180,0"),
    ("right", "0,0,0,90,0,90,0"),
    ("left", "0,0,0,90,0,270,0"),
    ("top", "0,0,0,0,0,0,0"),
    ("bottom", "0,0,0,180,0,0,0"),
];

/// The `--camera` string for `view`, or `None` if the view name is unknown.
pub fn camera(view: &str) -> Option<&'static str> {
    VIEW_CAMERAS
        .iter()
        .find(|(name, _)| *name == view)
        .map(|(_, cam)| *cam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_table_snapshot() {
        assert_eq!(camera("iso"), Some("0,0,0,55,0,25,0"));
        assert_eq!(camera("front"), Some("0,0,0,90,0,0,0"));
        assert_eq!(camera("back"), Some("0,0,0,90,0,180,0"));
        assert_eq!(camera("right"), Some("0,0,0,90,0,90,0"));
        assert_eq!(camera("left"), Some("0,0,0,90,0,270,0"));
        assert_eq!(camera("top"), Some("0,0,0,0,0,0,0"));
        assert_eq!(camera("bottom"), Some("0,0,0,180,0,0,0"));
    }

    #[test]
    fn unknown_view_is_none() {
        assert_eq!(camera("perspective"), None);
        assert_eq!(camera(""), None);
    }

    #[test]
    fn table_has_the_seven_known_views() {
        assert_eq!(VIEW_CAMERAS.len(), 7);
    }
}
