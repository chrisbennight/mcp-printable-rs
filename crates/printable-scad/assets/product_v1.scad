assert(pbl_nozzle_diameter_mm > 0);
assert(pbl_layer_height_mm > 0);
assert(pbl_minimum_wall_mm > 0);
assert(pbl_moving_clearance_mm > 0);
assert(pbl_maximum_overhang_degrees > 0 && pbl_maximum_overhang_degrees <= 90);
assert(pbl_primary_radius_mm >= pbl_secondary_radius_mm);
assert(pbl_secondary_radius_mm >= pbl_edge_break_mm);
assert(pbl_edge_break_mm > 0);
assert(pbl_transition_length_mm > 0);

function _pbl_curve_segments(feature_mm) =
    min(96, max(12, ceil(PI * max(feature_mm, pbl_nozzle_diameter_mm)
        / max(0.01, pbl_nozzle_diameter_mm * 1.5))));

function _pbl_supported_overhang_degrees() =
    pbl_maximum_overhang_degrees * 0.98;

function _pbl_default_span(base, radius) =
    max(base, 2 * radius + 2 * pbl_nozzle_diameter_mm);

function _pbl_default_height(base, edge_break) =
    max(base, 2 * edge_break + pbl_layer_height_mm);

function _pbl_corner_offset(radius, wall) =
    wall / cos(180 / _pbl_curve_segments(2 * radius));

function _pbl_default_shell_wall() =
    max(
        pbl_minimum_wall_mm,
        pbl_edge_break_mm + pbl_layer_height_mm
    );

function _pbl_default_shell_span(base) =
    max(
        _pbl_default_span(base, pbl_primary_radius_mm),
        2 * _pbl_corner_offset(
            pbl_primary_radius_mm,
            _pbl_default_shell_wall()
        ) + 2 * pbl_nozzle_diameter_mm
    );

module _pbl_roundrect_2d(size, radius, curve_segments) {
    assert(len(size) == 2 && size[0] > 0 && size[1] > 0);
    assert(radius >= 0 && radius <= min(size) / 2);
    assert(curve_segments >= 12 && curve_segments <= 96);
    if (radius == 0) {
        square(size, center = true);
    } else {
        hull() {
            for (x = [-size[0] / 2 + radius, size[0] / 2 - radius])
                for (y = [-size[1] / 2 + radius, size[1] / 2 - radius])
                    translate([x, y])
                        circle(r = radius, $fn = curve_segments);
        }
    }
}

module pbl_roundrect_2d(
    size = [
        _pbl_default_span(20, pbl_primary_radius_mm),
        _pbl_default_span(12, pbl_primary_radius_mm)
    ],
    radius = pbl_primary_radius_mm
) {
    _pbl_roundrect_2d(size, radius, _pbl_curve_segments(2 * radius));
}

module _pbl_panel_body(size, radius, edge_break, break_top) {
    slice = min(0.02, edge_break / 4);
    transition = edge_break - slice;
    inset = transition
        * min(1, tan(_pbl_supported_overhang_degrees()));
    curve_segments = _pbl_curve_segments(2 * radius);
    if (edge_break == 0) {
        linear_extrude(height = size[2])
            _pbl_roundrect_2d([size[0], size[1]], radius, curve_segments);
    } else {
        union() {
            hull() {
                linear_extrude(height = slice)
                    _pbl_roundrect_2d(
                        [size[0] - 2 * inset, size[1] - 2 * inset],
                        radius - inset,
                        curve_segments
                    );
                translate([0, 0, edge_break])
                    linear_extrude(height = slice)
                        _pbl_roundrect_2d(
                            [size[0], size[1]],
                            radius,
                            curve_segments
                        );
            }
            translate([0, 0, edge_break])
                linear_extrude(height = size[2] - edge_break * (break_top ? 2 : 1))
                    _pbl_roundrect_2d(
                        [size[0], size[1]],
                        radius,
                        curve_segments
                    );
            if (break_top)
                hull() {
                    translate([0, 0, size[2] - edge_break - slice])
                        linear_extrude(height = slice)
                            _pbl_roundrect_2d(
                                [size[0], size[1]],
                                radius,
                                curve_segments
                            );
                    translate([0, 0, size[2] - slice])
                        linear_extrude(height = slice)
                            _pbl_roundrect_2d(
                                [size[0] - 2 * inset, size[1] - 2 * inset],
                                radius - inset,
                                curve_segments
                            );
                }
        }
    }
}

module pbl_panel(
    size = [
        _pbl_default_span(40, pbl_primary_radius_mm),
        _pbl_default_span(30, pbl_primary_radius_mm),
        _pbl_default_height(3, pbl_edge_break_mm)
    ],
    radius = pbl_primary_radius_mm,
    edge_break = pbl_edge_break_mm
) {
    assert(len(size) == 3 && min(size) > 0);
    assert(radius >= edge_break && 2 * radius <= min(size[0], size[1]));
    assert(edge_break >= 0 && 2 * edge_break < size[2]);
    _pbl_panel_body(size, radius, edge_break, true);
}

module pbl_shell(
    size = [
        _pbl_default_shell_span(60),
        _pbl_default_shell_span(40),
        max(
            _pbl_default_height(20, pbl_edge_break_mm),
            2 * _pbl_default_shell_wall() + pbl_layer_height_mm
        )
    ],
    wall = _pbl_default_shell_wall(),
    radius = pbl_primary_radius_mm,
    edge_break = pbl_edge_break_mm,
    open_top = true
) {
    assert(len(size) == 3 && min(size) > 0);
    assert(wall >= pbl_minimum_wall_mm);
    assert(size[2] > wall * (open_top ? 1 : 2));
    assert(radius >= edge_break);
    assert(2 * radius <= min(size[0], size[1]));
    assert(edge_break >= 0 && edge_break < wall && edge_break < size[2] / 2);
    curve_segments = _pbl_curve_segments(2 * radius);
    corner_offset = _pbl_corner_offset(radius, wall);
    assert(size[0] > 2 * corner_offset && size[1] > 2 * corner_offset);
    difference() {
        _pbl_panel_body(size, radius, edge_break, false);
        translate([0, 0, wall])
            linear_extrude(height = size[2] - wall * (open_top ? 1 : 2) + (open_top ? 0.1 : 0))
                _pbl_roundrect_2d(
                    [
                        size[0] - 2 * corner_offset,
                        size[1] - 2 * corner_offset
                    ],
                    max(0, radius - corner_offset),
                    curve_segments
                );
    }
}

module pbl_capsule(
    length = max(40, 2 * pbl_edge_break_mm + 2 * pbl_nozzle_diameter_mm),
    diameter = max(16, 2 * pbl_edge_break_mm + 2 * pbl_nozzle_diameter_mm),
    height = _pbl_default_height(8, pbl_edge_break_mm),
    edge_break = pbl_edge_break_mm
) {
    assert(length >= diameter && diameter > 0 && height > 0);
    pbl_panel(
        size = [length, diameter, height],
        radius = diameter / 2,
        edge_break = edge_break
    );
}

module _pbl_extrude_x(depth) {
    rotate([0, 90, 0])
        rotate([0, 0, 90])
            linear_extrude(height = depth, center = true)
                children();
}

module pbl_tapered_transition(
    length = pbl_transition_length_mm,
    start_size = [
        _pbl_default_span(20, pbl_secondary_radius_mm),
        _pbl_default_span(12, pbl_secondary_radius_mm)
    ],
    end_size = [
        _pbl_default_span(14, pbl_secondary_radius_mm),
        _pbl_default_span(8, pbl_secondary_radius_mm)
    ],
    radius = pbl_secondary_radius_mm
) {
    assert(length > 0);
    assert(len(start_size) == 2 && len(end_size) == 2);
    assert(min(start_size) > 0 && min(end_size) > 0);
    assert(radius >= 0 && 2 * radius <= min(min(start_size), min(end_size)));
    slice = min(0.04, length / 10);
    hull() {
        translate([-length / 2 + slice / 2, 0, start_size[1] / 2])
            _pbl_extrude_x(slice)
                pbl_roundrect_2d(start_size, radius);
        translate([length / 2 - slice / 2, 0, end_size[1] / 2])
            _pbl_extrude_x(slice)
                pbl_roundrect_2d(end_size, radius);
    }
}

module pbl_rib(
    length = 20,
    height = 12,
    thickness = pbl_minimum_wall_mm
) {
    assert(length > 0 && height > 0);
    assert(thickness >= pbl_minimum_wall_mm);
    rotate([90, 0, 0])
        linear_extrude(height = thickness, center = true)
            polygon([[0, 0], [length, 0], [0, height]]);
}

module pbl_boss(
    height = 10,
    outer_diameter = 10,
    bore_diameter = 0
) {
    assert(height > 0 && outer_diameter > 0 && bore_diameter >= 0);
    curve_segments = _pbl_curve_segments(outer_diameter);
    assert(bore_diameter == 0
        || (outer_diameter - bore_diameter) / 2
            * cos(180 / curve_segments) >= pbl_minimum_wall_mm);
    difference() {
        cylinder(
            h = height,
            d = outer_diameter,
            $fn = curve_segments
        );
        if (bore_diameter > 0)
            translate([0, 0, -0.05])
                cylinder(
                    h = height + 0.1,
                    d = bore_diameter,
                    $fn = curve_segments
                );
    }
}

module _pbl_teardrop_2d(diameter) {
    radius = diameter / 2;
    roof_overhang = _pbl_supported_overhang_degrees();
    shoulder_x = radius * cos(roof_overhang);
    shoulder_z = radius * sin(roof_overhang);
    apex_z = radius / sin(roof_overhang);
    arc_segments = _pbl_curve_segments(diameter);
    lower_arc = [
        for (step = [1 : arc_segments])
            let(angle = 180 - roof_overhang
                + step * (180 + 2 * roof_overhang) / arc_segments)
                [radius * cos(angle), radius * sin(angle)]
    ];
    polygon(concat(
        [[0, apex_z], [-shoulder_x, shoulder_z]],
        lower_arc
    ));
}

module pbl_horizontal_bore_cutter(
    length = 20,
    diameter = 6,
    axis = "x"
) {
    assert(length > 0 && diameter > 0);
    assert(axis == "x" || axis == "y");
    if (axis == "x")
        rotate([0, 90, 0])
            rotate([0, 0, 90])
                linear_extrude(height = length, center = true)
                    _pbl_teardrop_2d(diameter);
    else
        rotate([-90, 0, 0])
            rotate([0, 0, 180])
                linear_extrude(height = length, center = true)
                    _pbl_teardrop_2d(diameter);
}

module pbl_linear_pattern(count = 2, spacing = 10, axis = "x") {
    assert(count >= 1 && count <= 256 && count == floor(count));
    assert(spacing > 0);
    assert(axis == "x" || axis == "y" || axis == "z");
    for (index = [0 : count - 1])
        translate(
            axis == "x"
                ? [index * spacing, 0, 0]
                : axis == "y"
                    ? [0, index * spacing, 0]
                    : [0, 0, index * spacing]
        )
            children();
}
