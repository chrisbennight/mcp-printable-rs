import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import pickle as P
import struct
import tempfile
import unittest

import flame_model_convert as convert_module
from flame_model_convert import ModelError, convert, main, write_safetensors


CALLS = []


def _record_call(*args):
    CALLS.append(args)


def _int(value):
    return P.BININT + struct.pack("<i", value)


def _tuple(*items):
    return P.MARK + b"".join(items) + P.TUPLE


PACK = {"f8": "d", "i8": "q", "i4": "i", "u4": "I"}


def _raw(code, values):
    return struct.pack(f"<{len(values)}{PACK[code]}", *values)


class Legacy:
    """Protocol 2 pickles with Python 2 strings, as the older published models use."""

    protocol = 2
    sparse_module = "scipy.sparse.csc"

    def glob(self, module, name):
        return P.GLOBAL + f"{module}\n{name}\n".encode()

    def text(self, value):
        data = value if isinstance(value, bytes) else value.encode("latin1")
        if len(data) < 256:
            return P.SHORT_BINSTRING + bytes([len(data)]) + data
        return P.BINSTRING + struct.pack("<i", len(data)) + data

    def dtype(self, code):
        return (self.glob("numpy", "dtype") + _tuple(self.text(code), P.NEWFALSE, P.NEWTRUE) + P.REDUCE
                + _tuple(_int(3), self.text("<"), P.NONE, P.NONE, P.NONE, _int(-1), _int(-1), _int(0))
                + P.BUILD)

    def array(self, code, shape, values, fortran=False):
        reconstruct = (self.glob("numpy.core.multiarray", "_reconstruct")
                       + _tuple(self.glob("numpy", "ndarray"), _tuple(_int(0)), self.text("b")) + P.REDUCE)
        state = _tuple(_int(1), _tuple(*map(_int, shape)), self.dtype(code),
                       P.NEWTRUE if fortran else P.NEWFALSE, self.text(_raw(code, values)))
        return reconstruct + state + P.BUILD

    def basis(self, encoded):
        state = self.mapping([("_make_sparse", P.NEWFALSE),
                              ("_dirty_vars", self.glob("__builtin__", "set") + _tuple(P.EMPTY_LIST) + P.REDUCE),
                              ("x", encoded), ("_status", self.text("new"))])
        return self.glob("chumpy.ch", "Ch") + P.EMPTY_TUPLE + P.NEWOBJ + state + P.BUILD

    def mapping(self, pairs):
        return P.EMPTY_DICT + P.MARK + b"".join(self.text(key) + value for key, value in pairs) + P.SETITEMS


class Modern(Legacy):
    """Protocol 5 pickles with out-of-band style buffers, as FLAME 2023 Open uses."""

    protocol = 5
    sparse_module = "scipy.sparse._csc"

    def glob(self, module, name):
        return self.text(module) + self.text(name) + P.STACK_GLOBAL

    def text(self, value):
        data = value.encode()
        return P.BINUNICODE + struct.pack("<I", len(data)) + data

    def array(self, code, shape, values, fortran=False):
        raw = _raw(code, values)
        return (self.glob("numpy.core.numeric", "_frombuffer")
                + _tuple(P.BYTEARRAY8 + struct.pack("<Q", len(raw)) + raw, self.dtype(code),
                         _tuple(*map(_int, shape)), self.text("F" if fortran else "C")) + P.REDUCE)

    def basis(self, encoded):
        return encoded


TEMPLATE = [0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 0.5]
FACES = [0, 1, 2, 0, 1, 3, 0, 2, 3, 1, 2, 3]
REGRESSOR = [(0, 0, 0.25), (0, 1, 0.25), (0, 2, 0.25), (0, 3, 0.25),
             (1, 0, 0.5), (1, 1, 0.5), (2, 2, 0.5), (2, 3, 0.5)]
WEIGHTS = [1.0, 0.0, 0.0, 0.5, 0.5, 0.0, 0.0, 0.5, 0.5, 0.0, 0.0, 1.0]
VERTICES, JOINTS = 4, 3


def _joint_locations():
    joints = [0.0] * (JOINTS * 3)
    for row, column, weight in REGRESSOR:
        for axis in range(3):
            joints[row * 3 + axis] += weight * TEMPLATE[column * 3 + axis]
    return joints


def _sparse(encoder, rows, columns, entries):
    entries = sorted(entries, key=lambda entry: (entry[1], entry[0]))
    indptr = [0]
    for column in range(columns):
        indptr.append(indptr[-1] + sum(entry[1] == column for entry in entries))
    state = encoder.mapping([
        ("indices", encoder.array("i4", (len(entries),), [row for row, _, _ in entries])),
        ("data", encoder.array("f8", (len(entries),), [value for _, _, value in entries])),
        ("maxprint", _int(50)),
        ("_shape", _tuple(_int(rows), _int(columns))),
        ("indptr", encoder.array("i4", (columns + 1,), indptr)),
    ])
    return encoder.glob(encoder.sparse_module, "csc_matrix") + P.EMPTY_TUPLE + P.NEWOBJ + state + P.BUILD


def model(encoder, components=6, *, overrides=None, omit=(), expression_metadata=None):
    """Build a tetrahedral three-joint model in the given published pickle layout."""
    items = {
        "f": encoder.array("u4", (4, 3), FACES),
        "J_regressor": _sparse(encoder, JOINTS, VERTICES, REGRESSOR),
        "kintree_table": encoder.array("i8", (2, JOINTS), [4294967295, 0, 1, 0, 1, 2]),
        "J": encoder.array("f8", (JOINTS, 3), _joint_locations()),
        "bs_style": encoder.text("lbs"),
        "weights": encoder.array("f8", (VERTICES, JOINTS), WEIGHTS),
        "posedirs": encoder.array("f8", (VERTICES, 3, 18), [(index % 5) * 0.125 for index in range(VERTICES * 3 * 18)]),
        "v_template": encoder.array("f8", (VERTICES, 3), TEMPLATE),
        "shapedirs": encoder.basis(encoder.array(
            "f8", (VERTICES, 3, components), [(index % 7) * 0.0625 for index in range(VERTICES * 3 * components)])),
        "bs_type": encoder.text("lrotmin"),
    }
    if expression_metadata is not None:
        count, source = expression_metadata
        items["supr_expression_metadata"] = encoder.mapping([
            ("expr_source", encoder.text(source)), ("supr_gender", encoder.text("male")),
            ("expr_components", encoder.text(f"last_{count}")), ("n_expr", _int(count))])
    items.update(overrides or {})
    for key in omit:
        items.pop(key)
    return (P.PROTO + bytes([encoder.protocol]) + encoder.mapping(list(items.items())) + P.STOP)


def _floats(payload):
    return list(struct.unpack(f"<{len(payload) // 4}f", payload))


class ConversionTests(unittest.TestCase):
    def test_legacy_model_converts_to_the_fixed_layout(self):
        data = model(Legacy())
        tensors, metadata = convert(data, "test_legacy", expression_components=2)
        self.assertEqual(tensors["v_template"][:2], ("F32", (VERTICES, 3)))
        self.assertEqual(_floats(tensors["v_template"][2]), TEMPLATE)
        self.assertEqual(tensors["faces"], ("U32", (4, 3), struct.pack("<12I", *FACES)))
        self.assertEqual(tensors["kintree_parents"], ("I32", (JOINTS,), struct.pack("<3i", -1, 0, 1)))
        self.assertEqual(tensors["shapedirs"][:2], ("F32", (VERTICES, 3, 6)))
        self.assertEqual(tensors["posedirs"][:2], ("F32", (VERTICES, 3, 18)))
        self.assertEqual(_floats(tensors["lbs_weights"][2]), WEIGHTS)
        dense = [0.0] * (JOINTS * VERTICES)
        for row, column, weight in REGRESSOR:
            dense[row * VERTICES + column] = weight
        self.assertEqual(tensors["joint_regressor"][:2], ("F32", (JOINTS, VERTICES)))
        self.assertEqual(_floats(tensors["joint_regressor"][2]), dense)
        self.assertEqual(metadata["identity_components"], "4")
        self.assertEqual(metadata["expression_components"], "2")
        self.assertEqual(metadata["expression_source"], "flame")
        self.assertEqual(metadata["source_sha256"], hashlib.sha256(data).hexdigest())

    def test_modern_model_uses_its_embedded_expression_basis(self):
        _, metadata = convert(model(Modern(), expression_metadata=(2, "SUPR_head")), "test_modern")
        self.assertEqual((metadata["identity_components"], metadata["expression_components"]), ("4", "2"))
        self.assertEqual(metadata["expression_source"], "SUPR_head")

    def test_both_layouts_produce_identical_tensors(self):
        legacy, _ = convert(model(Legacy()), "same", expression_components=2)
        modern, _ = convert(model(Modern()), "same", expression_components=2)
        self.assertEqual(legacy, modern)

    def test_conventional_basis_size_needs_no_expression_count(self):
        _, metadata = convert(model(Legacy(), components=400), "conventional")
        self.assertEqual((metadata["identity_components"], metadata["expression_components"]), ("300", "100"))

    def test_unrecorded_basis_size_requires_an_expression_count(self):
        with self.assertRaisesRegex(ModelError, "--expression-components"):
            convert(model(Legacy()), "unrecorded")

    def test_expression_count_contradicting_the_source_is_refused(self):
        with self.assertRaisesRegex(ModelError, "contradicts"):
            convert(model(Modern(), expression_metadata=(2, "SUPR_head")), "conflict", expression_components=3)


class SafetyTests(unittest.TestCase):
    def setUp(self):
        CALLS.clear()

    def test_foreign_symbol_is_refused_before_it_runs(self):
        hostile = Legacy().glob(__name__, "_record_call") + _tuple() + P.REDUCE
        with self.assertRaisesRegex(ModelError, "refusing"):
            convert(model(Legacy(), overrides={"v_template": hostile}), "hostile", expression_components=2)
        self.assertEqual(CALLS, [])

    def test_process_execution_symbol_is_refused(self):
        with self.assertRaisesRegex(ModelError, "refusing"):
            convert(P.dumps(os.system, protocol=2), "hostile")

    def test_truncated_source_is_refused(self):
        with self.assertRaisesRegex(ModelError, "not a readable model pickle"):
            convert(model(Legacy())[:-200], "truncated", expression_components=2)

    def test_non_dictionary_source_is_refused(self):
        with self.assertRaisesRegex(ModelError, "dictionary"):
            convert(P.dumps([1, 2, 3], protocol=2), "list")

    def test_sparse_component_declared_without_state_is_refused(self):
        encoder = Legacy()
        bare = (encoder.glob("numpy.core.multiarray", "_reconstruct")
                + _tuple(encoder.glob("numpy", "ndarray"), _tuple(_int(0)), encoder.text("b")) + P.REDUCE)
        state = encoder.mapping([
            ("indices", encoder.array("i4", (1,), [0])), ("data", bare), ("maxprint", _int(50)),
            ("_shape", _tuple(_int(JOINTS), _int(VERTICES))),
            ("indptr", encoder.array("i4", (VERTICES + 1,), [0, 1, 1, 1, 1]))])
        regressor = (encoder.glob(encoder.sparse_module, "csc_matrix") + P.EMPTY_TUPLE + P.NEWOBJ
                     + state + P.BUILD)
        with self.assertRaisesRegex(ModelError, "without its element type"):
            convert(model(encoder, overrides={"J_regressor": regressor}), "bare", expression_components=2)

    def test_fortran_ordered_array_is_refused(self):
        encoder = Modern()
        posedirs = encoder.array("f8", (VERTICES, 3, 18), [0.0] * (VERTICES * 54), fortran=True)
        with self.assertRaisesRegex(ModelError, "Fortran"):
            convert(model(encoder, overrides={"posedirs": posedirs}), "fortran", expression_components=2)

    def test_array_shorter_than_its_shape_is_refused(self):
        encoder = Legacy()
        short = encoder.array("f8", (VERTICES, 3), TEMPLATE[:-3])
        with self.assertRaisesRegex(ModelError, "does not match its shape"):
            convert(model(encoder, overrides={"v_template": short}), "short", expression_components=2)

    def test_child_listed_before_its_parent_is_refused(self):
        encoder = Legacy()
        tree = encoder.array("i8", (2, JOINTS), [4294967295, 2, 0, 0, 1, 2])
        with self.assertRaisesRegex(ModelError, "parent before its child"):
            convert(model(encoder, overrides={"kintree_table": tree}), "tree", expression_components=2)

    def test_joint_locations_disagreeing_with_the_regressor_are_refused(self):
        encoder = Legacy()
        joints = _joint_locations()
        joints[4] += 0.01
        with self.assertRaisesRegex(ModelError, "joint regressor"):
            convert(model(encoder, overrides={"J": encoder.array("f8", (JOINTS, 3), joints)}), "corrupt",
                    expression_components=2)

    def test_missing_joint_locations_are_refused(self):
        with self.assertRaisesRegex(ModelError, "J is missing"):
            convert(model(Legacy(), omit=("J",)), "missing", expression_components=2)

    def test_non_finite_joint_locations_are_refused(self):
        encoder = Legacy()
        joints = _joint_locations()
        joints[4] = float("nan")
        with self.assertRaisesRegex(ModelError, "joint regressor"):
            convert(model(encoder, overrides={"J": encoder.array("f8", (JOINTS, 3), joints)}), "nan",
                    expression_components=2)

    def test_regressor_rows_must_match_the_skinned_joints(self):
        # The declared row count sizes a dense allocation, so it is checked
        # against the decoded skinning weights before anything is allocated.
        encoder = Legacy()
        oversized = _sparse(encoder, 1 << 20, VERTICES, [])
        with self.assertRaisesRegex(ModelError, "skinned joints"):
            convert(model(encoder, overrides={"J_regressor": oversized}), "rows", expression_components=2)

    def test_face_outside_the_template_is_refused(self):
        encoder = Legacy()
        faces = encoder.array("u4", (4, 3), FACES[:-1] + [VERTICES])
        with self.assertRaisesRegex(ModelError, "outside the template"):
            convert(model(encoder, overrides={"f": faces}), "faces", expression_components=2)

    def test_weights_that_do_not_sum_to_one_are_refused(self):
        encoder = Legacy()
        weights = encoder.array("f8", (VERTICES, JOINTS), [0.5] + WEIGHTS[1:])
        with self.assertRaisesRegex(ModelError, "sum to one"):
            convert(model(encoder, overrides={"weights": weights}), "weights", expression_components=2)

    def test_other_skinning_models_are_refused(self):
        encoder = Legacy()
        with self.assertRaisesRegex(ModelError, "lrotmin"):
            convert(model(encoder, overrides={"bs_type": encoder.text("rotmat")}), "skinning",
                    expression_components=2)


class OutputTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)

    def tearDown(self):
        self.directory.cleanup()

    def test_written_file_follows_the_safetensors_layout(self):
        tensors, metadata = convert(model(Legacy()), "layout", expression_components=2)
        path = self.root / "layout.safetensors"
        digest = write_safetensors(path, tensors, metadata)
        blob = path.read_bytes()
        self.assertEqual(digest, hashlib.sha256(blob).hexdigest())
        (length,) = struct.unpack_from("<Q", blob)
        self.assertEqual((8 + length) % 8, 0)
        header = json.loads(blob[8:8 + length])
        self.assertEqual(header.pop("__metadata__"), metadata)
        data = blob[8 + length:]
        spans = sorted((entry["data_offsets"], name) for name, entry in header.items())
        self.assertEqual(spans[0][0][0], 0)
        self.assertEqual(spans[-1][0][1], len(data))
        for (previous, _), (current, _) in zip(spans, spans[1:]):
            self.assertEqual(previous[1], current[0])
        for name, entry in header.items():
            dtype, shape, payload = tensors[name]
            self.assertEqual((entry["dtype"], tuple(entry["shape"])), (dtype, shape))
            start, end = entry["data_offsets"]
            self.assertEqual(data[start:end], payload)

    def test_output_is_readable_according_to_the_umask(self):
        tensors, metadata = convert(model(Legacy()), "mode", expression_components=2)
        previous = os.umask(0o022)
        try:
            write_safetensors(self.root / "mode.safetensors", tensors, metadata)
        finally:
            os.umask(previous)
        self.assertEqual((self.root / "mode.safetensors").stat().st_mode & 0o777, 0o644)

    def test_failed_conversion_leaves_the_destination_untouched(self):
        source, destination = self.root / "source.pkl", self.root / "model.safetensors"
        source.write_bytes(P.dumps(os.system, protocol=2))
        destination.write_bytes(b"previous")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            status = main([str(source), str(destination), "--variant", "flame2020_generic"])
        self.assertEqual(status, 1)
        self.assertIn("refusing", stderr.getvalue())
        self.assertNotIn("Traceback", stderr.getvalue())
        self.assertEqual(destination.read_bytes(), b"previous")
        self.assertEqual(sorted(path.name for path in self.root.iterdir()), ["model.safetensors", "source.pkl"])

    def test_command_reports_the_output_digest_and_metadata(self):
        source, destination = self.root / "source.pkl", self.root / "model.safetensors"
        source.write_bytes(model(Modern(), expression_metadata=(2, "SUPR_head")))
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            status = main([str(source), str(destination), "--variant", "flame2023_open"])
        self.assertEqual(status, 0)
        report = json.loads(stdout.getvalue())
        self.assertEqual(report["sha256"], hashlib.sha256(destination.read_bytes()).hexdigest())
        self.assertEqual((report["variant"], report["expression_source"]), ("flame2023_open", "SUPR_head"))

    def test_invalid_variant_label_is_refused(self):
        with self.assertRaisesRegex(ModelError, "variant"):
            convert(model(Legacy()), "Not A Label", expression_components=2)

    def test_oversized_source_is_refused_without_reading_it_all(self):
        source = self.root / "large.pkl"
        source.write_bytes(b"\0" * 17)
        original = convert_module.MAX_SOURCE_BYTES
        convert_module.MAX_SOURCE_BYTES = 16
        try:
            with self.assertRaisesRegex(ModelError, "exceeds"):
                convert_module.read_bounded(source)
        finally:
            convert_module.MAX_SOURCE_BYTES = original


@unittest.skipUnless(os.environ.get("PRINTABLE_TEST_FLAME_PICKLE"),
                     "set PRINTABLE_TEST_FLAME_PICKLE to a published FLAME model to run")
class PublishedModelTests(unittest.TestCase):
    def test_published_model_converts(self):
        source = Path(os.environ["PRINTABLE_TEST_FLAME_PICKLE"])
        tensors, metadata = convert(convert_module.read_bounded(source), "published")
        vertices = tensors["v_template"][1][0]
        self.assertEqual(tensors["v_template"][1], (vertices, 3))
        self.assertEqual(tensors["lbs_weights"][1], (vertices, 5))
        self.assertEqual(tensors["kintree_parents"][1], (5,))
        self.assertEqual(int(metadata["identity_components"]) + int(metadata["expression_components"]),
                         tensors["shapedirs"][1][2])


if __name__ == "__main__":
    unittest.main()
