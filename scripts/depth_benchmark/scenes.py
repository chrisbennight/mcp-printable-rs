"""Render benchmark scenes with exact depth and normals.

Run: blender -b --factory-startup -P scenes.py -- <output-dir> [scene ...]

For each scene this writes <scene>/rgb.png (display image), <scene>/passes.exr
(multilayer: Combined, Depth, Normal, Position) and <scene>/camera.json. Depth
and normals are converted to the camera frame later, from the Position and
Normal passes, so no Blender-specific depth convention is relied on.
"""

import json
import math
import sys
from pathlib import Path

import bpy
from mathutils import Euler, Vector

W, H = 1024, 768


def reset():
    bpy.ops.wm.read_factory_settings(use_empty=True)
    scene = bpy.context.scene
    scene.render.engine = "CYCLES"
    prefs = bpy.context.preferences.addons["cycles"].preferences
    for kind in ("OPTIX", "CUDA"):
        try:
            prefs.compute_device_type = kind
            prefs.get_devices()
            if any(d.type == kind for d in prefs.devices):
                break
        except TypeError:
            continue
    for d in prefs.devices:
        d.use = d.type != "CPU"
    scene.cycles.device = "GPU"
    scene.cycles.samples = 256
    scene.cycles.use_denoising = True
    scene.render.resolution_x, scene.render.resolution_y = W, H
    scene.render.resolution_percentage = 100
    scene.render.film_transparent = False
    vl = scene.view_layers[0]
    vl.use_pass_z = True
    vl.use_pass_normal = True
    vl.use_pass_position = True
    world = bpy.data.worlds.new("world")
    scene.world = world
    world.use_nodes = True
    world.node_tree.nodes["Background"].inputs[0].default_value = (0.05, 0.05, 0.06, 1)
    world.node_tree.nodes["Background"].inputs[1].default_value = 0.4
    return scene


def material(name, color, roughness=0.5, metallic=0.0, noise=0.15, noise_scale=40.0, coat=0.0):
    """A Principled material with mild colour variation and no bump, so the
    normal pass matches the geometry."""
    m = bpy.data.materials.new(name)
    m.use_nodes = True
    nt = m.node_tree
    bsdf = nt.nodes["Principled BSDF"]
    bsdf.inputs["Roughness"].default_value = roughness
    bsdf.inputs["Metallic"].default_value = metallic
    if coat and "Coat Weight" in bsdf.inputs:
        bsdf.inputs["Coat Weight"].default_value = coat
    tex = nt.nodes.new("ShaderNodeTexNoise")
    tex.inputs["Scale"].default_value = noise_scale
    ramp = nt.nodes.new("ShaderNodeMix")
    ramp.data_type = "RGBA"
    c0 = tuple(max(0.0, c * (1 - noise)) for c in color) + (1,)
    c1 = tuple(min(1.0, c * (1 + noise)) for c in color) + (1,)
    ramp.inputs[6].default_value = c0
    ramp.inputs[7].default_value = c1
    nt.links.new(tex.outputs["Fac"], ramp.inputs[0])
    nt.links.new(ramp.outputs[2], bsdf.inputs["Base Color"])
    return m


def assign(obj, mat):
    obj.data.materials.clear()
    obj.data.materials.append(mat)
    return obj


def smooth(obj, level=2):
    mod = obj.modifiers.new("subd", "SUBSURF")
    mod.levels = mod.render_levels = level
    for p in obj.data.polygons:
        p.use_smooth = True
    return obj


def add(kind, **kw):
    getattr(bpy.ops.mesh, f"primitive_{kind}_add")(**kw)
    return bpy.context.active_object


def text(body, size, extrude, location, rotation=(0, 0, 0)):
    curve = bpy.data.curves.new("text", type="FONT")
    curve.body = body
    curve.size = size
    curve.extrude = extrude
    curve.align_x = "CENTER"
    curve.align_y = "CENTER"
    obj = bpy.data.objects.new("text", curve)
    bpy.context.collection.objects.link(obj)
    obj.location = location
    obj.rotation_euler = rotation
    bpy.ops.object.select_all(action="DESELECT")
    bpy.context.view_layer.objects.active = obj
    obj.select_set(True)
    bpy.ops.object.convert(target="MESH")
    return bpy.context.active_object


def light(kind, location, energy, size=1.0, rotation=(0, 0, 0)):
    data = bpy.data.lights.new(kind, kind)
    data.energy = energy
    if kind == "AREA":
        data.size = size
    obj = bpy.data.objects.new(kind, data)
    obj.location = location
    obj.rotation_euler = rotation
    bpy.context.collection.objects.link(obj)
    return obj


def look_at(obj, target):
    direction = Vector(target) - obj.location
    obj.rotation_euler = direction.to_track_quat("-Z", "Y").to_euler()


def camera(location, target, lens):
    data = bpy.data.cameras.new("cam")
    data.lens = lens
    data.sensor_fit = "HORIZONTAL"
    data.sensor_width = 36.0
    data.clip_start = 0.01
    obj = bpy.data.objects.new("cam", data)
    bpy.context.collection.objects.link(obj)
    obj.location = location
    look_at(obj, target)
    bpy.context.scene.camera = obj
    return obj


def backdrop(z=0.0, size=4.0, color=(0.55, 0.55, 0.52), y=1.2):
    floor = add("plane", size=size, location=(0, 0, z))
    wall = add("plane", size=size, location=(0, y, z + size / 2), rotation=(math.radians(90), 0, 0))
    m = material("backdrop", color, roughness=0.9, noise=0.08, noise_scale=8)
    assign(floor, m)
    assign(wall, m)


def scene_relief():
    plate = add("cube", size=1, location=(0, 0, 0))
    plate.scale = (0.24, 0.18, 0.012)
    stone = material("stone", (0.72, 0.66, 0.58), roughness=0.8, noise=0.12, noise_scale=25)
    assign(plate, stone)
    monkey = smooth(add("monkey", size=0.1, location=(-0.045, 0.0, 0.006)), 3)
    monkey.rotation_euler = (0, 0, 0)
    monkey.scale = (1, 1, 0.35)
    assign(monkey, stone)
    for i, (x, y, r) in enumerate([(0.07, 0.045, 0.012), (0.085, 0.0, 0.009), (0.07, -0.045, 0.015)]):
        s = smooth(add("uv_sphere", radius=r, location=(x, y, 0.006)), 1)
        s.scale = (1, 1, 0.6)
        assign(s, stone)
    t = text("PRINT", 0.03, 0.002, (0.0, -0.065, 0.008))
    assign(t, stone)
    rim = add("torus", major_radius=0.105, minor_radius=0.004, location=(0, 0, 0.006))
    rim.scale = (1.1, 0.8, 0.5)
    assign(rim, stone)
    light("AREA", (-0.4, 0.3, 0.6), 12, size=0.4)
    light("AREA", (0.5, -0.2, 0.4), 3, size=0.8)
    camera((0.0, -0.05, 0.75), (0, 0, 0), 85)


def scene_head():
    backdrop(color=(0.35, 0.38, 0.42))
    ped = add("cylinder", radius=0.25, depth=0.6, location=(0, 0, 0.3))
    assign(ped, material("ped", (0.3, 0.28, 0.26), roughness=0.7))
    head = smooth(add("monkey", size=0.5, location=(0, 0, 0.85)), 3)
    head.rotation_euler = (math.radians(-10), 0, math.radians(15))
    assign(head, material("marble", (0.86, 0.85, 0.82), roughness=0.35, noise=0.05, noise_scale=6))
    light("AREA", (-1.5, -1.2, 2.0), 300, size=1.0)
    light("AREA", (1.5, -1.0, 1.2), 80, size=2.0)
    camera((0.9, -1.9, 1.25), (0, 0, 0.8), 50)


def scene_table():
    backdrop(color=(0.6, 0.57, 0.5))
    table = add("cube", size=1, location=(0, 0, 0.4))
    table.scale = (1.6, 0.9, 0.04)
    assign(table, material("wood", (0.45, 0.3, 0.18), roughness=0.6, noise=0.3, noise_scale=6))
    top = 0.42
    s = smooth(add("uv_sphere", radius=0.09, location=(-0.45, 0.05, top + 0.09)), 1)
    assign(s, material("glossy", (0.7, 0.1, 0.1), roughness=0.08, coat=1.0))
    mug = add("cylinder", radius=0.06, depth=0.14, location=(-0.15, -0.15, top + 0.07))
    handle = add("torus", major_radius=0.04, minor_radius=0.01, location=(-0.07, -0.15, top + 0.07),
                 rotation=(math.radians(90), 0, 0))
    mm = material("mug", (0.2, 0.35, 0.7), roughness=0.3)
    assign(mug, mm)
    assign(handle, mm)
    tor = add("torus", major_radius=0.08, minor_radius=0.03, location=(0.15, 0.15, top + 0.03))
    assign(tor, material("torus", (0.85, 0.7, 0.2), roughness=0.4))
    cone = add("cone", radius1=0.07, depth=0.2, location=(0.45, -0.1, top + 0.1))
    assign(cone, material("cone", (0.2, 0.6, 0.3), roughness=0.5))
    box = add("cube", size=0.14, location=(0.0, 0.25, top + 0.07))
    box.rotation_euler = (0, 0, math.radians(30))
    bev = box.modifiers.new("bevel", "BEVEL")
    bev.width, bev.segments = 0.01, 3
    assign(box, material("box", (0.8, 0.8, 0.78), roughness=0.6))
    mk = smooth(add("monkey", size=0.18, location=(0.35, 0.25, top + 0.08)), 2)
    mk.rotation_euler = (0, 0, math.radians(-20))
    assign(mk, material("monkey", (0.6, 0.45, 0.35), roughness=0.5))
    light("AREA", (-1.0, -1.0, 2.0), 250, size=1.5)
    light("AREA", (1.2, -0.5, 1.5), 80, size=1.0)
    camera((0.0, -1.5, 1.25), (0, 0.05, 0.45), 35)


def scene_thin():
    backdrop(color=(0.75, 0.75, 0.72), y=0.9)
    hub = (0, 0, 0.6)
    rim = add("torus", major_radius=0.35, minor_radius=0.012, location=hub, rotation=(math.radians(90), 0, 0))
    metal = material("metal", (0.25, 0.25, 0.27), roughness=0.35, metallic=0.8, noise=0.05)
    assign(rim, metal)
    for i in range(24):
        a = 2 * math.pi * i / 24
        sp = add("cylinder", radius=0.0025, depth=0.34, location=(math.cos(a) * 0.17, 0, 0.6 + math.sin(a) * 0.17),
                 rotation=(0, math.pi / 2 - a, 0))
        sp.rotation_euler = Euler((0, -a + math.pi / 2, 0))
        assign(sp, metal)
    hubc = add("cylinder", radius=0.03, depth=0.06, location=hub, rotation=(math.radians(90), 0, 0))
    assign(hubc, metal)
    stem_m = material("stem", (0.3, 0.45, 0.2), roughness=0.6)
    for i in range(9):
        x = -0.55 + 0.035 * i
        st = add("cylinder", radius=0.003, depth=0.5 + 0.03 * i, location=(x, -0.15, (0.5 + 0.03 * i) / 2),
                 rotation=(0, math.radians(-8 + 2 * i), 0))
        assign(st, stem_m)
    light("AREA", (-1.0, -1.5, 2.0), 250, size=1.5)
    light("AREA", (1.0, -1.0, 1.0), 60, size=1.0)
    camera((0.05, -1.6, 0.65), (-0.05, 0, 0.5), 40)


def scene_coin():
    cloth = add("plane", size=1.0, location=(0, 0, 0))
    assign(cloth, material("cloth", (0.04, 0.04, 0.05), roughness=0.95, noise=0.3, noise_scale=80))
    silver = material("silver", (0.9, 0.9, 0.92), roughness=0.25, metallic=1.0, noise=0.03)
    disk = add("cylinder", radius=0.02, depth=0.0025, location=(0, 0, 0.00125), vertices=128)
    assign(disk, silver)
    rim = add("torus", major_radius=0.0192, minor_radius=0.0008, location=(0, 0, 0.0025), major_segments=128)
    assign(rim, silver)
    mk = smooth(add("monkey", size=0.018, location=(0, 0.002, 0.0026)), 3)
    mk.scale = (1, 1, 0.2)
    assign(mk, silver)
    t = text("2026", 0.004, 0.0003, (0, -0.012, 0.0027))
    assign(t, silver)
    light("AREA", (-0.15, -0.1, 0.2), 8, size=0.15)
    light("AREA", (0.2, 0.1, 0.15), 3, size=0.3)
    camera((0.0, -0.035, 0.11), (0, 0, 0), 100)


def scene_coin_engraved():
    """A 1 mm-deep engraved coin: lettering and a face cut into the surface."""
    cloth = add("plane", size=1.0, location=(0, 0, 0))
    assign(cloth, material("cloth", (0.3, 0.05, 0.05), roughness=0.95, noise=0.3, noise_scale=80))
    brass = material("brass", (0.8, 0.62, 0.3), roughness=0.35, metallic=1.0, noise=0.03)
    disk = add("cylinder", radius=0.02, depth=0.003, location=(0, 0, 0.0015), vertices=128)
    cutters = [text("PRINTABLE", 0.0035, 0.002, (0, -0.011, 0.0029))]
    face = smooth(add("monkey", size=0.016, location=(0, 0.003, 0.0032)), 2)
    face.scale = (1, 1, 0.12)
    cutters.append(face)
    bpy.context.view_layer.update()
    for cutter in cutters:
        mod = disk.modifiers.new("cut", "BOOLEAN")
        mod.operation = "DIFFERENCE"
        mod.object = cutter
        bpy.context.view_layer.objects.active = disk
        bpy.ops.object.modifier_apply(modifier=mod.name)
        bpy.data.objects.remove(cutter, do_unlink=True)
    assign(disk, brass)
    light("AREA", (-0.12, -0.08, 0.2), 6, size=0.12)
    light("AREA", (0.2, 0.1, 0.15), 2, size=0.3)
    camera((0.01, -0.03, 0.11), (0, 0, 0), 100)


def scene_lettering():
    """Raised and engraved lettering on a slate plaque, lit at a low angle."""
    plate = add("cube", size=1, location=(0, 0, 0))
    plate.scale = (0.24, 0.12, 0.01)
    slate = material("slate", (0.25, 0.27, 0.3), roughness=0.7, noise=0.15, noise_scale=30)
    assign(plate, slate)
    raised = text("RELIEF", 0.04, 0.003, (0, 0.025, 0.0055))
    assign(raised, slate)
    engraved = text("2026", 0.035, 0.004, (0, -0.03, 0.0035))
    mod = plate.modifiers.new("cut", "BOOLEAN")
    mod.operation = "DIFFERENCE"
    mod.object = engraved
    bpy.context.view_layer.objects.active = plate
    bpy.ops.object.modifier_apply(modifier=mod.name)
    bpy.data.objects.remove(engraved, do_unlink=True)
    light("AREA", (-0.6, 0.05, 0.25), 25, size=0.3)
    light("AREA", (0.4, -0.3, 0.6), 4, size=0.8)
    camera((0.0, -0.04, 0.6), (0, 0, 0), 85)


def scene_medal_metal():
    """A polished medal with a raised head: strong reflections over a shallow relief."""
    backdrop(color=(0.2, 0.22, 0.25), y=0.5, size=2.0)
    chrome = material("chrome", (0.95, 0.95, 0.97), roughness=0.08, metallic=1.0, noise=0.01)
    disk = add("cylinder", radius=0.035, depth=0.004, location=(0, 0, 0.002), vertices=128)
    assign(disk, chrome)
    rim = add("torus", major_radius=0.0335, minor_radius=0.0015, location=(0, 0, 0.004), major_segments=128)
    assign(rim, chrome)
    head = smooth(add("monkey", size=0.04, location=(0, 0, 0.0045)), 3)
    head.scale = (1, 1, 0.18)
    assign(head, chrome)
    light("AREA", (-0.2, -0.15, 0.3), 10, size=0.2)
    light("AREA", (0.25, 0.1, 0.2), 4, size=0.4)
    camera((0.0, -0.05, 0.17), (0, 0, 0), 100)


def scene_relief_shallow():
    """A 3 mm low relief: the same plaque as `relief`, flattened."""
    plate = add("cube", size=1, location=(0, 0, 0))
    plate.scale = (0.24, 0.18, 0.012)
    stone = material("stone", (0.7, 0.68, 0.62), roughness=0.85, noise=0.12, noise_scale=25)
    assign(plate, stone)
    monkey = smooth(add("monkey", size=0.12, location=(0, 0.0, 0.006)), 3)
    monkey.scale = (1, 1, 0.06)
    assign(monkey, stone)
    for x, y in [(-0.08, 0.05), (0.08, 0.05), (-0.08, -0.05), (0.08, -0.05)]:
        s = smooth(add("uv_sphere", radius=0.01, location=(x, y, 0.006)), 1)
        s.scale = (1, 1, 0.2)
        assign(s, stone)
    light("AREA", (-0.5, 0.25, 0.3), 14, size=0.3)
    light("AREA", (0.5, -0.2, 0.5), 3, size=0.8)
    camera((0.0, -0.03, 0.75), (0, 0, 0), 85)


SCENES = {"relief": scene_relief, "head": scene_head, "table": scene_table, "thin": scene_thin, "coin": scene_coin,
          "coin-engraved": scene_coin_engraved, "lettering": scene_lettering, "medal-metal": scene_medal_metal,
          "relief-shallow": scene_relief_shallow}


def save(out: Path):
    scene = bpy.context.scene
    bpy.ops.render.render(write_still=False)
    result = bpy.data.images["Render Result"]
    settings = scene.render.image_settings
    if hasattr(settings, "media_type"):
        settings.media_type = "IMAGE"
    settings.file_format = "PNG"
    settings.color_mode = "RGB"
    settings.color_depth = "8"
    result.save_render(str(out / "rgb.png"))
    if hasattr(settings, "media_type"):
        settings.media_type = "MULTI_LAYER_IMAGE"
        settings.file_format = "OPEN_EXR_MULTILAYER"
    else:
        settings.file_format = "OPEN_EXR_MULTILAYER"
    settings.color_depth = "32"
    settings.exr_codec = "ZIP"
    result.save_render(str(out / "passes.exr"))
    cam = scene.camera
    info = {
        "width": W, "height": H, "lens_mm": cam.data.lens, "sensor_width_mm": cam.data.sensor_width,
        "focal_px": cam.data.lens / cam.data.sensor_width * W,
        "matrix_world": [list(r) for r in cam.matrix_world],
        "blender": bpy.app.version_string,
    }
    (out / "camera.json").write_text(json.dumps(info, indent=1))


def main():
    args = sys.argv[sys.argv.index("--") + 1:]
    root = Path(args[0])
    names = args[1:] or list(SCENES)
    for name in names:
        reset()
        SCENES[name]()
        out = root / name
        out.mkdir(parents=True, exist_ok=True)
        save(out)
        print("rendered", name, flush=True)


main()
