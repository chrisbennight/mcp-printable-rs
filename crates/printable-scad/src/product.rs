//! Trusted product-design profiles and the bundled `product_v1` OpenSCAD kit.

use crate::{GateError, Token, tokenize};

pub const PRODUCT_V1_SOURCE: &str = include_str!("../assets/product_v1.scad");
pub const PRODUCT_V1_CALLER_FILE: &str = "caller.scad";
pub const PRODUCT_V1_KIT_FILE: &str = "product_v1.scad";

#[derive(Clone, Debug, PartialEq)]
pub struct ManufacturingProfile {
    pub nozzle_diameter_mm: f64,
    pub layer_height_mm: f64,
    pub minimum_wall_mm: f64,
    pub moving_clearance_mm: f64,
    pub maximum_overhang_degrees: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FormProfile {
    pub primary_radius_mm: f64,
    pub secondary_radius_mm: f64,
    pub edge_break_mm: f64,
    pub transition_length_mm: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProductProfile {
    pub manufacturing: ManufacturingProfile,
    pub form: FormProfile,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProductProfileError {
    #[error("product profile {0} must be a positive finite number")]
    InvalidDimension(&'static str),
    #[error("maximum_overhang_degrees must be at most 90")]
    InvalidOverhang,
    #[error("form radii must satisfy primary_radius_mm >= secondary_radius_mm >= edge_break_mm")]
    InvalidRadiusHierarchy,
}

impl ProductProfile {
    pub fn validate(&self) -> Result<(), ProductProfileError> {
        for (name, value) in [
            ("nozzle_diameter_mm", self.manufacturing.nozzle_diameter_mm),
            ("layer_height_mm", self.manufacturing.layer_height_mm),
            ("minimum_wall_mm", self.manufacturing.minimum_wall_mm),
            (
                "moving_clearance_mm",
                self.manufacturing.moving_clearance_mm,
            ),
            (
                "maximum_overhang_degrees",
                self.manufacturing.maximum_overhang_degrees,
            ),
            ("primary_radius_mm", self.form.primary_radius_mm),
            ("secondary_radius_mm", self.form.secondary_radius_mm),
            ("edge_break_mm", self.form.edge_break_mm),
            ("transition_length_mm", self.form.transition_length_mm),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(ProductProfileError::InvalidDimension(name));
            }
        }
        if self.manufacturing.maximum_overhang_degrees > 90.0 {
            return Err(ProductProfileError::InvalidOverhang);
        }
        if self.form.primary_radius_mm < self.form.secondary_radius_mm
            || self.form.secondary_radius_mm < self.form.edge_break_mm
        {
            return Err(ProductProfileError::InvalidRadiusHierarchy);
        }
        Ok(())
    }
}

pub fn product_v1_wrapper() -> String {
    format!("include <{PRODUCT_V1_KIT_FILE}>\ninclude <{PRODUCT_V1_CALLER_FILE}>\n")
}

pub fn validate_product_v1_caller(code: &str) -> Result<(), GateError> {
    let tokens = tokenize(code)?;
    for declaration in tokens.windows(2) {
        let [Token::Ident(kind), Token::Ident(name)] = [&declaration[0].0, &declaration[1].0]
        else {
            continue;
        };
        if (kind == "module" || kind == "function")
            && (name.starts_with("pbl_") || name.starts_with("_pbl_"))
        {
            return Err(GateError::ReservedProductSymbol(name.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> ProductProfile {
        ProductProfile {
            manufacturing: ManufacturingProfile {
                nozzle_diameter_mm: 0.4,
                layer_height_mm: 0.2,
                minimum_wall_mm: 2.0,
                moving_clearance_mm: 0.35,
                maximum_overhang_degrees: 45.0,
            },
            form: FormProfile {
                primary_radius_mm: 3.0,
                secondary_radius_mm: 1.5,
                edge_break_mm: 0.5,
                transition_length_mm: 8.0,
            },
        }
    }

    #[test]
    fn validates_explicit_positive_profile_and_radius_hierarchy() {
        assert_eq!(profile().validate(), Ok(()));
        let mut boundary = profile();
        boundary.manufacturing.maximum_overhang_degrees = 90.0;
        boundary.form.primary_radius_mm = 0.5;
        boundary.form.secondary_radius_mm = 0.5;
        assert_eq!(boundary.validate(), Ok(()));

        let mut invalid = profile();
        invalid.manufacturing.layer_height_mm = 0.0;
        assert_eq!(
            invalid.validate(),
            Err(ProductProfileError::InvalidDimension("layer_height_mm"))
        );
        invalid.manufacturing.layer_height_mm = f64::NAN;
        assert_eq!(
            invalid.validate(),
            Err(ProductProfileError::InvalidDimension("layer_height_mm"))
        );
        let mut invalid = profile();
        invalid.manufacturing.maximum_overhang_degrees = 90.1;
        assert_eq!(
            invalid.validate(),
            Err(ProductProfileError::InvalidOverhang)
        );
        let mut invalid = profile();
        invalid.form.secondary_radius_mm = 3.1;
        assert_eq!(
            invalid.validate(),
            Err(ProductProfileError::InvalidRadiusHierarchy)
        );
        let mut invalid = profile();
        invalid.form.edge_break_mm = 2.0;
        assert_eq!(
            invalid.validate(),
            Err(ProductProfileError::InvalidRadiusHierarchy)
        );
    }

    #[test]
    fn wrapper_loads_only_trusted_staged_files() {
        assert_eq!(
            product_v1_wrapper(),
            "include <product_v1.scad>\ninclude <caller.scad>\n"
        );
    }

    #[test]
    fn caller_can_use_but_cannot_redeclare_product_symbols() {
        let valid = r#"
pbl_variant = "default";
module enclosure() {
    pbl_shell(size=[60, 40, 20], wall=pbl_minimum_wall_mm);
}
function product_scale(value) = value * pbl_primary_radius_mm;
"#;
        assert_eq!(validate_product_v1_caller(valid), Ok(()));
        assert_eq!(
            validate_product_v1_caller(
                "module /* comments cannot hide the declaration */ pbl_shell() {}"
            ),
            Err(GateError::ReservedProductSymbol("pbl_shell".to_string()))
        );
        assert_eq!(
            validate_product_v1_caller("function _pbl_segments(size) = 3;"),
            Err(GateError::ReservedProductSymbol(
                "_pbl_segments".to_string()
            ))
        );
        assert_eq!(
            validate_product_v1_caller(
                r#"echo("module pbl_shell() {}"); // function _pbl_segments() = 3"#
            ),
            Ok(())
        );
    }
}
