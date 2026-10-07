#!/usr/bin/env python3
"""Back up the obsolete keeper and retire it only after native capture is healthy.

Read-only by default. --apply creates a private, consistent SQLite backup and
disables the old service. It never deletes historical data or changes Fusebox.
"""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import shlex
import sqlite3
import ssl
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request

SERVICE = "cpa-usage-keeper.service"


def require_native_capture(status, now_ms=None):
    health = status.get("health", {})
    now_ms = int(time.time() * 1000) if now_ms is None else now_ms
    if health.get("state") != "healthy":
        raise RuntimeError("native usage capture is not healthy")
    if any(health.get(k, 0) for k in ("dropped", "rejected", "writer_errors", "prior_unclosed_sessions")):
        raise RuntimeError("native usage capture has unresolved gaps")
    if health.get("written", 0) < 1 or not 0 <= now_ms - health.get("last_commit_at_ms", 0) <= 300_000:
        raise RuntimeError("native usage capture needs a recent durable observation")
    return {k: health.get(k) for k in ("state", "written", "last_commit_at_ms", "dropped", "rejected", "writer_errors")}


def require_recent_proxy_capture(observations, now_ms=None):
    """Other ledger transactions can refresh health.last_commit_at_ms."""
    now_ms = int(time.time() * 1000) if now_ms is None else now_ms
    rows = observations.get("items", [])
    if not rows:
        raise RuntimeError("native proxy capture has no durable observation")
    latest = rows[0]
    if latest.get("source") != "proxy" or latest.get("parser_version") != "proxy-native-v1" or latest.get("origin_id") != "local":
        raise RuntimeError("latest observation is not local native proxy capture")
    for field in ("event_at_ms", "ingested_at_ms"):
        timestamp = latest.get(field)
        if type(timestamp) is not int or not 0 <= now_ms - timestamp <= 300_000:
            raise RuntimeError("native proxy capture needs a recent durable observation")
    # Do not include the observation's account, client, session or request IDs.
    return {field: latest[field] for field in ("event_at_ms", "ingested_at_ms")}


def native_status(args):
    url = urllib.parse.urlsplit(args.base_url)
    if url.scheme != "https" or not url.hostname or url.username or url.password or url.query or url.fragment:
        raise RuntimeError("base URL must be HTTPS without credentials, query or fragment")
    key = None
    for line in Path(args.management_env).read_text().splitlines():
        if line.strip().startswith("CPA_MANAGEMENT_KEY="):
            values = shlex.split(line.strip().split("=", 1)[1])
            if len(values) == 1:
                key = values[0]
    if not key:
        raise RuntimeError("management credential unavailable")
    context = ssl.create_default_context(cafile=args.ca_file)
    context.verify_flags |= ssl.VERIFY_X509_PARTIAL_CHAIN

    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *unused):
            return None

    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=context), NoRedirect())
    def read(endpoint):
        request = urllib.request.Request(args.base_url.rstrip("/") + endpoint, headers={"Authorization": "Bearer " + key, "Connection": "close"})
        with opener.open(request, timeout=10) as response:
            data = response.read(1_048_577)
            if len(data) > 1_048_576:
                raise RuntimeError("oversized usage response")
        return json.loads(data)

    health = require_native_capture(read("/api/usage/status"))
    health["latest_proxy_observation"] = require_recent_proxy_capture(read("/api/usage/observations?source=proxy&limit=1"))
    return health


def sync_directory(directory):
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def backup_database(source, directory):
    source, directory = Path(source).resolve(strict=True), Path(directory)
    created = []
    ancestor = directory
    while not ancestor.exists():
        created.append(ancestor)
        ancestor = ancestor.parent
    directory.mkdir(parents=True, mode=0o700, exist_ok=True)
    if directory.is_symlink() or directory.stat().st_mode & 0o077:
        raise RuntimeError("backup directory must be private")
    # Persist each new directory entry, not just the final backup file entry.
    for child in reversed(created):
        sync_directory(child.parent)
    fd, name = tempfile.mkstemp(prefix="keeper-", suffix=".sqlite3", dir=directory)
    os.close(fd)
    destination = Path(name)
    deadline = time.monotonic() + 120

    def progress(*unused):
        if time.monotonic() > deadline:
            raise RuntimeError("backup deadline exceeded")

    try:
        with closing(sqlite3.connect(source.as_uri() + "?mode=ro", uri=True, timeout=10)) as reader:
            reader.execute("PRAGMA query_only=ON")
            with closing(sqlite3.connect(destination)) as writer:
                reader.backup(writer, pages=512, progress=progress, sleep=0.1)
                if writer.execute("PRAGMA quick_check").fetchall() != [("ok",)]:
                    raise RuntimeError("keeper backup integrity check failed")
                records = writer.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]
        with destination.open("rb") as handle:
            digest = hashlib.file_digest(handle, "sha256").hexdigest()
            os.fsync(handle.fileno())
        sync_directory(directory)
        return {"path": str(destination), "sha256": digest, "records": records, "bytes": destination.stat().st_size}
    except Exception:
        destination.unlink(missing_ok=True)
        raise


def service_state(action):
    result = subprocess.run(["systemctl", action, SERVICE], capture_output=True, text=True, timeout=15)
    return result.stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--base-url", default="https://10.10.0.235:8317")
    parser.add_argument("--ca-file", default="/etc/cli-proxy-api/tls/server.crt")
    parser.add_argument("--management-env", default="/etc/cpa-usage-keeper/keeper.env")
    parser.add_argument("--database", default="/var/lib/cpa-usage-keeper/data/app.db")
    parser.add_argument("--backup-dir", default="/var/lib/cliproxy-rust-update/legacy-usage-backups")
    args = parser.parse_args()
    os.umask(0o077)
    health = native_status(args)
    before = {"active": service_state("is-active"), "enabled": service_state("is-enabled")}
    if not args.apply:
        print(json.dumps({"mode": "read-only", "native_capture": health, "keeper": before, "would_back_up_then_disable": SERVICE}))
        return
    if os.geteuid() != 0:
        raise RuntimeError("retiring the system service requires root")
    backup = backup_database(args.database, args.backup_dir)
    health = native_status(args)
    subprocess.run(["systemctl", "disable", "--now", SERVICE], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True, timeout=120)
    after = {"active": service_state("is-active"), "enabled": service_state("is-enabled")}
    if after != {"active": "inactive", "enabled": "disabled"}:
        raise RuntimeError("keeper retirement did not complete; backup retained")
    receipt = {"outcome": "retired", "service": SERVICE, "before": before, "after": after, "backup": backup, "native_capture": health, "completed_at_ms": int(time.time() * 1000)}
    receipt_path = Path(backup["path"]).with_suffix(".json")
    with receipt_path.open("x") as handle:
        json.dump(receipt, handle, indent=2)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    sync_directory(args.backup_dir)
    print(json.dumps(receipt))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Library errors can include credential-bearing URLs or response content.
        print(json.dumps({"outcome": "failed", "error_type": type(error).__name__}))
        raise SystemExit(1)
