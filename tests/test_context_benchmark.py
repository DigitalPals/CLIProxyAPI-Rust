"""No provider calls: parser tests and an ephemeral loopback mock server."""
import contextlib
import http.server
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location("context_benchmark", Path(__file__).resolve().parents[1] / "scripts/context-benchmark.py")
bench = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench)


def sse(kind, **values):
    return ("data: " + json.dumps({"type": kind, **values}) + "\n\n").encode()


def completed(**values):
    return {"status": "completed", "model": "synthetic", **values}


@contextlib.contextmanager
def server(callback):
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            self.request_body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            try:
                callback(self)
            except (BrokenPipeError, ConnectionResetError):
                pass

        def log_message(self, *_):
            pass

    instance = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=instance.serve_forever, kwargs={"poll_interval": .01}, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{instance.server_port}/v1/responses"
    finally:
        instance.shutdown()
        instance.server_close()
        thread.join(timeout=2)


def headers(handler, content_type="text/event-stream", status=200):
    handler.send_response(status)
    handler.send_header("Content-Type", content_type)
    handler.end_headers()


class MetricsTests(unittest.TestCase):
    def test_session_fingerprint_matches_rust_affinity_and_journal_vectors(self):
        # Fixed vectors generated with the exact digest() extracted from
        # src/affinity.rs, compiled with Rust sha2/hex, then diagnostics' SHA32.
        self.assertEqual(bench.session_journal_fingerprint("synthetic-key", "context-benchmark-fixture"),
                         "5ec3d2c1888154267e20e0b86d96b0a4")
        self.assertEqual(bench.session_journal_fingerprint(" synthetic-key ", " context-benchmark-fixture "),
                         "5ec3d2c1888154267e20e0b86d96b0a4")
        for key in ("", "   "):
            self.assertEqual(bench.session_journal_fingerprint(key, "context-benchmark-fixture"),
                             "95b6bfc15931d3c1b3acbb1c9a2892a1")
        self.assertEqual(bench.session_journal_fingerprint("synthetic-key", "context-benchmark-é"),
                         "9fdffb4aeddb16402b8de75c88f5b2d3")

    def test_provider_usage_zero_is_not_missing(self):
        usage, warnings = bench.usage_fields(completed(usage={"input_tokens": 100, "output_tokens": 0,
            "input_tokens_details": {"cached_tokens": 0, "cache_creation_tokens": 8}}))
        self.assertEqual((usage["input_tokens"], usage["output_tokens"], usage["cached_input_tokens"]), (100, 0, 0))
        self.assertEqual(usage["cache_write_input_tokens"], 8)
        self.assertIsNone(usage["total_tokens"])
        self.assertEqual(warnings, [])
        absent, warnings = bench.usage_fields(completed())
        self.assertTrue(all(value is None for value in absent.values()))
        self.assertEqual(warnings, ["usage_missing"])

    def test_bad_usage_is_unknown_and_warned(self):
        usage, warnings = bench.usage_fields(completed(usage={"input_tokens": True, "output_tokens": -1,
            "input_tokens_details": {"cached_tokens": "5"}}))
        self.assertIsNone(usage["input_tokens"])
        self.assertIsNone(usage["output_tokens"])
        self.assertIsNone(usage["cached_input_tokens"])
        self.assertIn("invalid_input_tokens", warnings)

    def test_inconsistent_cache_count_is_unknown_and_excluded_from_comparison(self):
        usage, warnings = bench.usage_fields(completed(usage={"input_tokens": 10, "output_tokens": 2,
            "input_tokens_details": {"cached_tokens": 11}}))
        self.assertEqual(usage["input_tokens"], 10)
        self.assertIsNone(usage["cached_input_tokens"])
        self.assertIn("cached_exceeds_input_tokens", warnings)
        rows = [{"pair": 1, "variant": "original", "status": "completed", "usage": {"cached_input_tokens": 5}},
                {"pair": 1, "variant": "compacted", "status": "completed", "usage": usage}]
        comparison = bench.summarize(rows, 1)["paired_compacted_minus_original"]["cached_input_tokens"]
        self.assertEqual(comparison["observations"], 0)
        self.assertIsNone(comparison["median"])
        self.assertFalse(comparison["all_planned_pairs_observed"])

    def test_multiline_crlf_and_fragmented_utf8(self):
        metrics = bench.ResponseMetrics()
        parser = bench.SSEParser(metrics)
        wire = (': heartbeat\r\n\r\nevent: response.output_text.delta\r\n'
                'data: {"delta":\r\ndata: "héllo"}\r\n\r\n').encode()
        for byte in wire:
            parser.feed(bytes([byte]), 12)
        self.assertEqual(metrics.ttft_ms, 12)
        parser.feed(sse("response.completed", response=completed(usage={"input_tokens": 6, "output_tokens": 2})), 30)
        self.assertEqual(metrics.status, "completed")
        self.assertEqual(metrics.usage["input_tokens"], 6)

    def test_tool_delta_has_no_text_ttft(self):
        metrics = bench.ResponseMetrics()
        metrics.event("", {"type": "response.function_call_arguments.delta", "delta": "{}"}, 5)
        self.assertEqual(metrics.first_output_delta_ms, 5)
        self.assertIsNone(metrics.ttft_ms)

    def test_failed_incomplete_cancelled_and_error(self):
        for state in ("incomplete", "failed", "cancelled"):
            metrics = bench.ResponseMetrics()
            metrics.event("", {"type": "response." + state, "response": {"status": state,
                "usage": {"input_tokens": 20, "output_tokens": 3}}}, 10)
            self.assertEqual(metrics.status, state)
            self.assertEqual(metrics.usage["input_tokens"], 20)
        metrics = bench.ResponseMetrics()
        metrics.event("", {"type": "error", "message": "PRIVATE"}, 1)
        self.assertEqual(metrics.status, "provider_error")
        self.assertNotIn("PRIVATE", repr(vars(metrics)))

    def test_done_and_unterminated_frame_do_not_imply_completion(self):
        for wire in (b"data: [DONE]\n\n", sse("response.completed", response=completed()).rstrip(b"\n")):
            metrics = bench.ResponseMetrics()
            bench.SSEParser(metrics).feed(wire, 1)
            self.assertIsNone(metrics.status)

    def test_unexpected_types_and_contradictory_terminal(self):
        for event in ({"type": []}, {"type": "response.completed", "response": {"status": "failed"}}):
            with self.assertRaises(bench.BenchmarkError):
                bench.ResponseMetrics().event("", event, 0)

    def test_pairs_exclude_failures_and_missing_metrics(self):
        def row(pair, variant, status, tokens):
            return {"pair": pair, "variant": variant, "status": status, "latency_ms": 10,
                    "usage": {"input_tokens": tokens}}
        rows = [row(1, "original", "completed", 100), row(1, "compacted", "completed", 20),
                row(2, "original", "completed", 100), row(2, "compacted", "incomplete", 1),
                row(3, "original", "completed", 100), row(3, "compacted", "completed", None)]
        result = bench.summarize(rows, 3)
        delta = result["paired_compacted_minus_original"]["input_tokens"]
        self.assertEqual(delta["median"], -80)
        self.assertEqual(delta["observations"], 1)
        self.assertFalse(delta["all_planned_pairs_observed"])


class HTTPTests(unittest.TestCase):
    def call(self, callback, **options):
        with server(callback) as endpoint:
            return bench.request_trial(endpoint, b'{"input":"PRIVATE"}', "synthetic-secret", options.get("timeout", 1),
                                       options.get("limit", 4096))

    def test_sse_measurements_do_not_count_response_created_as_ttft(self):
        def callback(handler):
            self.assertEqual(handler.headers.get("Authorization"), "Bearer synthetic-secret")
            headers(handler)
            handler.wfile.write(sse("response.created", response={"status": "in_progress"}))
            handler.wfile.flush()
            time.sleep(.035)
            handler.wfile.write(sse("response.output_text.delta", delta="PRIVATE-GENERATION"))
            handler.wfile.flush()
            time.sleep(.02)
            handler.wfile.write(sse("response.completed", response=completed(usage={"input_tokens": 30,
                "input_tokens_details": {"cached_tokens": 12}, "output_tokens": 4})))
        result = self.call(callback)
        self.assertEqual(result["status"], "completed")
        self.assertGreaterEqual(result["ttft_ms"], 30)
        self.assertGreater(result["latency_ms"], result["ttft_ms"])
        self.assertEqual(result["usage"]["cached_input_tokens"], 12)
        self.assertNotIn("PRIVATE", json.dumps(result))
        self.assertNotIn("synthetic-secret", json.dumps(result))

    def test_json_completion_has_no_fabricated_ttft_or_usage(self):
        def callback(handler):
            headers(handler, "application/json")
            handler.wfile.write(json.dumps(completed(output=[])).encode())
        result = self.call(callback)
        self.assertEqual(result["status"], "completed")
        self.assertIsNone(result["ttft_ms"])
        self.assertIsNone(result["usage"]["input_tokens"])

    def test_truncated_stream_and_invalid_json_are_failures(self):
        for wire, expected in ((sse("response.output_text.delta", delta="PRIVATE"), "missing_terminal"),
                               (b"data: not json\n\n", "invalid_event_json")):
            def callback(handler):
                headers(handler)
                handler.wfile.write(wire)
            self.assertEqual(self.call(callback)["status"], expected)

    def test_truncated_http_json_body_is_not_complete(self):
        def callback(handler):
            handler.send_response(200)
            handler.send_header("Content-Type", "application/json")
            handler.send_header("Content-Length", "1000")
            handler.end_headers()
            handler.wfile.write(json.dumps(completed()).encode())
        self.assertEqual(self.call(callback)["status"], "truncated_http_body")

    def test_redirect_is_not_followed_and_body_is_not_logged(self):
        calls = []
        def callback(handler):
            calls.append(handler.path)
            handler.send_response(302)
            handler.send_header("Location", "/v1/responses")
            handler.end_headers()
            handler.wfile.write(b"PRIVATE ERROR DETAILS")
        result = self.call(callback)
        self.assertEqual(calls, ["/v1/responses"])
        self.assertEqual((result["status"], result["http_status"]), ("http_error", 302))
        self.assertNotIn("PRIVATE", json.dumps(result))

    def test_request_deadline_applies_to_trickling_stream(self):
        def callback(handler):
            headers(handler)
            for _ in range(100):
                handler.wfile.write(b": heartbeat\n\n")
                handler.wfile.flush()
                time.sleep(.02)
        result = self.call(callback, timeout=.12)
        self.assertEqual(result["status"], "timeout")
        self.assertLess(result["latency_ms"], 500)

    def test_body_bound(self):
        def callback(handler):
            headers(handler)
            handler.wfile.write(b"data: " + b"x" * 1000)
        self.assertEqual(self.call(callback, limit=100)["status"], "response_body_limit")

    def test_cancellation_is_recorded(self):
        with mock.patch.object(bench.http.client.HTTPConnection, "request", side_effect=KeyboardInterrupt):
            row = bench.request_trial("http://127.0.0.1/v1/responses", b"{}", "", 1, 100)
        self.assertEqual(row["status"], "cancelled_by_user")
        self.assertIsNone(row["usage"]["input_tokens"])


class CLITests(unittest.TestCase):
    def invoke(self, original, compacted, extra=()):
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory) / name for name in ("original.json", "compacted.json")]
            for path, value in zip(paths, (original, compacted)):
                path.write_text(json.dumps(value))
            out, err = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = bench.main(["--original", str(paths[0]), "--compacted", str(paths[1]), *extra])
            return code, out.getvalue(), err.getvalue()

    def test_default_dry_run_has_no_network_or_credential_read(self):
        original = {"model": "synthetic", "input": "PRIVATE-LONG" * 100, "stream": True}
        compacted = {**original, "input": "PRIVATE-SHORT"}
        with mock.patch.object(bench, "request_trial", side_effect=AssertionError("network forbidden")), \
                mock.patch.object(bench.getpass, "getpass", side_effect=AssertionError("prompt forbidden")):
            code, out, err = self.invoke(original, compacted)
        report = json.loads(out)
        self.assertEqual((code, err, report["mode"]), (0, "", "dry_run"))
        self.assertGreater(report["offline_context_bytes"]["reduction_fraction"], .9)
        self.assertIsNone(report["offline_context_bytes"]["token_savings"])
        self.assertNotIn("PRIVATE", out)

    def test_mismatched_settings_and_stateful_requests_rejected(self):
        original = {"model": "synthetic", "input": "one", "stream": True}
        for other, expected in (({**original, "model": "different"}, "non_context_settings_differ"),
                                ({**original, "previous_response_id": "PRIVATE"}, "stateful_requests_not_supported")):
            code, out, err = self.invoke(original, other)
            self.assertEqual((code, out, json.loads(err)["error"]), (2, "", expected))

    def test_hosted_tools_rejected_but_function_definitions_preserved(self):
        original = {"model": "synthetic", "input": "one", "tools": [{"type": "web_search"}]}
        code, _, err = self.invoke(original, original)
        self.assertEqual((code, json.loads(err)["error"]), (2, "only_function_tool_definitions_supported"))
        original["tools"] = [{"type": "function", "name": "example", "parameters": {"type": "object"}}]
        self.assertEqual(self.invoke(original, original)[0], 0)

    def test_auth_failure_stops_remaining_trials(self):
        original = {"model": "synthetic", "input": "one"}
        with mock.patch.dict(os.environ, {"FUSEBOX_API_KEY": "synthetic-secret"}), \
                mock.patch.object(bench, "request_trial", return_value={"http_status": 401,
                    "status": "http_error", "usage": {}}) as call:
            code, out, _ = self.invoke(original, original, ["--execute"])
        self.assertEqual((code, call.call_count), (1, 1))
        self.assertEqual(json.loads(out)["summary"]["variants"]["compacted"]["attempted"], 0)

    def test_execute_orders_pairs_and_stops_on_cancellation(self):
        original = {"model": "synthetic", "input": "one"}
        completed_row = {"http_status": 200, "status": "completed", "usage": {"input_tokens": 0}}
        with mock.patch.dict(os.environ, {"FUSEBOX_API_KEY": "synthetic-secret"}), \
                mock.patch.object(bench, "request_trial", side_effect=[dict(completed_row) for _ in range(6)]):
            code, out, _ = self.invoke(original, {**original, "input": "two"}, ["--execute"])
        self.assertEqual(code, 0)
        rows = json.loads(out)["trials"]
        self.assertEqual([row["variant"] for row in rows], ["original", "compacted", "compacted", "original", "original", "compacted"])
        with mock.patch.dict(os.environ, {"FUSEBOX_API_KEY": ""}), \
                mock.patch.object(bench, "request_trial", return_value={**completed_row, "status": "cancelled_by_user"}) as call:
            code, out, _ = self.invoke(original, original, ["--execute"])
        self.assertEqual((code, call.call_count), (130, 1))
        self.assertEqual(json.loads(out)["summary"]["variants"]["original"]["completed"], 0)

    def test_one_session_pins_all_trials_and_only_last_planned_request_ends_it(self):
        seen = []
        def callback(handler):
            seen.append((handler.headers.get("x-fusebox-session-id"),
                         handler.headers.get("x-fusebox-session-end"), strict_body(handler.request_body)))
            headers(handler, "application/json")
            handler.wfile.write(json.dumps(completed(usage={"input_tokens": 10, "output_tokens": 2})).encode())

        def strict_body(body):
            return bench.strict_json(body)

        original = {"model": "synthetic", "input": "original", "stream": False}
        compacted = {**original, "input": "compacted"}
        with server(callback) as endpoint, mock.patch.dict(os.environ, {"FUSEBOX_API_KEY": "synthetic-key"}), \
                mock.patch.object(bench.uuid, "uuid4", return_value="fixture"):
            code, out, err = self.invoke(original, compacted, ["--execute", "--endpoint", endpoint])
        self.assertEqual((code, err, len(seen)), (0, "", 6))
        self.assertEqual(len({row[0] for row in seen}), 1)
        self.assertTrue(seen[0][0].startswith("context-benchmark-"))
        self.assertEqual([row[1] for row in seen], [None] * 5 + ["true"])
        self.assertEqual([row[2] for row in seen], [original, compacted, compacted, original, original, compacted])
        self.assertNotIn(seen[0][0], out)
        self.assertNotIn("synthetic-key", out + err)
        self.assertEqual(json.loads(out)["session_fingerprint"], "5ec3d2c1888154267e20e0b86d96b0a4")

    def test_endpoint_rejects_credentials_queries_and_remote_plaintext(self):
        for endpoint in ("https://user:PRIVATE@example.com/v1/responses", "https://example.com/v1/responses?key=PRIVATE",
                         "http://example.com/v1/responses"):
            with self.assertRaises(bench.BenchmarkError):
                bench.validate_endpoint(endpoint, False)

    def test_request_size_and_duplicate_fields_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "request.json"
            path.write_text('{"model":"one","model":"two","input":"hello"}')
            for limit, expected in ((1, "request_body_limit"), (1000, "invalid_request_json")):
                with self.assertRaisesRegex(bench.BenchmarkError, expected):
                    bench.load_request(path, limit)


if __name__ == "__main__":
    unittest.main()
