"""Native smoke callers retain worker rejection bodies and processing budgets."""
import ast
import io
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
import urllib.error
import urllib.request


class NativeSmokeRequestTests(unittest.TestCase):
    def request_function(self, root):
        source = Path(__file__).resolve().parents[2] / 'slicer/smoke.py'
        tree = ast.parse(source.read_text())
        function = next(node for node in tree.body
                        if isinstance(node, ast.FunctionDef) and node.name == 'request')
        namespace = dict(json=json, time=time, threading=threading, urllib=urllib,
                         Path=Path, root=root, project=root)
        # Load only the checked-in request helper, without running native integration.
        exec(compile(ast.Module(body=[function], type_ignores=[]), str(source), 'exec'), namespace)
        return namespace['request']

    def test_expected_rejection_keeps_its_error_and_unread_body(self):
        body = {'code': 'slice', 'message': 'zoom exceeds native camera range'}
        error = urllib.error.HTTPError('http://127.0.0.1:8003/slice', 422, 'Rejected',
                                       {}, io.BytesIO(json.dumps(body).encode()))
        with tempfile.TemporaryDirectory() as directory:
            request = self.request_function(Path(directory))
            with patch('urllib.request.urlopen', side_effect=error):
                with self.assertRaises(urllib.error.HTTPError) as raised:
                    request('capture', {'camera': {'zoom': 500}})
            self.assertIs(raised.exception, error)
            self.assertEqual(json.load(raised.exception), body)

    def test_capture_deadline_allows_the_selected_processing_budget(self):
        with tempfile.TemporaryDirectory() as directory:
            request = self.request_function(Path(directory))
            for params, expected in (({}, 330), ({'timeout_seconds': 45}, 75)):
                with self.subTest(params=params), patch('urllib.request.urlopen') as opened:
                    opened.return_value.__enter__.return_value = io.BytesIO(b'{}')
                    self.assertEqual(request('capture', params), {})
                    self.assertEqual(opened.call_args.kwargs['timeout'], expected)
