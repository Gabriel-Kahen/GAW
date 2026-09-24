import importlib.util
from importlib.machinery import SourceFileLoader
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock
import weakref

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
    def test_chunked_identity_handles_short_and_partial_batches_without_full_padding(self):
        adapter = load_adapter()
        for frames in (0, 1, 2, 7, 8, 9, 14, 17, 100):
            for batch_size in (1, 3):
                with self.subTest(frames=frames, batch_size=batch_size):
                    audio = np.arange(frames * 2, dtype=np.float32).reshape(2, frames)
                    original = audio.copy()
                    batches = []

                    def demix(batch):
                        self.assertLessEqual(batch.shape[0], batch_size)
                        self.assertEqual(batch.shape[1:], (2, 8))
                        batches.append(batch.shape[0])
                        return batch

                    with mock.patch.object(np, "pad", side_effect=AssertionError("full padding")):
                        with mock.patch("builtins.print"):
                            result = adapter.process_audio_chunks(audio, 8, 2, batch_size, demix, np)
                    np.testing.assert_array_equal(result, original)
                    np.testing.assert_array_equal(audio, original)
                    self.assertEqual(result.dtype, np.float32)
                    self.assertTrue(batches)

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
    def test_long_input_is_inspected_without_decoding(self):
        adapter = load_adapter()
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "source.wav"
            sf.write(path, np.zeros((100, 2), dtype=np.float32), 10, subtype="FLOAT")
            with mock.patch.object(sf, "read", side_effect=AssertionError("full read")):
                result = adapter.padded_input(path, Path(directory), sf, np)
            self.assertEqual(result, (path, 100, 10, 2))

    def test_short_input_is_padded(self):
        adapter = load_adapter()
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "source.wav"
            sf.write(path, np.ones((20, 1), dtype=np.float32), 10, subtype="FLOAT")
            padded, frames, rate, channels = adapter.padded_input(
                path, Path(directory), sf, np
            )
            self.assertEqual((frames, rate, channels), (20, 10, 1))
            audio, _ = sf.read(padded, always_2d=True, dtype="float32")
            np.testing.assert_array_equal(audio[:20], np.ones((20, 1)))
            np.testing.assert_array_equal(audio[20:], np.zeros((80, 1)))

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

    def test_trim_reads_bounded_blocks_and_preserves_stereo(self):
        adapter = load_adapter()
        frames = adapter.AUDIO_BLOCK_FRAMES * 2 + 17
        audio = np.arange((frames + 4) * 2, dtype=np.float32).reshape(-1, 2)
        original_read = sf.SoundFile.read
        reads = []

        def bounded_read(source, count=-1, **kwargs):
            self.assertGreater(count, 0)
            self.assertLessEqual(count, adapter.AUDIO_BLOCK_FRAMES)
            reads.append(count)
            return original_read(source, count, **kwargs)

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "stem.wav"
            sf.write(path, audio, 44100, subtype="FLOAT")
            with mock.patch.object(sf.SoundFile, "read", bounded_read):
                adapter.trim_output(path, frames, 44100, 2, sf, np)
            actual, _ = sf.read(path, always_2d=True, dtype="float32")
            np.testing.assert_array_equal(actual, audio[:frames])
            self.assertEqual(reads, [adapter.AUDIO_BLOCK_FRAMES] * 2 + [17])

    def test_invalid_late_block_keeps_original_and_cleans_temporary_file(self):
        adapter = load_adapter()
        audio = np.zeros((adapter.AUDIO_BLOCK_FRAMES + 1, 2), dtype=np.float32)
        audio[-1, 0] = np.nan
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "stem.wav"
            sf.write(path, audio, 44100, subtype="FLOAT")
            original = path.read_bytes()
            with self.assertRaisesRegex(RuntimeError, "NaN"):
                adapter.trim_output(path, len(audio), 44100, 2, sf, np)
            self.assertEqual(path.read_bytes(), original)
            self.assertEqual(list(Path(directory).iterdir()), [path])


class ModelLifetimeTest(unittest.TestCase):
    def test_checkpoint_models_are_released_between_passes(self):
        adapter = load_adapter()
        references = []
        loaded_paths = []

        class Model:
            pass

        def load_models(paths, device):
            self.assertTrue(all(reference() is None for reference in references))
            self.assertEqual(len(paths), 1)
            model = Model()
            references.append(weakref.ref(model))
            loaded_paths.extend(paths)
            return [(None, model)]

        runtime = SimpleNamespace(
            load_audio=lambda _: (SimpleNamespace(shape=(2, 4), value=0), 44100),
            load_models=load_models,
            inference_pass=lambda config, model, audio, rate, batch_size: SimpleNamespace(
                shape=audio.shape, value=audio.value + 1
            ),
            restore_audio_length=lambda audio, samples: audio,
            calculate_rms=lambda audio: audio.value,
            save_audio=mock.Mock(),
        )
        adapter.run_stage(
            runtime, Path("in.wav"), Path("out.wav"), "cpu",
            pre=["denoise"], models=["stem1", "stem2"], post=["dereverb"],
        )
        self.assertEqual(loaded_paths, ["denoise", "stem1", "stem2", "dereverb"])
        self.assertTrue(all(reference() is None for reference in references))
        self.assertEqual(runtime.save_audio.call_args.args[0].value, 4)
        self.assertEqual(runtime.save_audio.call_args.args[1:], (44100, Path("out.wav")))

    def test_quiet_dereverb_preserves_original(self):
        adapter = load_adapter()
        runtime = SimpleNamespace(
            load_audio=lambda _: (SimpleNamespace(shape=(2, 4), value=20), 44100),
            load_models=lambda paths, device: [(None, paths[0])],
            inference_pass=lambda config, model, audio, rate, batch_size: SimpleNamespace(
                shape=audio.shape, value=audio.value - 15
            ),
            restore_audio_length=lambda audio, samples: audio,
            calculate_rms=lambda audio: audio.value,
            save_audio=mock.Mock(),
        )
        adapter.run_stage(runtime, Path("in.wav"), Path("out.wav"), "cpu", post=["post"])
        self.assertEqual(runtime.save_audio.call_args.args[0].value, 20)


@unittest.skipIf(np is None, "X-LANCE inference dependencies are not installed")
class ModelRateTest(unittest.TestCase):
    def test_mixed_rate_group_restores_length_only_after_final_model(self):
        try:
            import librosa
        except ImportError:
            self.skipTest("X-LANCE resampling dependency is not installed")
        adapter = load_adapter()
        source = np.ones((2, 4801), dtype=np.float32)
        seen = []
        rates = {"pre": 44100, "first": 44100, "second": 48000, "post": 44100}

        def process(model, audio, rate, **kwargs):
            seen.append((model, rate, audio.shape[-1]))
            return audio

        runtime = SimpleNamespace(
            librosa=librosa,
            process_long_audio=process,
            load_audio=lambda _: (source, 48000),
            load_models=lambda paths, device: [
                ({"data": {"sample_rate": rates[paths[0]]}}, paths[0])
            ],
            restore_audio_length=lambda audio, samples: adapter.restore_audio_length(
                audio, samples, np
            ),
            calculate_rms=lambda audio: 0,
            save_audio=mock.Mock(),
        )
        runtime.inference_pass = lambda *args, **kwargs: adapter.inference_pass(
            runtime, *args, **kwargs
        )
        adapter.run_stage(
            runtime, Path("in.wav"), Path("out.wav"), "cpu",
            pre=["pre"], models=["first", "second"], post=["post"],
        )
        self.assertEqual(seen, [
            ("pre", 44100, 4411),
            ("first", 44100, 4411),
            ("second", 48000, 4802),
            ("post", 44100, 4411),
        ])
        self.assertEqual(runtime.save_audio.call_args.args[0].shape, source.shape)


if __name__ == "__main__":
    unittest.main()
