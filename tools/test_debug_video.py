"""Display geometry and control framing tests; no GUI dependencies required."""
import importlib.util
from pathlib import Path
import sys
import unittest

spec = importlib.util.spec_from_file_location("debug_network_client", Path(__file__).with_name("debug-network-client.py"))
client = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = client
spec.loader.exec_module(client)


class GeometryTests(unittest.TestCase):
    def setUp(self):
        self.params = client.VideoParams(1080, 678, 30)

    def test_default_and_exact_crop(self):
        source, crop = client.source_geometry(self.params, None, None)
        self.assertEqual(source, self.params)
        self.assertIsNone(crop)
        source, crop = client.source_geometry(self.params, (1280, 720), (100, 20, 1080, 678))
        self.assertEqual((source.width, source.height), (1280, 720))
        self.assertEqual(crop, dict(left=100, top=20, width=1080, height=678))

    def test_invalid_geometry(self):
        for size, crop in [((1281, 720), None), ((1280, 720), None),
                ((1280, 720), (101, 20, 1080, 678)), ((1280, 720), (202, 20, 1080, 678)),
                ((1280, 720), (100, 44, 1080, 678)), ((1280, 720), (100, 20, 800, 480)),
                ((1280, 720), (-2, 20, 1080, 678))]:
            with self.subTest(size=size, crop=crop), self.assertRaises(ValueError):
                client.source_geometry(self.params, size, crop)


if __name__ == "__main__":
    unittest.main()
