#!/usr/bin/env python3
"""Convert a published FLAME head model into the safetensors file Printable reads.

The published model files are Python pickles, and loading a pickle can run
arbitrary code. This converter never constructs a foreign object: every symbol
the published files reference maps to an inert stand-in that only records the
data it receives, and any other symbol is refused before it is resolved. Only
the standard library is used, so conversion needs no scientific Python stack.

The output holds float32 and integer arrays in a fixed layout, with metadata
naming the variant, the source digest and the identity and expression basis
sizes. Decoding is checked against the model's own redundancy: the stored
joint locations must equal the joint regressor applied to the template mesh.
"""

from __future__ import annotations

import argparse
import array
import ast
import hashlib
import io
import json
import math
import os
from pathlib import Path
import pickle
import pickletools
import re
import resource
import struct
import sys
import tempfile
import zipfile


FORMAT = "printable-flame/1"
# Published models are about 53 MB. The cap bounds memory, which grows
# linearly with the source.
MAX_SOURCE_BYTES = 256 << 20
# Published models use well under a hundred memo slots.
MAX_MEMO_INDEX = 1 << 16
MEMO_STORES = {"PUT", "BINPUT", "LONG_BINPUT"}
# A published model converts in about a second using about 200 MiB. The
# command runs within these limits, far above that even at the source cap, so
# a source that drives the unpickler into runaway work fails instead of
# exhausting the machine.
CPU_SECONDS = 60
ADDRESS_SPACE_BYTES = 4 << 30
# Stand-ins copy only values bounded by these sizes, so a value that a source
# memoizes and references many times costs a constant amount per reference.
MAX_ARRAY_RANK = 3
MAX_DIMENSION = 1 << 31
MAX_NAME_IN_MESSAGE = 80
MAX_TYPE_CODE = 3
VARIANT = re.compile(r"[a-z0-9][a-z0-9._-]{0,63}")
SOURCE_LABEL = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}")
# Published FLAME bases order 300 identity components before 100 expression
# components; files that predate embedded metadata rely on this convention.
CONVENTIONAL_BASIS_COMPONENTS = 400
CONVENTIONAL_EXPRESSION_COMPONENTS = 100
JOINT_TOLERANCE = 1e-6
UV_TOLERANCE = 1e-6
WEIGHT_TOLERANCE = 1e-3
TYPECODES = {"f8": "d", "f4": "f", "i8": "q", "i4": "i", "u8": "Q", "u4": "I",
             "i2": "h", "u2": "H", "i1": "b", "u1": "B"}


class ModelError(ValueError):
    """The source is not a FLAME model this converter can represent faithfully."""


class _DType:
    """Records a NumPy dtype descriptor without constructing one."""

    def __init__(self, code, align=False, copy=True):
        if not isinstance(code, str) or len(code) > MAX_TYPE_CODE:
            raise ModelError("array element type is not a type code")
        self.code = code.lstrip("<>|=")
        self.byteorder = code[0] if code[:1] in ("<", ">", "|") else "<"

    def __setstate__(self, state):
        # NumPy dtype state: (version, byte order, sub-array, names, fields, ...).
        if (not isinstance(state, tuple) or len(state) < 5
                or any(part is not None for part in state[2:5])):
            raise ModelError("structured or sub-array element types are not supported")
        if state[1] not in ("<", ">", "|"):
            raise ModelError("array byte order is not recorded explicitly")
        self.byteorder = state[1]


class _Array:
    """Records an array's element type, shape and raw bytes without constructing it."""

    def __init__(self, dtype=None, shape=(), data=b"", fortran=False):
        self.dtype = dtype
        self.shape = _shape(shape)
        self.data = data
        self.fortran = fortran

    def __setstate__(self, state):
        # Legacy ndarray state: (version, shape, dtype, Fortran order, raw bytes).
        if not isinstance(state, tuple) or len(state) != 5:
            raise ModelError("array state is not in the NumPy raw-data layout")
        _, shape, dtype, fortran, data = state
        # Python 2 raw data arrives as a Latin-1 string. It is converted only
        # when read, so one string shared by many arrays is not copied for each.
        if not isinstance(dtype, _DType) or not isinstance(data, (str, bytes, bytearray)):
            raise ModelError("array state does not hold raw numeric data")
        self.dtype, self.shape, self.data, self.fortran = dtype, _shape(shape), data, bool(fortran)

    def values(self):
        """Return the elements in C order, in native byte order."""
        if not isinstance(self.dtype, _DType):
            raise ModelError("array was declared without its element type and data")
        if self.fortran and sum(size > 1 for size in self.shape) > 1:
            raise ModelError("Fortran-ordered arrays are not supported")
        typecode = TYPECODES.get(self.dtype.code)
        if typecode is None:
            raise ModelError(f"element type {self.dtype.code} is not supported")
        values = array.array(typecode)
        if values.itemsize != int(self.dtype.code[1:]):
            raise ModelError(f"element type {self.dtype.code} has no exact native equivalent")
        if len(self.data) != math.prod(self.shape) * values.itemsize:
            raise ModelError("array data length does not match its shape")
        data = self.data
        if isinstance(data, str):
            try:
                data = data.encode("latin1")
            except UnicodeEncodeError:
                raise ModelError("array data is not raw bytes") from None
        values.frombytes(data)
        if values.itemsize > 1 and (self.dtype.byteorder == ">") != (sys.byteorder == "big"):
            values.byteswap()
        return values


class _Ch:
    """Records a chumpy array's state; only its stored value is used."""

    def __setstate__(self, state):
        self.state = state


class _SparseCSC:
    """Records a SciPy compressed-sparse-column matrix's state."""

    def __setstate__(self, state):
        self.state = state


def _shape(shape):
    # Each dimension must be a small integer, so a shape can always be copied,
    # multiplied and printed cheaply.
    if (not isinstance(shape, (tuple, list)) or len(shape) > MAX_ARRAY_RANK
            or not all(isinstance(size, int) and not isinstance(size, bool)
                       and 0 <= size <= MAX_DIMENSION for size in shape)):
        raise ModelError("array shape is not a FLAME array shape")
    return tuple(shape)


def _empty_set(items=()):
    # Published models only build empty sets.
    if not isinstance(items, (list, tuple)) or items:
        raise ModelError("only empty sets appear in FLAME models")
    return set()


def _reconstruct(cls, shape, typecode):
    if cls is not _Array:
        raise ModelError("array reconstruction names an unexpected class")
    return _Array()


def _frombuffer(buffer, dtype, shape, order):
    if not isinstance(buffer, (bytes, bytearray)) or not isinstance(dtype, _DType):
        raise ModelError("array buffer does not hold raw numeric data")
    if order not in ("C", "F"):
        raise ModelError("array memory order is not recognised")
    return _Array(dtype, shape, buffer, fortran=order == "F")


# Every symbol a published FLAME model references, spelled as NumPy 1 and 2 and
# Python 2 and 3 write them. Each maps to an inert stand-in.
ALLOWED = {
    ("numpy.core.multiarray", "_reconstruct"): _reconstruct,
    ("numpy._core.multiarray", "_reconstruct"): _reconstruct,
    ("numpy.core.numeric", "_frombuffer"): _frombuffer,
    ("numpy._core.numeric", "_frombuffer"): _frombuffer,
    ("numpy", "ndarray"): _Array,
    ("numpy", "dtype"): _DType,
    ("scipy.sparse.csc", "csc_matrix"): _SparseCSC,
    ("scipy.sparse._csc", "csc_matrix"): _SparseCSC,
    ("chumpy.ch", "Ch"): _Ch,
    ("__builtin__", "set"): _empty_set,
    ("builtins", "set"): _empty_set,
}


class _RestrictedUnpickler(pickle.Unpickler):
    def find_class(self, module, name):
        try:
            return ALLOWED[(module, name)]
        except KeyError:
            shown = f"{module[:MAX_NAME_IN_MESSAGE]}.{name[:MAX_NAME_IN_MESSAGE]}"
            raise ModelError(f"refusing {shown}: it is not part of a FLAME model") from None


def _opcodes(data: bytes):
    """Yield each opcode's name and argument, as pickletools.genops does.

    Protocol 0 strings are read as raw lines rather than decoded: Python 2
    wrote binary array data into them as escaped bytes, which genops refuses
    to decode as ASCII. No opcode is executed.
    """
    stream = io.BytesIO(data)
    while True:
        code = stream.read(1)
        opcode = pickletools.code2op.get(code.decode("latin1"))
        if opcode is None:
            raise ValueError("unknown or missing pickle opcode")
        if opcode.arg is None:
            argument = None
        elif opcode.arg is pickletools.stringnl:
            argument = stream.readline()
            if not argument.endswith(b"\n"):
                raise ValueError("protocol 0 string has no terminating newline")
        else:
            argument = opcode.arg.reader(stream)
        yield opcode.name, argument
        if opcode.name == "STOP":
            return


def load_source(data: bytes) -> dict:
    """Decode a FLAME model pickle into stand-in objects without running foreign code."""
    try:
        # The C unpickler sizes its memo table from the largest explicit index,
        # so a few bytes could demand an arbitrarily large allocation. Decoding
        # the opcodes first builds no objects.
        for name, argument in _opcodes(data):
            if name in MEMO_STORES and argument > MAX_MEMO_INDEX:
                raise ModelError("source declares a memo index far beyond any FLAME model")
        model = _RestrictedUnpickler(io.BytesIO(data), encoding="latin1").load()
    except ModelError:
        raise
    except (pickle.UnpicklingError, EOFError, TypeError, ValueError,
            AttributeError, IndexError, KeyError, MemoryError) as error:
        raise ModelError(f"source is not a readable model pickle ({type(error).__name__})") from None
    if not isinstance(model, dict):
        raise ModelError("source pickle does not hold a model dictionary")
    return model


def _array(model, key):
    value = model.get(key)
    if isinstance(value, _Ch):
        value = value.state.get("x") if isinstance(value.state, dict) else None
    if not isinstance(value, _Array):
        raise ModelError(f"{key} is missing or is not a numeric array")
    return value


def _shaped(model, key, rank):
    value = _array(model, key)
    if len(value.shape) != rank:
        raise ModelError(f"{key} must have {rank} dimensions, found shape {value.shape}")
    return value


def _joint_regressor(model, vertices, joints):
    """Return the regressor densely as joints x vertices.

    Both sizes come from arrays already checked against their bytes, so the
    dense allocation is bounded by data present in the source.
    """
    value = model.get("J_regressor")
    if isinstance(value, _Array):
        if value.shape != (joints, vertices):
            raise ModelError("J_regressor does not map the template vertices to the skinned joints")
        return array.array("d", value.values())
    if not isinstance(value, _SparseCSC) or not isinstance(value.state, dict):
        raise ModelError("J_regressor is missing or is not a sparse matrix")
    state = value.state
    if state.get("_shape") != (joints, vertices):
        raise ModelError("J_regressor does not map the template vertices to the skinned joints")
    rows, columns = joints, vertices
    data, indices, indptr = (_array(state, key).values() for key in ("data", "indices", "indptr"))
    if len(indptr) != columns + 1 or indptr[0] != 0 or indptr[-1] != len(data) or len(indices) != len(data):
        raise ModelError("J_regressor sparse structure is inconsistent")
    dense = array.array("d", bytes(8 * rows * columns))
    for column in range(columns):
        start, end = indptr[column], indptr[column + 1]
        if not 0 <= start <= end <= len(data):
            raise ModelError("J_regressor sparse structure is inconsistent")
        for entry in range(start, end):
            row = indices[entry]
            if not 0 <= row < rows:
                raise ModelError("J_regressor references a joint outside its shape")
            # Duplicate entries add, matching sparse-matrix semantics.
            dense[row * columns + column] += data[entry]
    return dense


def _expression_split(model, components, requested):
    recorded = model.get("supr_expression_metadata")
    source = "flame"
    if recorded is not None:
        if not isinstance(recorded, dict) or not isinstance(recorded.get("n_expr"), int):
            raise ModelError("embedded expression metadata is malformed")
        count = recorded["n_expr"]
        # Range-check before formatting; Python refuses to print huge integers.
        if isinstance(count, bool) or not 0 <= count <= components:
            raise ModelError("embedded expression count is outside the basis size")
        if recorded.get("expr_components") != f"last_{count}":
            raise ModelError("embedded expression metadata does not describe trailing components")
        label = recorded.get("expr_source")
        if not isinstance(label, str) or not SOURCE_LABEL.fullmatch(label):
            raise ModelError("embedded expression source is not a plain label")
        source = label
        if requested is not None and requested != count:
            raise ModelError(f"--expression-components {requested} contradicts the {count} recorded in the source")
    elif requested is not None:
        count = requested
    elif components == CONVENTIONAL_BASIS_COMPONENTS:
        count = CONVENTIONAL_EXPRESSION_COMPONENTS
    else:
        raise ModelError("the source does not record its expression basis size; pass --expression-components")
    if not 0 <= count <= components:
        raise ModelError("expression component count exceeds the basis size")
    return components - count, count, source


def _float32(name, values):
    converted = array.array("f", values)
    if not all(map(math.isfinite, converted)):
        raise ModelError(f"{name} contains values that are not finite in float32")
    return converted


def _payload(values):
    if sys.byteorder == "big" and values.itemsize > 1:
        values = array.array(values.typecode, values)
        values.byteswap()
    return values.tobytes()


def convert(data: bytes, variant: str, expression_components: int | None = None):
    """Return (tensors, metadata) for a FLAME model pickle.

    Tensors map a name to (safetensors dtype, shape, little-endian bytes).
    """
    if not VARIANT.fullmatch(variant):
        raise ModelError("variant must be lowercase letters, digits, '.', '_' or '-'")
    model = load_source(data)
    if model.get("bs_style") != "lbs" or model.get("bs_type") != "lrotmin":
        raise ModelError("only linear blend skinning with lrotmin pose features is supported")

    template = _shaped(model, "v_template", 2)
    vertices = template.shape[0]
    if template.shape[1] != 3:
        raise ModelError("v_template must hold 3D vertices")
    template_values = template.values()

    faces = _shaped(model, "f", 2)
    if faces.shape[1] != 3:
        raise ModelError("f must hold triangles")
    face_values = faces.values()
    if any(not 0 <= index < vertices for index in face_values):
        raise ModelError("f references a vertex outside the template")

    shapedirs = _shaped(model, "shapedirs", 3)
    if shapedirs.shape[:2] != (vertices, 3):
        raise ModelError("shapedirs does not match the template vertices")
    posedirs = _shaped(model, "posedirs", 3)
    if posedirs.shape[:2] != (vertices, 3):
        raise ModelError("posedirs does not match the template vertices")

    weights = _shaped(model, "weights", 2)
    if weights.shape[0] != vertices or weights.shape[1] < 1:
        raise ModelError("weights do not assign every template vertex to the joints")
    # Decoding checks the weights against their bytes, which bounds the joint
    # count and every allocation sized by it.
    weight_values = weights.values()
    joints = weights.shape[1]
    for vertex in range(vertices):
        total = math.fsum(weight_values[vertex * joints:(vertex + 1) * joints])
        if abs(total - 1.0) > WEIGHT_TOLERANCE:
            raise ModelError("skinning weights do not sum to one for every vertex")

    regressor = _joint_regressor(model, vertices, joints)
    if posedirs.shape[2] != 9 * (joints - 1):
        raise ModelError("posedirs does not hold one rotation-matrix basis per non-root joint")

    kintree = _shaped(model, "kintree_table", 2)
    if kintree.shape != (2, joints):
        raise ModelError("kintree_table does not describe every joint")
    tree = kintree.values()
    if list(tree[joints:]) != list(range(joints)):
        raise ModelError("kintree_table joint identifiers are not in order")
    parents = array.array("i", [-1])
    for joint in range(1, joints):
        parent = tree[joint]
        if not 0 <= parent < joint:
            raise ModelError("kintree_table does not list every parent before its child")
        parents.append(parent)
    if 0 <= tree[0] < joints:
        raise ModelError("kintree_table root joint has a parent")

    stored = _shaped(model, "J", 2)
    if stored.shape != (joints, 3):
        raise ModelError("J does not hold one location per joint")
    stored_values = stored.values()
    for joint in range(joints):
        row = regressor[joint * vertices:(joint + 1) * vertices]
        for axis in range(3):
            computed = math.fsum(weight * template_values[vertex * 3 + axis]
                                 for vertex, weight in enumerate(row) if weight)
            # Phrased so that a non-finite stored value fails the comparison.
            if not abs(computed - stored_values[joint * 3 + axis]) <= JOINT_TOLERANCE:
                raise ModelError("stored joint locations disagree with the joint regressor; "
                                 "the source is corrupt or was decoded incorrectly")

    identity, expression, source = _expression_split(model, shapedirs.shape[2], expression_components)

    tensors = {
        "v_template": ("F32", template.shape, _payload(_float32("v_template", template_values))),
        "faces": ("U32", faces.shape, _payload(array.array("I", face_values))),
        "shapedirs": ("F32", shapedirs.shape, _payload(_float32("shapedirs", shapedirs.values()))),
        "posedirs": ("F32", posedirs.shape, _payload(_float32("posedirs", posedirs.values()))),
        "lbs_weights": ("F32", weights.shape, _payload(_float32("lbs_weights", weight_values))),
        "joint_regressor": ("F32", (joints, vertices), _payload(_float32("joint_regressor", regressor))),
        "kintree_parents": ("I32", (joints,), _payload(parents)),
    }
    metadata = {
        "format": FORMAT,
        "variant": variant,
        "source_sha256": hashlib.sha256(data).hexdigest(),
        "identity_components": str(identity),
        "expression_components": str(expression),
        "expression_source": source,
        "skinning": "lbs",
        "pose_feature": "lrotmin",
        "units": "metres",
    }
    return tensors, metadata


# The texture space archive holds large appearance bases next to the small UV
# layout; only these members are read, and each within this size.
MAX_LAYOUT_MEMBER_BYTES = 1 << 20
NPY_TYPES = {"<f8": "d", "<f4": "f", "<u4": "I", "<i4": "i", "<i8": "q", "<u8": "Q"}


def _npy(archive: zipfile.ZipFile, name: str):
    """Decode one little-endian, C-ordered array from an .npz archive."""
    try:
        info = archive.getinfo(f"{name}.npy")
    except KeyError:
        raise ModelError(f"texture space has no {name} array") from None
    if info.file_size > MAX_LAYOUT_MEMBER_BYTES:
        raise ModelError(f"texture space array {name} is too large")
    with archive.open(info) as handle:
        data = handle.read(MAX_LAYOUT_MEMBER_BYTES + 1)
    return _parse_npy(data, f"texture space array {name}")


def _parse_npy(data: bytes, what: str):
    """Decode one little-endian, C-ordered two-dimensional array from the
    bytes of an .npy file, without unpickling anything."""
    if data[:6] != b"\x93NUMPY" or len(data) < 10:
        raise ModelError(f"{what} is not a NumPy array")
    major = data[6]
    if major == 1:
        length, start = struct.unpack("<H", data[8:10])[0], 10
    elif major in (2, 3):
        length, start = struct.unpack("<I", data[8:12])[0], 12
    else:
        raise ModelError(f"{what} has an unknown format version")
    try:
        header = ast.literal_eval(data[start:start + length].decode("latin1"))
    except (ValueError, SyntaxError, RecursionError, MemoryError):
        raise ModelError(f"{what} has an unreadable header") from None
    if not isinstance(header, dict) or header.get("fortran_order") is not False:
        raise ModelError(f"{what} must be C-ordered")
    code = NPY_TYPES.get(header.get("descr"))
    shape = header.get("shape")
    if code is None or not isinstance(shape, tuple) or len(shape) != 2 \
            or not all(isinstance(size, int) and 0 < size < MAX_DIMENSION for size in shape):
        raise ModelError(f"{what} has an unsupported type or shape")
    body = data[start + length:]
    count = shape[0] * shape[1]
    if len(body) != count * struct.calcsize(code):
        raise ModelError(f"{what} is truncated")
    return shape, struct.unpack(f"<{count}{code}", body)


def texture_layout(path: Path, faces: int):
    """FLAME's UV layout from the texture space archive, as tensors: per-UV
    coordinates and, for each triangle, the UV indices of its corners."""
    try:
        with zipfile.ZipFile(path) as archive:
            (coord_shape, coords), (corner_shape, corners) = _npy(archive, "vt"), _npy(archive, "ft")
    except zipfile.BadZipFile:
        raise ModelError("texture space is not an .npz archive") from None
    if coord_shape[1] != 2:
        raise ModelError("texture space vt must hold 2D coordinates")
    if corner_shape != (faces, 3):
        raise ModelError("texture space ft must give UV corners for every model triangle")
    if not all(-UV_TOLERANCE <= value <= 1 + UV_TOLERANCE for value in coords):
        raise ModelError("texture space coordinates must lie in the unit square")
    if not all(0 <= index < coord_shape[0] for index in corners):
        raise ModelError("texture space ft references a coordinate that does not exist")
    return {
        "uv_coords": ("F32", coord_shape, _payload(_float32("uv_coords", coords))),
        "uv_faces": ("U32", corner_shape, _payload(array.array("I", corners))),
    }


# Each eyelid basis holds one displacement per vertex, about 120 KB for FLAME.
MAX_EYELID_BYTES = 1 << 20
# No vertex of a published eyelid basis moves more than a few millimetres;
# this bound, in metres, refuses a basis in other units.
MAX_EYELID_DISPLACEMENT_M = 0.05
EYELID_TENSORS = ("eyelid.left", "eyelid.right")


def eyelid_basis(left: Path, right: Path, vertices: int):
    """The eyelid basis from SMIRK's l_eyelid.npy and r_eyelid.npy, the
    per-vertex displacements that close each eye, as eyelid.left and
    eyelid.right."""
    tensors = {}
    for name, path in zip(EYELID_TENSORS, (left, right)):
        shape, values = _parse_npy(read_bounded(path, MAX_EYELID_BYTES, f"{name} file"), name)
        if shape != (vertices, 3):
            raise ModelError(f"{name} must hold one 3D displacement for each of the model's {vertices} vertices")
        if not all(math.isfinite(value) and abs(value) < MAX_EYELID_DISPLACEMENT_M for value in values):
            raise ModelError(f"{name} displacements must be finite and under 5 cm, in metres")
        tensors[name] = ("F32", shape, _payload(_float32(name, values)))
    return tensors


# FLAME_masks.pkl is about 210 KB and names 14 regions.
MAX_MASKS_BYTES = 4 << 20
MAX_REGIONS = 64
REGION_NAME = re.compile(r"[a-z][a-z0-9_]{0,31}")
REGION_PREFIX = "region."
INTEGER_CODES = {"i8", "i4", "i2", "i1", "u8", "u4", "u2", "u1"}


def regions(data: bytes, vertices: int):
    """FLAME's named vertex regions from FLAME_masks.pkl, as tensors of
    sorted vertex indices named region.<name>."""
    try:
        masks = load_source(data)
    except ModelError as error:
        raise ModelError(f"masks: {error}") from None
    if not 0 < len(masks) <= MAX_REGIONS:
        raise ModelError(f"masks must name between 1 and {MAX_REGIONS} regions")
    tensors = {}
    for name, value in masks.items():
        if not isinstance(name, str) or not REGION_NAME.fullmatch(name):
            raise ModelError("mask names must be lowercase letters, digits and '_', starting with a letter")
        if (not isinstance(value, _Array) or len(value.shape) != 1
                or not isinstance(value.dtype, _DType) or value.dtype.code not in INTEGER_CODES):
            raise ModelError(f"mask {name} is not a list of vertex indices")
        # The published lips mask lists some vertices twice; a region is a set.
        indices = sorted(set(value.values()))
        if not indices or not 0 <= indices[0] <= indices[-1] < vertices:
            raise ModelError(f"mask {name} is empty or names a vertex outside the model")
        tensors[REGION_PREFIX + name] = ("U32", (len(indices),), _payload(array.array("I", indices)))
    return tensors


def write_safetensors(path: Path, tensors, metadata) -> str:
    """Atomically write tensors as safetensors and return the file's SHA-256."""
    header = {"__metadata__": metadata}
    offset = 0
    for name in sorted(tensors):
        dtype, shape, payload = tensors[name]
        header[name] = {"dtype": dtype, "shape": list(shape), "data_offsets": [offset, offset + len(payload)]}
        offset += len(payload)
    encoded = json.dumps(header, separators=(",", ":"), sort_keys=True).encode()
    # Pad with spaces so tensor data starts on an 8-byte boundary.
    encoded += b" " * (-len(encoded) % 8)
    digest = hashlib.sha256()
    descriptor, temporary = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".partial")
    try:
        with os.fdopen(descriptor, "wb") as handle:
            for chunk in (struct.pack("<Q", len(encoded)), encoded,
                          *(tensors[name][2] for name in sorted(tensors))):
                handle.write(chunk)
                digest.update(chunk)
            handle.flush()
            os.fsync(handle.fileno())
        # mkstemp creates owner-only files; apply the umask as ordinary file
        # creation would, so a server running as another user can read the model.
        mask = os.umask(0)
        os.umask(mask)
        os.chmod(temporary, 0o666 & ~mask)
        os.replace(temporary, path)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise
    return digest.hexdigest()


def read_bounded(path: Path, limit: int | None = None, what: str = "source") -> bytes:
    limit = MAX_SOURCE_BYTES if limit is None else limit
    with path.open("rb") as handle:
        data = handle.read(limit + 1)
    if len(data) > limit:
        raise ModelError(f"{what} exceeds {limit >> 20} MiB")
    return data


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        description="Convert a published FLAME model pickle into the safetensors file Printable reads.")
    parser.add_argument("source", type=Path, help="published FLAME model .pkl file")
    parser.add_argument("destination", type=Path, help="output .safetensors file")
    parser.add_argument("--variant", required=True,
                        help="label recorded in the output, for example flame2023_open")
    parser.add_argument("--expression-components", type=int,
                        help="trailing basis components that model expression, for sources that do not record it")
    parser.add_argument("--texture", type=Path,
                        help="FLAME_texture.npz from the FLAME texture space, to add the UV layout "
                             "that surface detail is applied through")
    parser.add_argument("--masks", type=Path,
                        help="FLAME_masks.pkl from the FLAME vertex masks, to add named regions such as "
                             "the scalp, face, ears and eyeballs")
    parser.add_argument("--eyelids", type=Path, nargs=2, metavar=("LEFT", "RIGHT"),
                        help="l_eyelid.npy and r_eyelid.npy, an eyelid basis obtained under its own licence, "
                             "to let heads close their eyelids")
    args = parser.parse_args(argv)
    try:
        tensors, metadata = convert(read_bounded(args.source), args.variant, args.expression_components)
        if args.texture is not None:
            tensors.update(texture_layout(args.texture, tensors["faces"][1][0]))
        if args.masks is not None:
            data = read_bounded(args.masks, MAX_MASKS_BYTES, "masks file")
            tensors.update(regions(data, tensors["v_template"][1][0]))
        if args.eyelids is not None:
            tensors.update(eyelid_basis(*args.eyelids, tensors["v_template"][1][0]))
        digest = write_safetensors(args.destination, tensors, metadata)
    except (ModelError, OSError) as error:
        print(f"flame_model_convert: {error}", file=sys.stderr)
        return 1
    names = sorted(name.removeprefix(REGION_PREFIX) for name in tensors if name.startswith(REGION_PREFIX))
    print(json.dumps({"destination": str(args.destination), "sha256": digest, "regions": names,
                      "eyelids": EYELID_TENSORS[0] in tensors, **metadata}, sort_keys=True))
    return 0


def limit_resources():
    """Lower this process's CPU, address-space and core-dump limits."""
    for kind, value in ((resource.RLIMIT_CPU, CPU_SECONDS),
                        (resource.RLIMIT_AS, ADDRESS_SPACE_BYTES),
                        (resource.RLIMIT_CORE, 0)):
        _, hard = resource.getrlimit(kind)
        if hard != resource.RLIM_INFINITY:
            value = min(value, hard)
        resource.setrlimit(kind, (value, hard))


if __name__ == "__main__":
    limit_resources()
    sys.exit(main())
