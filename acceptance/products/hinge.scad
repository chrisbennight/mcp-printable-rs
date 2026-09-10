hinge_length_mm = 70;
pivot_z_mm = 6.4;
housing_half_width_mm = 6.4;
housing_roof_mm = 7.4;
bore_diameter_mm = 7.0;
pin_radius_mm = 2.8;
axial_clearance_mm = 0.5;
end_knuckle_length_mm = 11.5;
center_knuckle_length_mm =
    hinge_length_mm - 2 * end_knuckle_length_mm - 2 * axial_clearance_mm;

assert(axial_clearance_mm >= pbl_moving_clearance_mm);
assert(bore_diameter_mm / 2 - pin_radius_mm >= pbl_moving_clearance_mm);
assert(
    housing_half_width_mm - bore_diameter_mm / 2
        >= pbl_minimum_wall_mm
);
assert(
    pivot_z_mm - bore_diameter_mm / 2
        >= pbl_minimum_wall_mm
);
assert(
    housing_roof_mm
        - (bore_diameter_mm / 2)
            / sin(pbl_maximum_overhang_degrees * 0.98)
        >= pbl_minimum_wall_mm
);
assert(3 >= pbl_minimum_wall_mm);

module hinge_axis_extrude(length) {
    rotate([0, 90, 0])
        rotate([0, 0, 90])
            linear_extrude(height = length, center = true)
                children();
}

module hinge_housing_body(length) {
    translate([0, 0, pivot_z_mm])
        hinge_axis_extrude(length)
            polygon([
                [-housing_half_width_mm, -pivot_z_mm],
                [housing_half_width_mm, -pivot_z_mm],
                [housing_half_width_mm, 2.2],
                [0, housing_roof_mm],
                [-housing_half_width_mm, 2.2]
            ]);
}

module hinge_fixed_knuckle(length) {
    difference() {
        hinge_housing_body(length);
        translate([0, 0, pivot_z_mm])
            pbl_horizontal_bore_cutter(
                length = length + 0.2,
                diameter = bore_diameter_mm,
                axis = "x"
            );
    }
}

module hinge_pin(length) {
    translate([0, 0, pivot_z_mm])
        hinge_axis_extrude(length)
            polygon([
                [0, -pin_radius_mm],
                [pin_radius_mm, 0],
                [0, pin_radius_mm],
                [-pin_radius_mm, 0]
            ]);
}

module fixed_body() {
    union() {
        translate([0, -22.5, 0])
            pbl_panel(size = [hinge_length_mm, 27, 3]);

        for (side = [-1, 1]) {
            knuckle_x = side
                * (hinge_length_mm - end_knuckle_length_mm) / 2;
            translate([knuckle_x, 0, 0])
                hinge_fixed_knuckle(end_knuckle_length_mm);
            translate([
                knuckle_x - end_knuckle_length_mm / 2,
                -10.5,
                0
            ])
                cube([end_knuckle_length_mm, 6, 4]);
        }
    }
}

module moving_body() {
    union() {
        translate([0, 22.5, 0])
            pbl_panel(size = [hinge_length_mm, 27, 3]);

        hinge_housing_body(center_knuckle_length_mm);
        hinge_pin(hinge_length_mm - 2 * axial_clearance_mm);

        translate([-center_knuckle_length_mm / 2, 4.5, 0])
            cube([center_knuckle_length_mm, 6, 4]);
    }
}

if (pbl_variant == "fixed") {
    fixed_body();
} else if (pbl_variant == "moving") {
    moving_body();
} else {
    assert(false, "pbl_variant must be fixed or moving");
}
