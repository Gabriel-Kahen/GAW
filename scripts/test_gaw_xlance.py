import importlib.util
from importlib.machinery import SourceFileLoader
from pathlib import Path
import unittest

try:
    import numpy as np
except ImportError:
    np = None


def load_adapter():
    path = Path(__file__).with_name("gaw-xlance")
    spec = importlib.util.spec_from_loader(
        "gaw_xlance", SourceFileLoader("gaw_xlance", str(path))
    )
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load gaw-xlance adapter")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@unittest.skipIf(np is None, "X-LANCE inference dependencies are not installed")
class MergeChunksTest(unittest.TestCase):
    def test_identity_chunks_preserve_song_edges_and_dtype(self):
        adapter = load_adapter()
        chunks = [np.ones((2, 8), dtype=np.float32) for _ in range(3)]

        merged = adapter.merge_chunks_without_edge_fades(chunks, 8, 2, np)

        np.testing.assert_array_equal(merged, np.ones((2, 20), dtype=np.float32))
        self.assertEqual(merged.dtype, np.float32)


if __name__ == "__main__":
    unittest.main()
