"""Three new focused R52 observer timing-guard cases, authored UNRUN.

Integrate beside the exact public evaluator/support modules before CPU testing.
Reuse existing synthetic fixtures; do not discover/replay their test classes.
No ROM, native, audio, GPU or runtime configured() call is made by these cases.
"""
import copy
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location('r54_public_evaluator_fixtures', Path(__file__).with_name('test_evaluate.py'))
fixtures = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fixtures)
evaluate = fixtures.evaluate
diagnostics = evaluate.diagnostic_rules
OBSERVER = 'ZAKURO_AUDIO_PROGRESS_OBSERVER'
RUN_ID = 'ZAKURO_AUDIO_PROGRESS_RUN_ID'


def with_environment(environment):
    manifest = fixtures.fixture()
    for trial in manifest['trials']:
        trial['environment'].update(environment)
    return manifest


class AudioProgressTimingGuardCases(unittest.TestCase):
    def test_exact_runtime_activation_request_is_shared_attribution_without_profile_or_output(self):
        # configured() requires the exact string1 and1..80 ASCII alnum/_/- ID.
        # This tests exclusion of that request, not actual runtime activation.
        self.assertTrue({OBSERVER, RUN_ID} <= diagnostics.ATTRIBUTION_ENV_KEYS)
        for run_id in ('a', 'r54_run-01', 'A' * 80):
            manifest = with_environment({OBSERVER: '1', RUN_ID: run_id})
            self.assertNotIn('diagnostic_profile', manifest)
            self.assertTrue(all('audio' not in trial['probe']['report'] and 'diagnostics' not in trial
                                for trial in manifest['trials']))
            self.assertEqual(diagnostics.timing_reasons(manifest), ['diagnostic process environment'])

    def test_identical_or_one_sided_observer_requests_cannot_become_apparent_speed_wins(self):
        for affected in ('both', 'baseline', 'candidate'):
            baseline, candidate = fixtures.fixture(), fixtures.fixture()
            for name, manifest in (('baseline', baseline), ('candidate', candidate)):
                if affected in ('both', name):
                    for trial in manifest['trials']:
                        trial['environment'].update({OBSERVER: '1', RUN_ID: 'synthetic_r54'})
            for trial in candidate['trials']:
                fixtures.scale_metrics(trial, .1)
            result = evaluate.compare_values(baseline, candidate)
            self.assertEqual(result['status'], 'incomparable')
            self.assertFalse(result['target_met'])
            # Incomparability alone already followed from undeclared env before
            # this patch. Require the genuinely new explicit NON_TIMING reason.
            self.assertTrue(any(diagnostics.NON_TIMING in error and 'diagnostic process environment' in error
                                for error in result['errors']))
            self.assertTrue(any('undeclared process environment variables' in error for error in result['errors']))

    def test_partial_or_malformed_setup_is_conservatively_excluded_without_normalizing_or_claiming_audio(self):
        setups = [{OBSERVER: '1'}, {RUN_ID: 'orphan_label'}, {OBSERVER: '1', RUN_ID: ''},
                  {OBSERVER: '1', RUN_ID: 'not.allowed'}, {OBSERVER: '1', RUN_ID: 'A' * 81},
                  {OBSERVER: '1', RUN_ID: 'nonascii_é'}]
        setups += [{OBSERVER: value, RUN_ID: 'synthetic_r54'} for value in ('0', 'true', '01', '1 ', '')]
        for environment in setups:
            manifest = with_environment(environment)
            before = copy.deepcopy(manifest)
            with self.subTest(environment=environment):
                self.assertEqual(diagnostics.timing_reasons(manifest), ['diagnostic process environment'])
                self.assertEqual(manifest, before)
                self.assertNotIn('evidence_class', manifest)
                self.assertTrue(all('diagnostics' not in trial and 'audio' not in trial['probe']['report']
                                    for trial in manifest['trials']))
        # No new diagnostic-profile schema or acceptance route is introduced.
        with self.assertRaises(ValueError):
            diagnostics.validate({'schema_version': 1, 'audio_progress_observer': True})


if __name__ == '__main__':
    unittest.main()
