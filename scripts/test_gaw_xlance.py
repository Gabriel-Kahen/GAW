import importlib.util
from importlib.machinery import SourceFileLoader
from pathlib import Path
import tempfile
import unittest

try:
    import numpy as np
except ImportError:
    np = None
try:
    import soundfile as sf
except ImportError:
    sf = None


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


@unittest.skipIf(
    np is None or sf is None, "X-LANCE audio dependencies are not installed"
)
class OutputValidationTest(unittest.TestCase):
    def test_trim_restores_source_frames_channels_and_float_format(self):
        adapter = load_adapter()
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "stem.wav"
            stereo = np.column_stack(
                [np.ones(20, dtype=np.float32), np.full(20, 3, dtype=np.float32)]
            )
            sf.write(path, stereo, 44100, subtype="FLOAT")

            adapter.trim_output(path, 12, 44100, 1, sf, np)

            audio, rate = sf.read(path, always_2d=True, dtype="float32")
            self.assertEqual(rate, 44100)
            self.assertEqual(audio.shape, (12, 1))
            np.testing.assert_array_equal(audio, np.full((12, 1), 2, dtype=np.float32))
            self.assertEqual(sf.info(path).subtype, "FLOAT")


if __name__ == "__main__":
    unittest.main()
