width = parameters.get("width", 40)
base = cq.Workplane("XY").box(width, 30, 4, centered=(False, False, False))
wall = cq.Workplane("XY").box(4, 30, 24, centered=(False, False, False))
result = base.union(wall)
for y in (8, 22):
    hole = cq.Workplane("XY").center(25, y).circle(2).extrude(4)
    result = result.cut(hole)
