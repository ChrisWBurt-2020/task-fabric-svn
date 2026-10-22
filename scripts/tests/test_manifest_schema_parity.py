"""Tests for the manifest schema parity checker.

The checker exists because the proxy's private copy of the manifest key
set drifted from containment's and wedged all dispatch (fixed by hand in
c1c08ef). Each test here proves a drift class is CAUGHT and named: the
incident replay (twice -- synthetic, and against the real sources with
the c1c08ef tolerance removed), contract-set drift, required-set drift,
producer drift in both directions, and the fail-closed paths (a
declaration that cannot be extracted, or a validator copy the checker
does not know about, must never read as parity).
"""
import contextlib
import io
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import check_manifest_schema_parity as parity

SCRIPTS_DIR = Path(__file__).resolve().parents[1]
CONTRACT_KEYS = ('"task_id", "trace_id", "model", "consumer", '
                 '"destination", "max_request_bytes", "max_requests"')
TOP_KEYS = '"schema", "harness_executable", "proxy_socket", "contract"'

CONTAINMENT_SRC = f'''
def load_config(path, task_id, trace_id, model):
    config = {{}}
    required = {{{TOP_KEYS}}}
    optional = {{"codemode_executable"}}
    if not required <= set(config) <= required | optional \\
            or config["schema"] != 1:
        raise ValueError("invalid config")
    contract = config["contract"]
    if set(contract) != {{{CONTRACT_KEYS}}}:
        raise ValueError("invalid proxy contract")
'''

PROXY_SRC = f'''
def load_proxy_config(path):
    config = {{}}
    required = {{{TOP_KEYS}}}
    if not required <= set(config) <= required | {{"codemode_executable"}} \\
            or config["schema"] != 1:
        raise ValueError("containment_config_invalid")
    contract = config["contract"]
    if set(contract) != {{{CONTRACT_KEYS}}}:
        raise ValueError("containment_config_invalid")
'''

# The pre-c1c08ef proxy: no codemode_executable tolerance.
PROXY_SRC_PREFIX = PROXY_SRC.replace(
    'required | {"codemode_executable"}', 'required')

PRODUCER_SRC = f'''
def build_manifest(task_id, trace_id, model, codemode_executable=None):
    document = {{
        "schema": 1,
        "harness_executable": "/bin/true",
        "proxy_socket": "/tmp/x.socket",
        "contract": {{
            "task_id": task_id,
            "trace_id": trace_id,
            "model": model,
            "consumer": "c",
            "destination": "http://127.0.0.1:20128/v1/chat/completions",
            "max_request_bytes": 1,
            "max_requests": 1,
        }},
    }}
    if codemode_executable:
        document["codemode_executable"] = codemode_executable
    return document
'''


def run_main(scripts_dir):
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        code = parity.main(["--scripts-dir", str(scripts_dir)])
    return code, buf.getvalue()


class ParityFixtureTest(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp(prefix="parity-fixture-"))
        self.addCleanup(shutil.rmtree, self.dir, True)

    def write_trio(self, containment=CONTAINMENT_SRC, proxy=PROXY_SRC,
                   producer=PRODUCER_SRC):
        (self.dir / "task_containment.py").write_text(containment)
        (self.dir / "task_inference_proxy.py").write_text(proxy)
        (self.dir / "task_proxy_manifest.py").write_text(producer)

    def test_in_sync_fixtures_pass(self):
        self.write_trio()
        code, out = run_main(self.dir)
        self.assertEqual(code, 0, out)
        self.assertIn("parity verified", out)

    def test_incident_replay_proxy_missing_optional_key(self):
        """The c1c08ef wedge: producer emits codemode_executable,
        containment allows it, the proxy's copy does not."""
        self.write_trio(proxy=PROXY_SRC_PREFIX)
        code, out = run_main(self.dir)
        self.assertEqual(code, 1, out)
        self.assertIn("DIVERGE", out)
        self.assertIn("codemode_executable", out)
        self.assertIn("proxy rejects it", out)
        self.assertIn("NOT verified", out)

    def test_contract_key_drift_is_named(self):
        drifted = CONTAINMENT_SRC.replace(
            '"max_requests"}', '"max_requests", "budget"}')
        self.assertNotEqual(drifted, CONTAINMENT_SRC)
        self.write_trio(containment=drifted)
        code, out = run_main(self.dir)
        self.assertEqual(code, 1, out)
        self.assertIn("contract key 'budget'", out)

    def test_required_key_drift_is_named(self):
        drifted = PROXY_SRC.replace(
            '"contract"}', '"contract", "telemetry"}', 1)
        self.assertNotEqual(drifted, PROXY_SRC)
        self.write_trio(proxy=drifted)
        code, out = run_main(self.dir)
        self.assertEqual(code, 1, out)
        self.assertIn("'telemetry'", out)
        self.assertIn("requires it", out)

    def test_producer_emitting_unallowed_key_is_named(self):
        drifted = PRODUCER_SRC.replace(
            '"proxy_socket": "/tmp/x.socket",',
            '"proxy_socket": "/tmp/x.socket",\n        "telemetry": "/x",')
        self.assertNotEqual(drifted, PRODUCER_SRC)
        self.write_trio(producer=drifted)
        code, out = run_main(self.dir)
        self.assertEqual(code, 1, out)
        self.assertIn("'telemetry'", out)
        self.assertIn("producer emits it", out)

    def test_producer_missing_required_key_is_named(self):
        drifted = PRODUCER_SRC.replace(
            '        "proxy_socket": "/tmp/x.socket",\n', '')
        self.assertNotEqual(drifted, PRODUCER_SRC)
        self.write_trio(producer=drifted)
        code, out = run_main(self.dir)
        self.assertEqual(code, 1, out)
        self.assertIn("'proxy_socket'", out)
        self.assertIn("never emits it", out)

    def test_dynamic_declaration_is_unresolved_not_a_pass(self):
        dynamic = CONTAINMENT_SRC.replace(
            f'required = {{{TOP_KEYS}}}',
            'required = BASE_KEYS | {"schema"}')
        self.assertNotEqual(dynamic, CONTAINMENT_SRC)
        self.write_trio(containment=dynamic)
        code, out = run_main(self.dir)
        self.assertEqual(code, 2, out)
        self.assertIn("UNRESOLVED", out)
        self.assertIn("NOT verified", out)

    def test_missing_files_are_unresolved_not_a_pass(self):
        code, out = run_main(self.dir)
        self.assertEqual(code, 2, out)
        self.assertIn("UNRESOLVED", out)

    def test_unregistered_validator_copy_is_flagged(self):
        self.write_trio()
        (self.dir / "task_extra_validator.py").write_text(
            'def check(config):\n'
            '    contract = config["contract"]\n'
            '    if set(contract) != {"task_id"}:\n'
            '        raise ValueError("nope")\n')
        code, out = run_main(self.dir)
        self.assertEqual(code, 2, out)
        self.assertIn("task_extra_validator.py", out)
        self.assertIn("unregistered", out)


class RealTreeParityTest(unittest.TestCase):
    def test_live_sources_are_in_parity(self):
        """The shipped containment/proxy/producer declarations agree
        today; this test is the standing guard that they keep agreeing."""
        code, out = run_main(SCRIPTS_DIR)
        self.assertEqual(code, 0, out)
        self.assertIn("codemode_executable", out)

    def test_incident_replay_against_real_sources(self):
        """Copy the real sources, remove the proxy's c1c08ef tolerance,
        and the checker must catch the original wedge by name."""
        tmp = Path(tempfile.mkdtemp(prefix="parity-incident-"))
        self.addCleanup(shutil.rmtree, tmp, True)
        for fname in ("task_containment.py", "task_inference_proxy.py",
                      "task_proxy_manifest.py"):
            shutil.copy(SCRIPTS_DIR / fname, tmp / fname)
        proxy_path = tmp / "task_inference_proxy.py"
        source = proxy_path.read_text()
        needle = 'required | {"codemode_executable"}'
        self.assertIn(needle, source,
                      "proxy source shape changed; update the replay")
        proxy_path.write_text(source.replace(needle, "required"))
        code, out = run_main(tmp)
        self.assertEqual(code, 1, out)
        self.assertIn("codemode_executable", out)
        self.assertIn("proxy rejects it", out)


if __name__ == "__main__":
    unittest.main()
