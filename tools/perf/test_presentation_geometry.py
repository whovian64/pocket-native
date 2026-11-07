"""Worked source-derived examples, independent of the model's calculations."""
import copy
import math
import unittest

import presentation_geometry as geometry


class ModelTest(unittest.TestCase):
    def test_640_by_480_worked_layouts(self):
        # frontend/layout.rs r36s tests; roles remain top/bottom on swap.
        cases = [
            ('BigTop', 'Original', False, [60, 0, 520, 312], [208, 312, 224, 168]),
            ('BigTop', 'Original', True, [180, 312, 280, 168], [112, 0, 416, 312]),
            ('Original', 'Original', False, [120, 0, 400, 240], [160, 240, 320, 240]),
            ('TopOnly', 'Original', True, None, [0, 0, 640, 480]),
            ('SideBySide', 'Original', False, [0, 133, 356, 213], [356, 133, 284, 213]),
            ('SideBySide', 'Original', True, [284, 133, 356, 213], [0, 133, 284, 213]),
            ('Original', 'FourThree', False, [160, 0, 320, 240], [160, 240, 320, 240]),
            ('BigTop', 'FourThree', False, [112, 0, 416, 312], [208, 312, 224, 168]),
        ]
        for layout, aspect, swap, top, bottom in cases:
            with self.subTest(layout=layout, aspect=aspect, swap=swap):
                result = geometry.expected(layout, aspect, swap, [640, 480], [640, 480])
                for name, area in (('top', top), ('bottom', bottom)):
                    self.assertEqual(result['screens'][name], None if area is None else {'area': area, 'viewport': area})

    def test_fit_rounds_endpoints_in_target_pixels(self):
        # 320x240 target is half the layout. Independent endpoint rounding:
        # top x=30..290, y=0..156; bottom x=104..216, y=156..240.
        result = geometry.expected('BigTop', 'Original', False, [640, 480], [320, 240])
        self.assertEqual(result['fit'], [.5, 0., 0.])
        self.assertEqual(result['screens']['top']['viewport'], [30, 0, 260, 156])
        self.assertEqual(result['screens']['bottom']['viewport'], [104, 156, 112, 84])

    def test_observation_fails_closed(self):
        profile = {'internal_scale': 1., 'layout': 'BigTop', 'aspect': 'Original', 'swapped': False,
                   'stretched': 'Off', 'presentation_capture': {'panel_size': [640, 480]}}
        row = {'sequence': 1, 'output': 'offscreen', 'panel': [640, 480], 'target': [640, 480],
               'layout': [640, 480], 'layout_set': True, 'turned': False, 'second_window': False,
               'fit': [1., 0., 0.], 'top_crop': 0.,
               'top': {'area': [60, 0, 520, 312], 'viewport': [60, 0, 520, 312]},
               'bottom': {'area': [208, 312, 224, 168], 'viewport': [208, 312, 224, 168]}}
        self.assertEqual(geometry.validate_observation(profile, row), [])
        for field, value in [('fit', [math.nan, 0, 0]), ('sequence', 0), ('top_crop', .1),
                             ('second_window', True), ('turned', True), ('layout_set', False),
                             ('top', {'area': [60, 0, 520, 312]}),
                             ('bottom', {'area': [208, 312, 224, 168], 'viewport': [208, 311, 224, 168]})]:
            bad = copy.deepcopy(row); bad[field] = value
            with self.subTest(field=field):
                self.assertTrue(geometry.validate_observation(profile, bad))
        for change in ({'internal_scale': math.inf}, {'layout': 'BigBeside'}, {'aspect': 'Crop'}, {'swapped': 1}):
            self.assertTrue(geometry.validate_observation({**profile, **change}, row))


class IntegrationTest(unittest.TestCase):
    def test_numbered_observation_is_partial_and_identity_checked(self):
        import tempfile
        from pathlib import Path
        import render_profiles as rules
        from test_render_profiles import capture, declaration, geometry as record, presentation, write_ppm
        profile = rules.validate(declaration(capture=capture('numbered')))
        row = presentation(profile, 1, 0)
        row['top'] = {'area': [60, 0, 520, 312], 'viewport': [60, 0, 520, 312]}
        row['bottom'] = {'area': [208, 312, 224, 168], 'viewport': [208, 312, 224, 168]}
        with tempfile.TemporaryDirectory() as directory:
            write_ppm(Path(directory) / 'panel-00000.ppm', (640, 480))
            result = rules.observe(profile, geometry=record(profile, presentations=[row]), trial=directory)
            self.assertEqual(result['observed']['presentation_geometry_conformance'], 'observed_numbered_presentations_only')
            self.assertFalse(result['matrix_presentation_capability_admitted'])
            self.assertIn('unknown', result['observed']['presentation_frame_binding'])
            altered = copy.deepcopy(row); altered['top']['viewport'][0] += 1
            result = rules.observe(profile, geometry=record(profile, presentations=[row, altered]), trial=directory)
            self.assertEqual(result['observed']['presentation_geometry_conformance'], 'contradicted')
            self.assertIn('contradictory presentation identity', result['observed']['presentation_geometry_errors'])
            missing = rules.observe(profile, geometry=record(profile), trial=directory)
            self.assertEqual(missing['observed']['presentation_geometry_conformance'], 'unknown_missing_bound_evidence')


if __name__ == '__main__':
    unittest.main()
