import importlib.util
from contextlib import closing
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("retirement", Path(__file__).parents[1] / "scripts/retire-legacy-usage-keeper.py")
retirement = importlib.util.module_from_spec(spec)
spec.loader.exec_module(retirement)


class KeeperRetirement(unittest.TestCase):
    def test_native_capture_requires_recent_durable_error_free_observations(self):
        health = {"state": "healthy", "written": 1, "last_commit_at_ms": 900_000}
        self.assertEqual(retirement.require_native_capture({"health": health}, 1_000_000)["written"], 1)
        for change in [{"state": "degraded"}, {"written": 0}, {"last_commit_at_ms": 1}, {"last_commit_at_ms": 1_000_001}, {"dropped": 1}, {"writer_errors": 1}, {"rejected": 1}, {"prior_unclosed_sessions": 1}]:
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                retirement.require_native_capture({"health": {**health, **change}}, 1_000_000)

    def test_other_ledger_writes_cannot_hide_stale_native_capture(self):
        now = 1_000_000
        # Imports/settings can commit recently despite native capture being stale.
        retirement.require_native_capture({"health": {"state": "healthy", "written": 10, "last_commit_at_ms": now}}, now)
        recent = {"source": "proxy", "parser_version": "proxy-native-v1", "origin_id": "local", "event_at_ms": now - 1000, "ingested_at_ms": now - 100}
        result = retirement.require_recent_proxy_capture({"items": [{**recent, "account_id": "private-account", "session_id": "private-session"}]}, now)
        self.assertEqual(result, {"event_at_ms": now - 1000, "ingested_at_ms": now - 100})
        for change in [{"event_at_ms": 1}, {"ingested_at_ms": 1}, {"event_at_ms": now + 1}, {"ingested_at_ms": now + 1}, {"source": "codex"}, {"origin_id": "collector"}, {"parser_version": "other"}, {"event_at_ms": None}]:
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                retirement.require_recent_proxy_capture({"items": [{**recent, **change}]}, now)
        with self.assertRaises(RuntimeError):
            retirement.require_recent_proxy_capture({"items": []}, now)

    def test_native_capture_is_rechecked_after_backup_before_disabling_service(self):
        with mock.patch("sys.argv", ["retire", "--apply"]), \
                mock.patch.object(retirement.os, "geteuid", return_value=0), \
                mock.patch.object(retirement.os, "umask"), \
                mock.patch.object(retirement, "native_status", side_effect=[{"state": "healthy"}, RuntimeError("stale capture")]), \
                mock.patch.object(retirement, "service_state", return_value="active"), \
                mock.patch.object(retirement, "backup_database", return_value={"path": "/retained/backup"}) as backup, \
                mock.patch.object(retirement.subprocess, "run") as service:
            with self.assertRaisesRegex(RuntimeError, "stale capture"):
                retirement.main()
            backup.assert_called_once()
            service.assert_not_called()

    def test_backup_includes_committed_wal_and_keeps_source_intact(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "live.sqlite3"
            with closing(sqlite3.connect(source)) as writer:
                writer.execute("PRAGMA journal_mode=WAL")
                writer.execute("PRAGMA wal_autocheckpoint=0")
                writer.execute("CREATE TABLE usage_events(id INTEGER PRIMARY KEY)")
                writer.execute("INSERT INTO usage_events VALUES (1)")
                writer.commit()
                self.assertTrue(source.with_name(source.name + "-wal").exists())
                destination = root / "new-parent" / "private"
                with mock.patch.object(retirement, "sync_directory", wraps=retirement.sync_directory) as sync:
                    proof = retirement.backup_database(source, destination)
                self.assertEqual([call.args[0] for call in sync.call_args_list], [root, destination.parent, destination])
                self.assertEqual(proof["records"], 1)
                self.assertEqual(Path(proof["path"]).stat().st_mode & 0o777, 0o600)
                self.assertEqual(writer.execute("SELECT COUNT(*) FROM usage_events").fetchone(), (1,))
                with closing(sqlite3.connect(proof["path"])) as backup:
                    self.assertEqual(backup.execute("SELECT id FROM usage_events").fetchall(), [(1,)])


if __name__ == "__main__":
    unittest.main()
