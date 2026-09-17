"""The model's verdicts: fencing is what stops a paused holder's late completion."""

import contextlib
import io
import unittest
from unittest import mock

import model
from model import Config


class ModelTest(unittest.TestCase):
    def test_without_the_token_check_a_double_acceptance_is_reachable(self):
        result = model.check(Config(fencing=False, done_flag=False, stale_read=False))
        self.assertFalse(result.holds)
        self.assertEqual([step for step in result.trace if "ACCEPTED" in step],
                         ["peer0 completes with token 1: ACCEPTED",
                          "peer1 completes with token 2: ACCEPTED"])
        self.assertIn("peer0 pauses", result.trace)
        self.assertIn("lease expires while peer0 is paused", result.trace)

    def test_the_counterexample_is_a_real_path_through_the_model(self):
        config = Config(fencing=False, done_flag=False, stale_read=False)
        state = model.INITIAL
        for label in model.check(config).trace:
            state = dict(model.steps(state, config))[label]
        self.assertEqual(state.accepted, 2)

    def test_with_the_token_check_the_invariant_holds_everywhere(self):
        result = model.check(Config(fencing=True, done_flag=False, stale_read=False))
        self.assertTrue(result.holds)
        self.assertEqual(result.trace, [])
        self.assertGreater(result.states, 40)

    def test_the_token_check_alone_does_not_survive_a_stale_pending_read(self):
        result = model.check(Config(fencing=True, done_flag=False, stale_read=True))
        self.assertFalse(result.holds)
        self.assertNotIn("peer0 pauses", result.trace)

    def test_token_check_plus_done_flag_survives_a_stale_pending_read(self):
        self.assertTrue(model.check(Config(fencing=True, done_flag=True, stale_read=True)).holds)

    def test_the_done_flag_alone_keeps_exactly_once_but_not_freshness(self):
        """It caps acceptances at one, but lets the stale holder be the one accepted."""
        self.assertTrue(model.check(Config(fencing=False, done_flag=True, stale_read=True)).holds)

    def test_main_exits_zero_only_when_every_verdict_is_the_expected_one(self):
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(model.main(), 0)
            title, config, expected = model.CASES[0]
            with mock.patch.object(model, "CASES", ((title, config, not expected),)):
                self.assertEqual(model.main(), 1)


if __name__ == "__main__":
    unittest.main()
