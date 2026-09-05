"""The soak runs as 60 s segments so oha's per-request memory stays bounded;
cell.py must fold the segments into one honest cell."""
import unittest

import cell


def oha(rps, total_secs, p99, status=None, errors=None):
    n = int(rps * total_secs)
    return {"summary": {"requestsPerSec": rps, "total": total_secs},
            "latencyPercentiles": {"p50": p99 / 3, "p90": p99 / 1.5, "p99": p99},
            "statusCodeDistribution": status or {"200": n},
            "errorDistribution": errors or {}}


class Aggregate(unittest.TestCase):
    def test_one_result_is_itself(self):
        a = cell.aggregate([oha(1000.0, 10.0, 0.006)])
        self.assertAlmostEqual(a["rps"], 1000.0)
        self.assertAlmostEqual(a["p99_ms"], 6.0)
        self.assertEqual(a["requests"], 10000)
        self.assertEqual(a["errors"], 0)
        self.assertEqual(a["segments"], [1000.0])

    def test_segments_sum_requests_weight_rps_and_keep_the_worst_tail(self):
        a = cell.aggregate([oha(1000.0, 10.0, 0.005), oha(500.0, 10.0, 0.020)])
        self.assertEqual(a["requests"], 15000)
        self.assertAlmostEqual(a["rps"], 750.0)
        self.assertAlmostEqual(a["p99_ms"], 20.0)
        self.assertEqual(a["segments"], [1000.0, 500.0])

    def test_errors_count_non_2xx_and_real_failures_but_not_the_deadline_cutoff(self):
        a = cell.aggregate([oha(1000.0, 1.0, 0.005, status={"200": 990, "500": 10},
                                errors={"aborted due to deadline": 7, "connection refused": 3})])
        self.assertEqual(a["errors"], 13)


if __name__ == "__main__":
    unittest.main()
