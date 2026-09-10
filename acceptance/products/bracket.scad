width_mm = 54;
include_ribs = true;
mounting_x_positions_mm = [-20, 20];

assert(pbl_variant == "print", "pbl_variant must be print");

bracket_width_mm = width_mm;
bracket_depth_mm = 28;
bracket_height_mm = 28;

difference() {
    union() {
        pbl_panel(size = [bracket_width_mm, bracket_depth_mm, 4]);

        translate([-20, -6, 3.8])
            cube([40, 12, bracket_height_mm - 3.8]);

        if (include_ribs)
            for (x = mounting_x_positions_mm)
                translate([x, -6, 3.8])
                    rotate([0, 0, x < 0 ? 0 : 180])
                        pbl_rib(length = 11, height = 13);
    }

    translate([0, 0, 18])
        pbl_horizontal_bore_cutter(
            length = 42,
            diameter = 9,
            axis = "x"
        );

    for (x = mounting_x_positions_mm)
        translate([x, 9, -0.1])
            cylinder(
                h = 4.2,
                d = 4,
                $fn = 24
            );
}
