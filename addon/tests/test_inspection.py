"""Inspection contracts without Blender or external systems."""

import json
from types import SimpleNamespace as NS
import unittest

from printable_bridge.handlers import BlenderHandlers, HandlerError
from printable_bridge.inspection import InspectionError, node_tree_info, object_details


class NamedList(list):
    def get(self, name):
        return next((item for item in self if item.name == name), None)


class InspectionTests(unittest.TestCase):
    def handler(self, objects, collections=()):
        handler = BlenderHandlers.__new__(BlenderHandlers)
        handler._bpy = NS(
            app=NS(version_string="test"),
            data=NS(objects=NamedList(objects), collections=NamedList(collections)),
            context=NS(scene=NS(name="Scene", objects=objects),
                       view_layer=NS(objects=NS(active=None))),
        )
        return handler

    def test_filter_cursor_and_concise_projection(self):
        parts, other = NS(name="Parts"), NS(name="Other")
        objects = [
            NS(name="HandleLight", type="LIGHT", users_collection=[parts]),
            NS(name="HandleA", type="MESH", users_collection=[other]),
            NS(name="HANDLE_B", type="MESH", users_collection=[parts]),
            NS(name="HandleC", type="MESH", users_collection=[parts]),
        ]
        handler = self.handler(objects, [parts, other])
        params = {"name_contains": "handle", "object_type": "MESH",
                  "collection": "Parts", "include_transforms": False, "limit": 1}
        first = handler._get_scene_info(params)
        self.assertEqual(first["objects"], [{"name": "HANDLE_B", "type": "MESH"}])
        self.assertEqual(first["object_count"], 4)
        self.assertEqual(first["next_offset"], 3)
        second = handler._get_scene_info({**params, "offset": first["next_offset"]})
        self.assertEqual(second["objects"][0]["name"], "HandleC")
        self.assertIsNone(second["next_offset"])
        with self.assertRaises(HandlerError):
            handler._get_scene_info({"collection": "Absent"})

    def test_empty_filtered_page_has_continuation(self):
        objects = [NS(name="Unrelated", type="EMPTY")] * 10_000
        objects.append(NS(name="Target", type="MESH"))
        handler = self.handler(objects)
        params = {"name_contains": "Target", "include_transforms": False}
        first = handler._get_scene_info(params)
        self.assertEqual(first["objects"], [])
        self.assertEqual(first["next_offset"], 10_000)
        last = handler._get_scene_info({**params, "offset": first["next_offset"]})
        self.assertEqual(last["objects"], [{"name": "Target", "type": "MESH"}])
        self.assertIsNone(last["next_offset"])

    def test_concise_response_omits_mesh_and_transform_data(self):
        obj = NS(name="Part", type="MESH", location=[0., 0., 0.],
                 rotation_euler=[0., 0., 0.], scale=[1., 1., 1.],
                 dimensions=[80., 18., 12.], data=NS(vertices=range(1000), polygons=range(500)))
        handler = self.handler([obj] * 100)
        full = handler._get_scene_info({})
        concise = handler._get_scene_info({"include_transforms": False})
        self.assertLess(len(json.dumps(concise)), len(json.dumps(full)) / 2)

    def test_object_sections_page_without_reading_unrequested_data(self):
        obj = NS(material_slots=[NS(name="Paint", link="DATA", material=NS(name="Paint", use_nodes=True)),
                                 NS(name="", link="OBJECT", material=None)])
        first = object_details(obj, "materials", 0, 1)
        self.assertEqual(first["items"][0]["material"], "Paint")
        self.assertEqual(first["next_offset"], 1)
        self.assertIsNone(object_details(obj, "materials", 1, 1)["items"][0]["material"])
        obj = NS(modifiers=[NS(name="Pattern", type="NODES", show_viewport=True,
                               show_render=False, node_group=NS(name="PatternGroup"))])
        self.assertEqual(object_details(obj, "modifiers", 0, 1)["items"][0]["node_group"], "PatternGroup")
        obj = NS(parent=NS(name="Assembly"), children=[NS(name="A"), NS(name="B")],
                 users_collection=[NS(name="Parts")])
        result = object_details(obj, "hierarchy", 0, 1)
        self.assertEqual(result["children"]["next_offset"], 1)
        self.assertIsNone(result["collections"]["next_offset"])
        self.assertEqual(result["parent"], "Assembly")

    def test_node_topology_pages_preserve_endpoint_identity(self):
        nodes = [NS(name="Shader", bl_idname="ShaderNodeBsdfPrincipled", type="BSDF_PRINCIPLED", mute=False),
                 NS(name="Output", bl_idname="ShaderNodeOutputMaterial", type="OUTPUT_MATERIAL", mute=False, is_active_output=True)]
        links = [NS(from_node=nodes[0], from_socket=NS(identifier="BSDF"),
                    to_node=nodes[1], to_socket=NS(identifier="Surface"), is_muted=False, is_valid=True)]
        tree = NS(name="PaintTree", nodes=nodes, links=links, bl_idname="GeometryNodeTree")
        bpy = NS(data=NS(materials=NamedList([NS(name="Paint", node_tree=tree)]),
                         node_groups=NamedList([tree])))
        first = node_tree_info(bpy, {"name": "Paint", "limit": 1})
        self.assertEqual(first["next_offset"], 1)
        second = node_tree_info(bpy, {"name": "Paint", "offset": 1})
        self.assertTrue(second["items"][0]["is_active_output"])
        result = node_tree_info(bpy, {"name": "Paint", "section": "links"})
        self.assertEqual(result["items"][0]["to_socket"], "Surface")
        geometry = node_tree_info(bpy, {"name": "PaintTree", "kind": "geometry"})
        self.assertEqual(geometry["total"], 2)
        with self.assertRaises(InspectionError):
            node_tree_info(bpy, {"name": "Absent"})
        for bad in ({"limit": 0}, {"limit": 101}, {"offset": -1}, {"offset": True}, {"kind": []}):
            with self.subTest(bad=bad), self.assertRaises(InspectionError):
                node_tree_info(bpy, {"name": "Paint", **bad})
        self.assertEqual(tree.nodes, nodes)
        self.assertEqual(tree.links, links)


if __name__ == "__main__":
    unittest.main()
