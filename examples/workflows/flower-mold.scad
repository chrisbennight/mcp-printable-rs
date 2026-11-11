$fn = 96;
part = is_undef(pbl_variant) ? "master" : pbl_variant;
module flower() {
  circle(r=5);
  for (a=[0:60:300]) rotate(a) translate([9,0]) scale([1,0.55]) circle(r=7);
}
module master() {
  cylinder(h=3,r=23);
  translate([0,0,3]) linear_extrude(height=2,scale=0.95) flower();
}
module tray() {
  difference() {
    cylinder(h=14,r=29);
    translate([0,0,2]) cylinder(h=14,r=26);
  }
  translate([0,0,2]) master();
}
if (part == "master") master();
else if (part == "tray") tray();
