"""Check the shipped sparse attention against independent, dense reference math."""

import importlib.util
import os
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

import torch


@unittest.skipUnless(importlib.util.find_spec("trellis2"), "the pinned TRELLIS source is installed in the reconstruction image")
class SparseAttentionTests(unittest.TestCase):
    def setUp(self):
        from trellis2.modules.sparse import config
        from trellis2.modules.sparse.basic import VarLenTensor
        from trellis2.modules.sparse.attention.full_attn import sparse_scaled_dot_product_attention

        self.previous = config.ATTN
        config.set_attn_backend("sdpa")
        self.addCleanup(config.set_attn_backend, self.previous)
        self.tensor = VarLenTensor
        self.attention = sparse_scaled_dot_product_attention
        torch.manual_seed(42)

    def reference(self, q, k, v):
        q, k, v = (value.transpose(0, 1) for value in (q, k, v))
        weights = (q @ k.transpose(-1, -2) / q.shape[-1] ** 0.5).softmax(-1)
        return (weights @ v).transpose(0, 1)

    def test_packed_self_attention_keeps_sequences_separate(self):
        sequences = [torch.randn(length, 3, 2, 8) for length in (3, 7)]
        result = self.attention(self.tensor.from_tensor_list(sequences))
        expected = torch.cat([self.reference(*value.unbind(1)) for value in sequences])
        torch.testing.assert_close(result.feats, expected)
        self.assertEqual(result.layout, [slice(0, 3), slice(3, 10)])

    def test_sparse_queries_and_dense_key_values(self):
        queries = [torch.randn(length, 2, 8) for length in (2, 5)]
        kv = torch.randn(2, 4, 2, 2, 8)
        result = self.attention(self.tensor.from_tensor_list(queries), kv)
        expected = torch.cat([self.reference(q, *values.unbind(1)) for q, values in zip(queries, kv)])
        torch.testing.assert_close(result.feats, expected)

    def test_dense_queries_and_sparse_keys_allow_different_value_width(self):
        queries = torch.randn(2, 3, 2, 8)
        keys = [torch.randn(length, 2, 8) for length in (2, 5)]
        values = [torch.randn(length, 2, 6) for length in (2, 5)]
        result = self.attention(queries, self.tensor.from_tensor_list(keys), self.tensor.from_tensor_list(values))
        expected = torch.stack([self.reference(q, k, v) for q, k, v in zip(queries, keys, values)])
        torch.testing.assert_close(result, expected)

    @unittest.skipUnless(importlib.util.find_spec("flex_gemm"), "compiled extensions are checked in the reconstruction image")
    def test_shape_code_and_compiled_extensions_import_with_device_inventory_fakes(self):
        # FlexGEMM selects its compiler configuration from device inventory
        # during import. These queries are the only faked boundary; kernels
        # are never executed and this test supplies no GPU performance evidence.
        with (patch.dict(os.environ, {"FLEX_GEMM_USE_AUTOTUNE_CACHE": "0"}),
              patch.object(torch.cuda, "get_device_name", return_value="NVIDIA A100"),
              patch.object(torch.cuda, "is_available", return_value=True)):
            from trellis2 import models
            from trellis2.pipelines import Trellis2ImageTo3DPipeline
            from o_voxel.convert import flexible_dual_grid_to_mesh
            from flex_gemm.ops.spconv import sparse_submanifold_conv3d
            from printable_recon.trellis import MODEL_TYPES

            for name in MODEL_TYPES.values():
                self.assertTrue(callable(getattr(models, name)))
            self.assertTrue(callable(Trellis2ImageTo3DPipeline))
            self.assertTrue(callable(flexible_dual_grid_to_mesh))
            self.assertTrue(callable(sparse_submanifold_conv3d))

    @unittest.skipUnless(importlib.util.find_spec("triton"), "Triton is supplied by the CUDA image")
    def test_runtime_can_compile_and_load_a_triton_launcher_module(self):
        from triton.runtime.build import compile_module_from_src

        # Exercise the launcher's actual compiler and Python-header path without
        # needing a GPU or substituting a prebuilt extension.
        source = r'''
#include <Python.h>
static struct PyModuleDef definition = {
    PyModuleDef_HEAD_INIT, "printable_launcher_probe", NULL, -1, NULL
};
PyMODINIT_FUNC PyInit_printable_launcher_probe(void) {
    PyObject *module = PyModule_Create(&definition);
    if (module == NULL) return NULL;
    if (PyModule_AddIntConstant(module, "answer", 42) < 0) {
        Py_DECREF(module);
        return NULL;
    }
    return module;
}
'''
        with TemporaryDirectory() as cache, patch.dict(os.environ, {"TRITON_CACHE_DIR": cache}):
            self.assertFalse(os.statvfs(cache).f_flag & os.ST_NOEXEC,
                             "the launcher test needs an executable cache, as provided by mapped runtime scratch storage")
            module = compile_module_from_src(source, "printable_launcher_probe")
            self.assertEqual(module.answer, 42)


if __name__ == "__main__":
    unittest.main()
