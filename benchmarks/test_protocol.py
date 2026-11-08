"""Regression checks for false endpoints and misleading benchmark reports."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from common import finish_report, new_report, output_directory, qemu_identity, save_report

ROOT = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location(
    "langsel_endpoint", ROOT / "reactos_livecd_boot_to_langsel/endpoint.py")
endpoint = importlib.util.module_from_spec(spec)
spec.loader.exec_module(endpoint)


class EndpointTests(unittest.TestCase):
    def frame(self, path, omitted=()):
        rgb = bytearray(800 * 600 * 3)
        for x, y, r, g, b in endpoint.points():
            if (x, y) not in omitted:
                index = (y * 800 + x) * 3
                rgb[index:index + 3] = bytes((r, g, b))
        path.write_bytes(b"P6\n800 600\n255\n" + rgb)

    def test_next_before_cancel_is_not_the_endpoint(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "frame.ppm"
            for missing in (((625, 456),), ((604, 448),)):
                self.frame(path, missing)
                self.assertFalse(endpoint.matching_frame(path))
            self.frame(path)
            self.assertTrue(endpoint.matching_frame(path))

    def test_truncated_capture_is_an_error(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "frame.ppm"
            self.frame(path)
            path.write_bytes(path.read_bytes()[:-1])
            with self.assertRaisesRegex(RuntimeError, "incomplete PPM"):
                endpoint.matching_frame(path)

    def test_headless_capture_requires_the_complete_viewport(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "frame.ppm"
            self.frame(path)
            rgb = path.read_bytes().split(b"255\n", 1)[1][:800 * 584 * 3]
            path.write_bytes(b"P6\n800 584\n255\n" + rgb)
            self.assertFalse(endpoint.matching_frame(path))

class QemuIdentityTests(unittest.TestCase):
    def test_interpreter_and_native_results_cannot_be_mislabeled(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "qemu-system-i386"
            config = binary.parent / "config-host.h"
            base = "#define CONFIG_TCG 1\n#define HOST_AARCH64 1\n"
            variants = {
                "native": "#undef CONFIG_TCG_INTERPRETER\n#undef CONFIG_TCG_THREADED_INTERPRETER\n",
                "tcti": "#undef CONFIG_TCG_INTERPRETER\n#define CONFIG_TCG_THREADED_INTERPRETER\n",
                "tci": "#define CONFIG_TCG_INTERPRETER\n#undef CONFIG_TCG_THREADED_INTERPRETER\n",
            }
            for actual, defines in variants.items():
                config.write_text(base + defines)
                for requested in ("native", "tcti"):
                    with self.subTest(actual=actual, requested=requested):
                        with patch("common.subprocess.check_output", return_value="QEMU test\n") as version:
                            if actual == requested:
                                self.assertEqual(qemu_identity(binary, requested)["qemu_backend"], requested)
                                version.assert_called_once()
                            else:
                                with self.assertRaises(ValueError):
                                    qemu_identity(binary, requested)
                                version.assert_not_called()


class ReportTests(unittest.TestCase):
    def test_warmups_are_preserved_but_excluded_from_statistics(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            report = new_report("tinycore_boot", "firmware-r86", {}, cpu=0,
                                warmups=1, runs=2, guest_clocks={"r86": "realtime"})
            for index, seconds in enumerate((99, 1, 3)):
                report["runs"].append({"engine": "r86", "round": index,
                                       "warmup": index == 0, "elapsed_seconds": seconds,
                                       "cpu_seconds": seconds / 2})
            finish_report(report, output)
            saved = json.loads((output / "results.json").read_text())
            self.assertTrue(saved["completed"])
            self.assertTrue(saved["full_run_count"])
            self.assertEqual(len(saved["runs"]), 3)
            self.assertEqual(saved["medians"]["r86"]["elapsed_seconds"], 2)
            self.assertEqual(saved["ranges"]["r86"]["elapsed_seconds"], [1, 3])
            self.assertIsNone(saved["baseline"])
            self.assertNotIn("ratios", saved)

    def test_incomplete_run_is_never_reported_as_complete(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            report = new_report("tinycore_boot", "firmware-r86", {}, cpu=0,
                                warmups=0, runs=2, guest_clocks={"r86": "realtime"})
            report["runs"] = [{"engine": "r86", "round": 0, "warmup": False,
                               "elapsed_seconds": 1, "cpu_seconds": 1}]
            save_report(report, output)
            with self.assertRaisesRegex(ValueError, "incomplete"):
                finish_report(report, output)
            self.assertFalse(json.loads((output / "results.json").read_text())["completed"])

    def test_existing_results_are_not_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            (output / "results.json").write_text("preserved")
            with self.assertRaisesRegex(ValueError, "must be empty"):
                output_directory(output)
            self.assertEqual((output / "results.json").read_text(), "preserved")

    def test_legacy_mixed_clocks_are_visible_in_the_report(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            report = new_report("tinycore_boot", "firmware-tiny386", {}, cpu=0,
                                warmups=0, runs=2, baseline="tiny386",
                                guest_clocks={"r86": "deterministic", "tiny386": "realtime"})
            for index in range(2):
                for engine in ("r86", "tiny386"):
                    report["runs"].append({"engine": engine, "warmup": False,
                                           "round": index, "elapsed_seconds": 1,
                                           "cpu_seconds": 1})
            finish_report(report, output)
            saved = json.loads((output / "results.json").read_text())
            rendered = (output / "results.md").read_text()
            self.assertEqual(saved["schema_version"], 2)
            self.assertTrue(saved["full_run_count"])
            self.assertFalse(saved["clock_policies_match"])
            self.assertIn("| r86 | deterministic |", rendered)
            self.assertIn("| tiny386 | realtime |", rendered)
            self.assertIn("Guest clock policies differ", rendered)


if __name__ == "__main__":
    unittest.main()
