#!/usr/bin/env python3
"""Opt-in, metadata-only comparison of two caller-authored Responses requests."""

import argparse
import contextlib
import getpass
import hashlib
import http.client
import ipaddress
import json
import math
import os
import signal
import socket
import statistics
import sys
import time
import urllib.parse
import uuid
from pathlib import Path


USAGE_PATHS = {
    "input_tokens": ("input_tokens",),
    "output_tokens": ("output_tokens",),
    "total_tokens": ("total_tokens",),
    "cached_input_tokens": ("input_tokens_details", "cached_tokens"),
    "cache_write_input_tokens": ("input_tokens_details", "cache_write_tokens"),
    "reasoning_output_tokens": ("output_tokens_details", "reasoning_tokens"),
}
TERMINALS = {"completed", "incomplete", "failed", "cancelled"}


class BenchmarkError(Exception):
    """Only fixed, safe error codes belong in this exception."""


class DeadlineExceeded(TimeoutError):
    pass


def strict_json(data):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError("duplicate_key")
            result[key] = value
        return result

    def reject_constant(_):
        raise ValueError("non_finite_number")

    def finite_float(value):
        number = float(value)
        if not math.isfinite(number):
            raise ValueError("non_finite_number")
        return number

    return json.loads(data, object_pairs_hook=pairs, parse_constant=reject_constant, parse_float=finite_float)


def usage_fields(response):
    usage = response.get("usage")
    values = {key: None for key in USAGE_PATHS}
    warnings = []
    if not isinstance(usage, dict):
        return values, ["usage_missing"]
    for name, path in USAGE_PATHS.items():
        value = usage
        for part in path:
            value = value.get(part) if isinstance(value, dict) else None
        if name == "cache_write_input_tokens" and value is None:
            details = usage.get("input_tokens_details")
            if isinstance(details, dict):
                value = details.get("cache_creation_tokens")
        if value is not None:
            if type(value) is int and value >= 0:
                values[name] = value
            else:
                warnings.append("invalid_" + name)
    if values["input_tokens"] is None or values["output_tokens"] is None:
        warnings.append("input_or_output_usage_missing")
    if (values["cached_input_tokens"] is not None and values["input_tokens"] is not None
            and values["cached_input_tokens"] > values["input_tokens"]):
        warnings.append("cached_exceeds_input_tokens")
        values["cached_input_tokens"] = None
    return values, warnings


def metadata_label(value):
    # Do not retain IDs, error messages, text, tool arguments, or arbitrary objects.
    return value if isinstance(value, str) and 0 < len(value) <= 128 and all(
        char.isalnum() or char in "._:/-" for char in value) else None


def session_journal_fingerprint(api_key, session_id):
    """Match server::client_auth, affinity::session_identity and diagnostics::fingerprint."""
    def digest(parts):
        value = hashlib.sha256()
        for part in parts:
            encoded = part.encode("utf-8")
            value.update(len(encoded).to_bytes(8, "little"))
            value.update(encoded)
        return value.hexdigest()

    # No Authorization header is sent for an empty key. A whitespace-only Bearer
    # value also has no usable credential after the server trims the whole header.
    scope = digest(("client", api_key.strip() or "anonymous"))
    identity = digest((scope, "session", session_id.strip()))
    return hashlib.sha256(identity.encode("ascii")).hexdigest()[:32]


class ResponseMetrics:
    def __init__(self):
        self.status = None
        self.ttft_ms = None
        self.first_output_delta_ms = None
        self.usage = {key: None for key in USAGE_PATHS}
        self.usage_warnings = ["usage_missing"]
        self.reported_model = None
        self.reported_service_tier = None

    def terminal(self, response, status):
        if not isinstance(response, dict) or not isinstance(status, str) or status not in TERMINALS:
            raise BenchmarkError("malformed_terminal")
        if response.get("status", status) != status:
            raise BenchmarkError("contradictory_terminal_status")
        self.status = status
        self.usage, self.usage_warnings = usage_fields(response)
        self.reported_model = metadata_label(response.get("model"))
        self.reported_service_tier = metadata_label(response.get("service_tier"))

    def event(self, event_name, data, elapsed_ms):
        if not isinstance(data, dict):
            raise BenchmarkError("malformed_event")
        kind = data.get("type", event_name)
        if not isinstance(kind, str):
            raise BenchmarkError("malformed_event_type")
        if kind == "error":
            self.status = "provider_error"
            return
        text_delta = kind in {"response.output_text.delta", "response.refusal.delta"}
        output_delta = text_delta or kind in {
            "response.function_call_arguments.delta", "response.output_audio.delta", "response.audio.delta"}
        if output_delta and isinstance(data.get("delta"), str) and data["delta"]:
            if self.first_output_delta_ms is None:
                self.first_output_delta_ms = elapsed_ms
            if text_delta and self.ttft_ms is None:
                self.ttft_ms = elapsed_ms
        if kind in {"response." + status for status in TERMINALS}:
            self.terminal(data.get("response"), kind.removeprefix("response."))
        elif kind == "response.done":
            response = data.get("response")
            self.terminal(response, response.get("status") if isinstance(response, dict) else None)


class SSEParser:
    """Incremental SSE framing; EOF does not dispatch an unterminated event."""
    def __init__(self, metrics):
        self.metrics = metrics
        self.buffer = b""
        self.data = []
        self.event_name = ""
        self.done = False

    def feed(self, chunk, elapsed_ms):
        self.buffer += chunk
        while b"\n" in self.buffer:
            line, self.buffer = self.buffer.split(b"\n", 1)
            line = line.removesuffix(b"\r")
            if not line:
                if self.data:
                    payload = b"\n".join(self.data)
                    if payload == b"[DONE]":
                        self.done = True
                    else:
                        try:
                            parsed = strict_json(payload)
                        except (ValueError, UnicodeError, RecursionError):
                            raise BenchmarkError("invalid_event_json") from None
                        self.metrics.event(self.event_name, parsed, elapsed_ms)
                self.data, self.event_name = [], ""
                if self.metrics.status is not None or self.done:
                    return
            elif not line.startswith(b":"):
                field, separator, value = line.partition(b":")
                if separator and value.startswith(b" "):
                    value = value[1:]
                if field == b"data":
                    self.data.append(value)
                elif field == b"event":
                    try:
                        self.event_name = value.decode("utf-8")
                    except UnicodeError:
                        raise BenchmarkError("invalid_event_name") from None


@contextlib.contextmanager
def deadline(seconds):
    # A socket timeout alone does not bound a stream that keeps trickling bytes.
    def expired(_signum, _frame):
        raise DeadlineExceeded()

    old_handler = signal.signal(signal.SIGALRM, expired)
    old_timer = signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, old_handler)
        if old_timer[0]:
            signal.setitimer(signal.ITIMER_REAL, *old_timer)


def request_trial(endpoint, body, api_key, timeout, max_response_bytes, session_id=None, session_end=False):
    metrics = ResponseMetrics()
    result = {"http_status": None, "response_bytes": 0}
    parsed = urllib.parse.urlsplit(endpoint)
    connection_class = http.client.HTTPSConnection if parsed.scheme == "https" else http.client.HTTPConnection
    connection = connection_class(parsed.hostname, parsed.port, timeout=timeout)
    response = None
    started = time.perf_counter()
    try:
        with deadline(timeout):
            headers = {"Content-Type": "application/json", "Accept": "text/event-stream, application/json",
                       "Accept-Encoding": "identity"}
            if api_key:
                headers["Authorization"] = "Bearer " + api_key
            if session_id:
                headers["x-fusebox-session-id"] = session_id
                if session_end:
                    headers["x-fusebox-session-end"] = "true"
            connection.request("POST", parsed.path, body=body, headers=headers)
            response = connection.getresponse()
            result["http_status"] = response.status
            if response.status != 200:
                # In particular, never follow a redirect with a credential or replay a POST.
                raise BenchmarkError("http_error")
            if response.getheader("Content-Encoding", "identity").lower() != "identity":
                raise BenchmarkError("unsupported_content_encoding")
            content_type = response.getheader("Content-Type", "").split(";", 1)[0].strip().lower()
            if content_type not in {"text/event-stream", "application/json"}:
                raise BenchmarkError("unsupported_content_type")
            parser = SSEParser(metrics)
            body_parts = []
            while True:
                chunk = response.read1(min(65536, max_response_bytes - result["response_bytes"] + 1))
                if not chunk:
                    break
                result["response_bytes"] += len(chunk)
                if result["response_bytes"] > max_response_bytes:
                    raise BenchmarkError("response_body_limit")
                if content_type == "text/event-stream":
                    parser.feed(chunk, (time.perf_counter() - started) * 1000)
                    if metrics.status is not None or parser.done:
                        break
                else:
                    body_parts.append(chunk)
            if content_type == "application/json":
                if response.length is not None and response.length > 0:
                    raise BenchmarkError("truncated_http_body")
                try:
                    value = strict_json(b"".join(body_parts))
                except (ValueError, UnicodeError, RecursionError):
                    raise BenchmarkError("invalid_response_json") from None
                metrics.terminal(value, value.get("status") if isinstance(value, dict) else None)
            if metrics.status is None:
                raise BenchmarkError("missing_terminal")
            result["status"] = metrics.status
    except KeyboardInterrupt:
        result["status"] = "cancelled_by_user"
    except (DeadlineExceeded, socket.timeout):
        result["status"] = "timeout"
    except BenchmarkError as error:
        result["status"] = str(error)
    except (OSError, http.client.HTTPException, ValueError):
        # Do not print exception strings: providers may include private response fragments.
        result["status"] = "transport_error"
    finally:
        if response is not None:
            response.close()
        connection.close()
    result.update({"latency_ms": round((time.perf_counter() - started) * 1000, 3),
                   "ttft_ms": metrics.ttft_ms, "first_output_delta_ms": metrics.first_output_delta_ms,
                   "usage": metrics.usage, "usage_warnings": metrics.usage_warnings,
                   "reported_model": metrics.reported_model, "reported_service_tier": metrics.reported_service_tier})
    return result


def load_request(path, limit):
    with Path(path).open("rb") as source:
        body = source.read(limit + 1)
    if len(body) > limit:
        raise BenchmarkError("request_body_limit")
    try:
        value = strict_json(body)
    except (ValueError, UnicodeError, RecursionError):
        raise BenchmarkError("invalid_request_json") from None
    if not isinstance(value, dict) or not isinstance(value.get("model"), str) or not value["model"]:
        raise BenchmarkError("request_requires_model")
    if "input" not in value or not isinstance(value["input"], (str, list)):
        raise BenchmarkError("request_requires_input")
    if "stream" in value and type(value["stream"]) is not bool:
        raise BenchmarkError("invalid_stream_setting")
    if value.get("background") or "previous_response_id" in value or "conversation" in value:
        raise BenchmarkError("stateful_requests_not_supported")
    if "tools" in value and (not isinstance(value["tools"], list) or any(
            not isinstance(tool, dict) or tool.get("type") != "function" for tool in value["tools"])):
        raise BenchmarkError("only_function_tool_definitions_supported")
    context = {key: value[key] for key in ("input", "instructions") if key in value}
    encode = lambda obj: json.dumps(obj, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    try:
        metadata = {"sha256": hashlib.sha256(body).hexdigest(), "wire_bytes": len(body),
                    "canonical_json_bytes": len(encode(value)), "context_json_bytes": len(encode(context))}
    except UnicodeError:
        raise BenchmarkError("invalid_request_unicode") from None
    return body, value, metadata


def validate_endpoint(endpoint, allow_insecure):
    try:
        parsed = urllib.parse.urlsplit(endpoint)
        if (parsed.scheme not in {"http", "https"} or not parsed.hostname or parsed.username is not None
                or parsed.password is not None or parsed.query or parsed.fragment
                or parsed.path != "/v1/responses" or not (0 < (parsed.port or 80) < 65536)):
            raise ValueError()
        try:
            loopback = ipaddress.ip_address(parsed.hostname).is_loopback
        except ValueError:
            loopback = parsed.hostname == "localhost"
        if parsed.scheme == "http" and not loopback and not allow_insecure:
            raise BenchmarkError("remote_http_requires_allow_insecure_http")
    except ValueError:
        raise BenchmarkError("invalid_endpoint") from None


def distribution(values):
    return {"observations": len(values), "median": statistics.median(values) if values else None,
            "mean": statistics.mean(values) if values else None,
            "min": min(values) if values else None, "max": max(values) if values else None}


def metric_value(trial, key):
    return trial["usage"].get(key) if key in USAGE_PATHS else trial.get(key)


def summarize(trials, planned_pairs):
    keys = ["latency_ms", "ttft_ms", "first_output_delta_ms", *USAGE_PATHS]
    result = {"variants": {}, "paired_compacted_minus_original": {}}
    for variant in ("original", "compacted"):
        rows = [trial for trial in trials if trial["variant"] == variant]
        completed = [trial for trial in rows if trial["status"] == "completed"]
        result["variants"][variant] = {
            "attempted": len(rows), "completed": len(completed), "planned": planned_pairs,
            "metrics_for_completed_responses": {
                key: distribution([metric_value(row, key) for row in completed if metric_value(row, key) is not None])
                for key in keys}}
    pairs = []
    for index in range(1, planned_pairs + 1):
        pair = {trial["variant"]: trial for trial in trials if trial["pair"] == index}
        if set(pair) == {"original", "compacted"} and all(row["status"] == "completed" for row in pair.values()):
            pairs.append(pair)
    for key in keys:
        deltas = [metric_value(pair["compacted"], key) - metric_value(pair["original"], key)
                  for pair in pairs if all(metric_value(row, key) is not None for row in pair.values())]
        result["paired_compacted_minus_original"][key] = {
            **distribution(deltas), "all_planned_pairs_observed": len(deltas) == planned_pairs}
    result["provider_metadata_varied"] = any(
        len({row.get(field) for row in trials if row["status"] == "completed"}) > 1
        for field in ("reported_model", "reported_service_tier"))
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--original", required=True, help="Original request JSON file")
    parser.add_argument("--compacted", required=True, help="Caller-authored compacted request JSON file")
    parser.add_argument("--endpoint", default="http://127.0.0.1:8317/v1/responses")
    parser.add_argument("--trials", type=int, default=3, help="Pairs of requests, 1–20 (default: 3)")
    parser.add_argument("--timeout", type=float, default=120, help="Total seconds per request, 0–600")
    parser.add_argument("--max-request-bytes", type=int, default=2 * 1024 * 1024)
    parser.add_argument("--max-response-bytes", type=int, default=8 * 1024 * 1024)
    parser.add_argument("--execute", action="store_true", help="Send requests; may spend tokens")
    parser.add_argument("--allow-insecure-http", action="store_true", help="Allow unencrypted remote HTTP")
    args = parser.parse_args(argv)
    try:
        if not 1 <= args.trials <= 20 or not 0 < args.timeout <= 600:
            raise BenchmarkError("invalid_trial_or_timeout_limit")
        if not 1 <= args.max_request_bytes <= 16 * 1024 * 1024 or not 1 <= args.max_response_bytes <= 64 * 1024 * 1024:
            raise BenchmarkError("invalid_body_limit")
        validate_endpoint(args.endpoint, args.allow_insecure_http)
        loaded = {name: load_request(getattr(args, name), args.max_request_bytes) for name in ("original", "compacted")}
        controls = lambda value: {key: val for key, val in value.items() if key not in {"input", "instructions"}}
        if controls(loaded["original"][1]) != controls(loaded["compacted"][1]):
            raise BenchmarkError("non_context_settings_differ")
        report = {"schema_version": 1, "mode": "execute" if args.execute else "dry_run",
                  "planned_requests": 2 * args.trials, "timeout_seconds_per_request": args.timeout,
                  "requests": {name: item[2] for name, item in loaded.items()},
                  "model": metadata_label(loaded["original"][1]["model"]),
                  "stream_requested": loaded["original"][1].get("stream", False),
                  "order": "original/compacted on odd pairs; compacted/original on even pairs",
                  "usage_source": "response_usage_fields_at_fusebox_endpoint",
                  "trials": [], "quality_evaluated": False}
        before = loaded["original"][2]["context_json_bytes"]
        after = loaded["compacted"][2]["context_json_bytes"]
        report["offline_context_bytes"] = {"compacted_minus_original": after - before,
                                            "reduction_fraction": (before - after) / before,
                                            "token_savings": None}
        if args.execute:
            if not hasattr(signal, "setitimer"):
                raise BenchmarkError("execute_requires_unix_deadline_support")
            api_key = os.environ.get("FUSEBOX_API_KEY")
            if api_key is None:
                if not sys.stdin.isatty():
                    raise BenchmarkError("set_FUSEBOX_API_KEY_for_noninteractive_execution")
                api_key = getpass.getpass("Fusebox API key (blank for unauthenticated local endpoint): ")
            if any(ord(char) < 32 or ord(char) > 126 for char in api_key):
                raise BenchmarkError("invalid_api_key_characters")
            session_id = "context-benchmark-" + str(uuid.uuid4())
            report["session_fingerprint"] = session_journal_fingerprint(api_key, session_id)
            stop = False
            for pair in range(1, args.trials + 1):
                order = ("original", "compacted") if pair % 2 else ("compacted", "original")
                for variant in order:
                    row = request_trial(args.endpoint, loaded[variant][0], api_key, args.timeout, args.max_response_bytes,
                                        session_id=session_id, session_end=pair == args.trials and variant == order[-1])
                    row.update({"variant": variant, "pair": pair})
                    report["trials"].append(row)
                    if row["status"] == "cancelled_by_user" or row["http_status"] in {401, 403, 404, 429}:
                        stop = True
                        break
                if stop:
                    break
            report["summary"] = summarize(report["trials"], args.trials)
        print(json.dumps(report, indent=2, allow_nan=False))
        if any(row["status"] == "cancelled_by_user" for row in report["trials"]):
            return 130
        if any(row["status"] != "completed" for row in report["trials"]):
            return 1
        return 0
    except (BenchmarkError, OSError, RecursionError) as error:
        code = str(error) if isinstance(error, BenchmarkError) else "request_file_read_or_decode_error"
        print(json.dumps({"error": code}), file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print(json.dumps({"error": "cancelled_before_or_between_requests"}), file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
