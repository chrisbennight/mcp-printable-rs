import unittest

import orca_definitions


SOURCE = '''
static t_config_enum_values s_keys_map_Mode {
    {"old", mOld}, /* GUI can omit aliases */ {"new", mNew}, {"alias", mNew}
};
void PrintConfigDef::init() {
    ConfigOptionDef* def;
    def = this->add("mode", coEnum);
    def->enum_keys_map = &ConfigOptionEnum<Mode>::get_enum_values();
    def->enum_values.push_back("new");
    def->set_default_value(new ConfigOptionEnum<Mode>(mOld));
    def = this->add("brim", coFloat);
    def->gui_type = ConfigOptionDef::GUIType::f_enum_open;
    def->sidetext = L("mm");
    def->min = -1;
    def->enum_values.push_back("-1");
    def->set_default_value(new ConfigOptionFloat(3.));
    def = this->add("conditional", coInt);
    def->min = 0;
    if (hardware) def->min = 10;
    def->set_default_value(new ConfigOptionInt(SOME_CONSTANT));
    def = this->add("percent", coFloatOrPercent);
    def->set_default_value(new ConfigOptionFloatOrPercent(50., true));
    def = this->add("nullable", coFloats);
    def->nullable = true;
    def->set_default_value(new ConfigOptionFloats{1., 2.});
}
void CLIConfigDef::init() {
    def = this->add("brim", coString);
    def->set_default_value(new ConfigOptionString("wrong scope"));
}
'''


class OrcaDefinitionsTests(unittest.TestCase):
    def test_closed_enums_include_native_aliases_and_open_values_are_suggestions(self):
        values = orca_definitions.extract(SOURCE, {"mode", "brim"})
        self.assertEqual(values["mode"]["choices"], ["alias", "new", "old"])
        self.assertEqual(values["mode"]["default"]["value"], "old")
        self.assertEqual(values["brim"]["suggestions"], ["-1"])
        self.assertNotIn("choices", values["brim"])
        self.assertEqual(values["brim"]["minimum"], -1)
        self.assertEqual(values["brim"]["default"]["value"], 3)
        self.assertEqual(values["brim"]["unit"], "mm")

    def test_conditions_expressions_missing_keys_and_duplicates_remain_unknown(self):
        values = orca_definitions.extract(SOURCE, {"conditional", "missing"})
        self.assertNotIn("minimum", values["conditional"])
        self.assertIn("minimum", values["conditional"]["unknown_fields"])
        self.assertIn("default", values["conditional"]["unknown_fields"])
        self.assertEqual(values["missing"]["status"], "unknown")
        duplicate = SOURCE.replace('def = this->add("percent", coFloatOrPercent);',
                                   'def = this->add("brim", coFloatOrPercent);')
        self.assertEqual(orca_definitions.extract(duplicate, {"brim"})["brim"]["reason"],
                         "multiple_definitions")

    def test_percent_defaults_and_nullable_vectors_are_not_profile_values(self):
        values = orca_definitions.extract(SOURCE, {"percent", "nullable"})
        self.assertEqual(values["percent"]["default"]["value"], {"value": 50, "percent": True})
        self.assertEqual(values["nullable"]["default"]["value"], [1, 2])
        self.assertTrue(values["nullable"]["nullable"])
        self.assertTrue(values["nullable"]["vector"])
        self.assertEqual(values["nullable"]["applicability"], "unknown")

    def test_unresolved_updates_and_malformed_statements_do_not_report_literal_bounds(self):
        for replacement in ["def->min += 2;", "def->min++;", "def->min = broken ???;"]:
            source = SOURCE.replace("def->min = -1;", "def->min = -1; " + replacement)
            with self.subTest(replacement=replacement):
                definition = orca_definitions.extract(source, {"brim"})["brim"]
                self.assertNotIn("minimum", definition)
                self.assertIn("minimum", definition["unknown_fields"])

    def test_vector_constructor_lists_keep_their_native_shape(self):
        source = '''void PrintConfigDef::init() {
            def = this->add("numbers", coFloats);
            def->set_default_value(new ConfigOptionFloats({0}));
            def = this->add("points", coPoints);
            def->set_default_value(new ConfigOptionPoints({Vec2d(1., 2.)}));
            def = this->add("overload", coFloats);
            def->set_default_value(new ConfigOptionFloats(3, 0.));
            def = this->add("gui", coFloat);
            def->gui_type = other->gui_type;
        }'''
        values = orca_definitions.extract(source, {"numbers", "points", "overload", "gui"})
        self.assertEqual(values["numbers"]["default"]["value"], [0])
        self.assertEqual(values["points"]["default"]["value"], [[1, 2]])
        self.assertNotIn("default", values["overload"])
        self.assertIn("default", values["overload"]["unknown_fields"])
        self.assertNotIn("gui_type", values["gui"])
        self.assertIn("gui_type", values["gui"]["unknown_fields"])


if __name__ == "__main__":
    unittest.main()
