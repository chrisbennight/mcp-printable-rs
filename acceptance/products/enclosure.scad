product_width_mm = 64;
product_depth_mm = 42;
product_height_mm = 22;

union() {
    pbl_shell(
        size = [product_width_mm, product_depth_mm, product_height_mm],
        wall = pbl_minimum_wall_mm
    );

    for (x = [-22, 22])
        for (y = [-12, 12])
            translate([x, y, 0])
                pbl_boss(
                    height = 9,
                    outer_diameter = 10,
                    bore_diameter = 3
                );

    for (x = [-24, 24])
        translate([x, -1, pbl_minimum_wall_mm - 0.2])
            rotate([0, 0, x < 0 ? 0 : 180])
                pbl_rib(length = 12, height = 8.2);
}
