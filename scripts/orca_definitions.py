"""Read literal setting definitions from Orca C++ without executing source code.

This is a deliberately partial interpreter of definition statements. Expressions,
conditional updates and dynamically generated keys that cannot be resolved remain
unknown. Native validation still owns combinations and hardware applicability.
"""

import json
import math
import re

from tree_sitter import Language, Parser
import tree_sitter_cpp


UNKNOWN = object()
FIELDS = {"min": "minimum", "max": "maximum", "sidetext": "unit",
          "nullable": "nullable", "gui_type": "gui_type", "ratio_over": "ratio_over"}
TYPES = {
    "coFloat": ("number", False), "coFloats": ("number", True),
    "coInt": ("integer", False), "coInts": ("integer", True),
    "coBool": ("boolean", False), "coBools": ("boolean", True),
    "coString": ("string", False), "coStrings": ("string", True),
    "coPercent": ("percent", False), "coPercents": ("percent", True),
    "coFloatOrPercent": ("number_or_percent", False),
    "coFloatsOrPercents": ("number_or_percent", True),
    "coEnum": ("enum", False), "coEnums": ("enum", True),
    "coPoint": ("point", False), "coPoints": ("point", True),
    "coPoint3": ("point3", False), "coPointsGroups": ("point_groups", True),
}


def text(node):
    return node.text.decode("utf-8")


def walk(node):
    yield node
    for child in node.named_children:
        yield from walk(child)


def literal(node):
    """Resolve only literal syntax, never evaluate C++ or caller-provided text."""
    if node is None or node.has_error:
        return UNKNOWN
    value = text(node)
    if node.type == "number_literal":
        if not re.fullmatch(r"[-+]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][-+]?\d+)?[fF]?", value):
            return UNKNOWN
        number = float(value.rstrip("fF"))
        return (int(number) if number.is_integer() else number) if math.isfinite(number) else UNKNOWN
    if value in ("true", "false"):
        return value == "true"
    if node.type == "string_literal":
        value = value.removeprefix("u8")
        try:
            return json.loads(value)
        except (ValueError, TypeError):
            return UNKNOWN
    if node.type in ("initializer_list", "argument_list"):
        values = [literal(child) for child in node.named_children]
        return values if all(item is not UNKNOWN for item in values) else UNKNOWN
    if node.type == "call_expression":
        function = node.child_by_field_name("function")
        arguments = node.child_by_field_name("arguments")
        if text(function) in ("L", "Vec2d", "Vec3d") and arguments is not None:
            values = literal(arguments)
            if values is not UNKNOWN:
                return values[0] if text(function) == "L" and len(values) == 1 else values
    return UNKNOWN


def enum_maps(root):
    """Retain every native serialization alias, including values hidden in the GUI."""
    maps = {}
    for node in walk(root):
        if node.type != "init_declarator":
            continue
        name = node.child_by_field_name("declarator")
        value = node.child_by_field_name("value")
        if name is None or not text(name).startswith("s_keys_map_") or value is None or node.has_error:
            continue
        entries = {}
        for pair in value.named_children:
            if pair.type == "comment":
                continue
            children = pair.named_children
            if len(children) != 2 or not isinstance(key := literal(children[0]), str):
                entries = {}
                break
            symbol = children[1]
            if symbol.type == "call_expression" and text(symbol.child_by_field_name("function")) == "int":
                arguments = symbol.child_by_field_name("arguments").named_children
                if len(arguments) == 1:
                    symbol = arguments[0]
            entries[key] = text(symbol)
        if entries:
            maps[text(name).removeprefix("s_keys_map_")] = entries
    return maps


def default_value(node, mapping):
    if node.type != "new_expression":
        return UNKNOWN
    kind = node.child_by_field_name("type")
    arguments = node.child_by_field_name("arguments")
    if kind is None or arguments is None or not arguments.named_children:
        return UNKNOWN
    name = text(kind)
    children = arguments.named_children
    if name.startswith("ConfigOptionEnum<") and len(children) == 1:
        symbol = text(children[0]).split("::")[-1]
        matches = [key for key, target in mapping.items() if target.split("::")[-1] == symbol]
        return matches[0] if len(matches) == 1 else UNKNOWN
    values = literal(arguments)
    if values is UNKNOWN:
        return UNKNOWN
    if name == "ConfigOptionFloatOrPercent" and len(values) == 2:
        return {"value": values[0], "percent": values[1]}
    scalar = {"ConfigOptionFloat", "ConfigOptionInt", "ConfigOptionBool",
              "ConfigOptionString", "ConfigOptionPercent"}
    if name in scalar and len(values) == 1:
        return values[0]
    if name in {"ConfigOptionFloats", "ConfigOptionInts", "ConfigOptionBools",
                "ConfigOptionStrings", "ConfigOptionPercents", "ConfigOptionPoints"}:
        if arguments.type == "initializer_list":
            return values
        if len(children) == 1 and children[0].type == "initializer_list":
            return literal(children[0])
        return UNKNOWN
    return UNKNOWN


def field_name(node, variable):
    if node is None or node.type != "field_expression":
        return None
    base = node.child_by_field_name("argument")
    field = node.child_by_field_name("field")
    return text(field) if base is not None and text(base) == variable else None


def extract(source, keys):
    parser = Parser(Language(tree_sitter_cpp.language()))
    root = parser.parse(source.encode("utf-8")).root_node
    maps = enum_maps(root)
    candidates = {}
    for call in walk(root):
        if call.type != "call_expression" or call.has_error:
            continue
        function = call.child_by_field_name("function")
        args = call.child_by_field_name("arguments")
        if function is None or text(function) != "this->add" or args is None:
            continue
        arguments = args.named_children
        if len(arguments) != 2 or not isinstance(key := literal(arguments[0]), str) or key not in keys:
            continue
        owner = call
        conditional = False
        while owner is not None and owner.type != "function_definition":
            conditional |= owner.type in {"if_statement", "switch_statement", "for_statement",
                                         "for_range_loop", "while_statement", "preproc_if", "preproc_ifdef"}
            owner = owner.parent
        if owner is None:
            continue
        declarator = owner.child_by_field_name("declarator")
        if declarator is None or not text(declarator).startswith("PrintConfigDef::init"):
            continue
        assignment = call.parent
        if assignment.type != "assignment_expression":
            continue
        variable = text(assignment.child_by_field_name("left"))
        statement = assignment.parent
        block = statement.parent
        if block.type != "compound_statement":
            continue
        native_type = text(arguments[1])
        definition = {"status": "source_literal", "native_type": native_type,
                      "type": TYPES.get(native_type, ("unknown", False))[0],
                      "vector": TYPES.get(native_type, (None, None))[1],
                      "source": "src/libslic3r/PrintConfig.cpp",
                      "applicability": "unknown", "unknown_fields": []}
        mapping = {}
        default = None
        choices = []
        unknown = set()
        siblings = block.named_children
        start = next(i for i, item in enumerate(siblings) if item.id == statement.id)
        for sibling in siblings[start + 1:]:
            if any(item.type == "assignment_expression" and text(item.child_by_field_name("left")) == variable
                   for item in walk(sibling)):
                break
            if sibling.has_error:
                unknown.update(set(FIELDS.values()) | {"choices", "default"})
                continue
            direct = sibling.named_children[0] if sibling.type == "expression_statement" and sibling.named_children else None
            for item in walk(sibling):
                if item.type == "assignment_expression":
                    field = field_name(item.child_by_field_name("left"), variable)
                    value = item.child_by_field_name("right")
                    if field in FIELDS:
                        resolved = literal(value)
                        if field == "gui_type":
                            match = re.fullmatch(r"ConfigOptionDef::GUIType::(undefined|i_enum_open|f_enum_open|color|select_open|slider|legend|one_string)", text(value))
                            resolved = match[1] if match else UNKNOWN
                        operator = item.child_by_field_name("operator")
                        if item.id != getattr(direct, "id", None) or resolved is UNKNOWN or text(operator) != "=":
                            unknown.add(FIELDS[field])
                        else:
                            definition[FIELDS[field]] = resolved
                    elif field == "enum_keys_map":
                        match = re.fullmatch(r"&ConfigOptionEnum<([^>]+)>::get_enum_values\(\)", text(value))
                        if item.id == getattr(direct, "id", None) and match and match[1] in maps:
                            mapping = maps[match[1]]
                        else:
                            unknown.add("choices")
                elif item.type == "update_expression":
                    for child in item.named_children:
                        if (field := field_name(child, variable)) in FIELDS:
                            unknown.add(FIELDS[field])
                if item.type != "call_expression":
                    continue
                target = item.child_by_field_name("function")
                params = item.child_by_field_name("arguments")
                if target is None or params is None:
                    continue
                field = field_name(target, variable)
                if field == "set_default_value":
                    if item.id == getattr(direct, "id", None) and len(params.named_children) == 1:
                        default = params.named_children[0]
                    else:
                        unknown.add("default")
                elif text(target) in (f"{variable}->enum_values.push_back", f"{variable}->enum_values.emplace_back"):
                    value = literal(params)
                    if item.id == getattr(direct, "id", None) and value is not UNKNOWN and len(value) == 1:
                        choices.extend(value)
                    else:
                        unknown.add("choices")
        if conditional:
            unknown.update({"minimum", "maximum", "unit", "nullable", "gui_type", "ratio_over", "choices", "default"})
            definition["status"] = "conditional_definition"
        if native_type in ("coEnum", "coEnums"):
            if mapping:
                definition["choices"] = sorted(mapping)
            else:
                unknown.add("choices")
            if "choices" in unknown:
                unknown.add("default")
        elif choices:
            definition["suggestions"] = choices
        if default is not None:
            resolved = default_value(default, mapping)
            if resolved is not UNKNOWN:
                definition["default"] = {"value": resolved, "source": "native_definition"}
            else:
                unknown.add("default")
        else:
            unknown.add("default")
        for name in unknown:
            definition.pop(name, None)
        if "nullable" not in definition:
            unknown.add("nullable")
        definition["unknown_fields"] = sorted(unknown)
        candidates.setdefault(key, []).append(definition)
    result = {}
    for key in sorted(keys):
        definitions = candidates.get(key, [])
        if len(definitions) == 1:
            result[key] = definitions[0]
        else:
            result[key] = {"status": "unknown", "applicability": "unknown",
                           "reason": "dynamic_or_missing_definition" if not definitions else "multiple_definitions"}
    return result
