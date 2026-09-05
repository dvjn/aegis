"""Executable reporting-contract fixtures: python3 -m unittest discover -s scripts."""

import unittest

from analytics_reference import Appearance as A, Request as R, report


class ReportingContract(unittest.TestCase):
    def test_inclusive_boundaries_and_replayed_calls_across_hours(self):
        requests = [R(str(t), "u", "p", t, 100, 100) for t in (0, 3599999, 3600000, 7200000)]
        parts = [A(r.id, "call", "c", "h", 10, "Read") for r in requests]
        tools, _ = report(requests, parts, "u", 3599999, 3600000)
        self.assertEqual(tools["Read"].integers(), (1, 0, 10, 20))
        tools, _ = report(requests, parts, "u", 3600001, 7199999)
        self.assertEqual(tools, {})

    def test_window_maximum_is_not_lifetime_maximum(self):
        requests = [R("old", "u", "p", 0, 100, 100), R("new", "u", "p", 10, 100, 100)]
        parts = [A("old", "call", "c", "old", 90, "Read"), A("new", "call", "c", "new", 10, "Read")]
        tools, _ = report(requests, parts, "u", 10, 10)
        self.assertEqual(tools["Read"].integers(), (1, 0, 10, 10))

    def test_fractional_cost_is_rounded_after_summing(self):
        requests = [R(str(t), "u", "p", t * 3600000, 3, 1) for t in range(3)]
        parts = [A(r.id, "call", str(i), str(i), 1, "Read") for i, r in enumerate(requests)]
        tools, _ = report(requests, parts, "u", 0, 7200000)
        self.assertEqual(tools["Read"].integers(), (3, 0, 3, 1))
        self.assertEqual(sum(int(report(requests, parts, "u", r.started, r.started)[0]["Read"].cost) for r in requests), 0)

    def test_definitions_split_container_and_count_every_transmission(self):
        requests = [R("r", "u", "p", 0, 9, 3)]
        parts = [A("r", "definition", None, "container", 9, name, multiplicity=2, definitions_in_container=3) for name in ("Read", "Write", "Bash")]
        tools, _ = report(requests, parts, "u", 0, 0)
        self.assertEqual(tools["Read"].integers(), (0, 2, 6, 2))

    def test_results_before_calls_and_outside_window_attribution(self):
        requests = [R("r", "u", "p", 0, 100, 100), R("c", "u", "p", 10, 100, 100)]
        parts = [A("r", "result", "c", "result", 20), A("c", "call", "c", "call", 10, "Skill", "review")]
        tools, skills = report(requests, parts, "u", 0, 0)
        self.assertEqual(tools["Skill"].integers(), (0, 0, 20, 20))
        self.assertEqual(skills["review"].integers(), (0, 0, 20, 20))

    def test_owner_provider_and_conversation_scope(self):
        for owner, provider, conversation in (("other", "p", None), ("u", "other", None), ("u", "p", "other")):
            with self.subTest(scope=(owner, provider, conversation)):
                requests = [R("r", "u", "p", 0, 100, 100), R("c", owner, provider, 10, 100, 100, conversation)]
                parts = [A("r", "result", "c", "result", 20), A("c", "call", "c", "call", 10, "PrivateTool")]
                tools, _ = report(requests, parts, "u", 0, 0)
                self.assertEqual(tools[None].integers(), (0, 0, 20, 20))
                self.assertNotIn("PrivateTool", tools)

    def test_conflicting_fallback_identity_stays_unresolved(self):
        requests = [R("r", "u", "p", 0, 100, 100)]
        parts = [A("r", "call", "c", "a", 10, "Read"), A("r", "call", "c", "b", 20, "Bash"), A("r", "result", "c", "c", 30)]
        tools, _ = report(requests, parts, "u", 0, 0)
        self.assertEqual(tools[None].integers(), (1, 0, 50, 60))

    def test_missing_call_ids_use_tagged_content_hashes(self):
        requests = [R("r", "u", "p", 0, 100, 100)]
        parts = [A("r", "call", None, "same", 10, "Read", multiplicity=2), A("r", "call", "same", "different", 10, "Read")]
        tools, _ = report(requests, parts, "u", 0, 0)
        self.assertEqual(tools["Read"].integers(), (2, 0, 20, 30))

    def test_unknown_price_is_not_zero_price(self):
        requests = [R("unknown", "u", "p", 0, 1, None), R("free", "u", "p", 1, 1, 0)]
        parts = [A(r.id, "call", r.id, r.id, 1, "Read") for r in requests]
        tools, _ = report(requests, parts, "u", 0, 0)
        self.assertEqual(tools["Read"].unpriced_appearances, 1)
        tools, _ = report(requests, parts, "u", 1, 1)
        self.assertEqual(tools["Read"].unpriced_appearances, 0)
        self.assertEqual(tools["Read"].integers()[-1], 0)

    def test_large_product_and_zero_denominator(self):
        largest = 2**63 - 1
        requests = [R("large", "u", "p", 0, largest, largest), R("zero", "u", "p", 1, 0, 1)]
        parts = [A("large", "call", "large", "large", largest, "Read"), A("zero", "call", "zero", "zero", 0, "Read")]
        tools, _ = report(requests, parts, "u", 0, 1)
        self.assertEqual(tools["Read"].integers(), (2, 0, largest, largest))


if __name__ == "__main__":
    unittest.main()
