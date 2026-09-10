grip_length_mm = 56;
grip_diameter_mm = 22;
grip_height_mm = 10;
neck_length_mm = 10;

union() {
    pbl_capsule(
        length = grip_length_mm,
        diameter = grip_diameter_mm,
        height = grip_height_mm
    );

    translate([
        grip_length_mm / 2 + neck_length_mm / 2 - 0.1,
        0,
        0
    ])
        pbl_tapered_transition(
            length = neck_length_mm,
            start_size = [grip_diameter_mm, grip_height_mm],
            end_size = [16, grip_height_mm],
            radius = 0
        );

    translate([grip_length_mm / 2 + neck_length_mm + 6.8, 0, 0])
        pbl_panel(
            size = [14, 16, grip_height_mm],
            radius = 7,
            edge_break = pbl_edge_break_mm
        );
}
