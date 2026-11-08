width_mm = 54;
include_ribs = true;
mounting_x_positions_mm = [-20, 20];

assert(pbl_variant == "print", "pbl_variant must be print");

bracket_width_mm = width_mm;
bracket_depth_mm = 28;
bracket_height_mm = 28;
block_length_mm = 40;
// Leaves 2.5 mm of wall either side of the bore.
block_depth_mm = 14;
bore_diameter_mm = 9;

difference() {
    union() {
        pbl_panel(size = [bracket_width_mm, bracket_depth_mm, 4]);

        translate([-block_length_mm / 2, -block_depth_mm / 2, 3.8])
            cube([block_length_mm, block_depth_mm, bracket_height_mm - 3.8]);

        // Gussets from the block's front face down to the panel, set inside
        // the block's ends so that neither tip of a rib stands free.
        if (include_ribs)
            for (x = mounting_x_positions_mm)
                translate([
                    x - sign(x) * pbl_minimum_wall_mm,
                    -block_depth_mm / 2 + 0.1,
                    3.8
                ])
                    rotate([0, 0, -90])
                        pbl_rib(length = 6, height = 13);
    }

    // A through bore, running just past both ends of the block.
    translate([0, 0, 18])
        pbl_horizontal_bore_cutter(
            length = block_length_mm + 0.2,
            diameter = bore_diameter_mm,
            axis = "x"
        );

    for (x = mounting_x_positions_mm)
        translate([x, 9.25, -0.1])
            cylinder(
                h = 4.2,
                d = 4,
                $fn = 24
            );
}
