"""Offline and localhost-mock tests for handy-sync. No production calls."""
from __future__ import annotations

import contextlib
import io
import json
import os
from pathlib import Path
import socket
import stat
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import handy_sync as hs
import handy_auto as ha


SECRET_API = "api-secret-test-marker"
SECRET_CONNECTION = "connection-secret-test-marker"
SECRET_BEARER = "bearer-secret-test-marker"


class Response:
    def __init__(self, data=None, headers=None, status=200):
        self.data = data or {}
        self.headers = headers or {}
        self.status = status
        self.closed = False

    def read(self, limit=-1):
        raw = json.dumps(self.data).encode()
        return raw if limit < 0 else raw[:limit]

    def close(self):
        self.closed = True

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


class QueueOpener:
    def __init__(self, *responses):
        self.responses = list(responses)
        self.requests = []

    def open(self, request, timeout=0):
        self.requests.append(request)
        item = self.responses.pop(0)
        if isinstance(item, BaseException):
            raise item
        return item


class FakeClient:
    dry_run = False

    def __init__(self, max_points=100):
        self.max_points = max_points
        self.calls = []
        self.tail_index = 0
        self.secrets = (SECRET_API, SECRET_CONNECTION)
        self.messages = []

    def log(self, message): self.messages.append(message)
    def stop(self): self.calls.append(("stop",))
    def pause(self): self.calls.append(("pause",))
    def resume(self, pick_up=False): self.calls.append(("resume", pick_up))
    def flush(self): self.calls.append(("flush",))
    def set_stroke(self, low, high): self.calls.append(("stroke", low, high))
    def setup(self): self.calls.append(("setup",))
    def play(self, t, rate=1.0): self.calls.append(("play", t() if callable(t) else t, rate))
    def synctime(self, t): self.calls.append(("synctime", t() if callable(t) else t))
    def add(self, points, *, flush=False, stream_start=0):
        self.calls.append(("add", [dict(p) for p in points], flush, stream_start))


def error_response(status, headers=None, body=b""):
    return urllib.error.HTTPError("https://www.handyfeeling.com/", status, "private", headers or {}, io.BytesIO(body))


class HandySyncTests(unittest.TestCase):
    def test_funscript_validates_order_and_bounds(self):
        self.assertEqual(hs.validate_actions({"actions": [{"at": 0, "pos": 0}, {"at": 5, "pos": 100}]}),
                         [{"at": 0, "pos": 0}, {"at": 5, "pos": 100}])
        for data in (
            {"actions": []},
            {"actions": [{"at": True, "pos": 1}]},
            {"actions": [{"at": 1, "pos": 101}]},
            {"actions": [{"at": 1, "pos": -1}]},
            {"actions": [{"at": -1, "pos": 5}]},
            {"actions": [{"at": "1", "pos": 5}]},
            {"actions": [{"at": float("nan"), "pos": 5}]},
        ):
            with self.subTest(data=data), self.assertRaises(hs.HandyError):
                hs.validate_actions(data)

    def test_real_world_scripts_are_tidied_in_memory_and_reported(self):
        notes = []
        data = {"actions": [{"at": 100.4, "pos": 50.0}, {"at": 50, "pos": 10}, {"at": 100, "pos": 99.6},
                            {"at": 200, "pos": 100.4}, {"at": 300, "pos": 0}]}
        # 100.4 and 100 both become 100: merged, the later one in file order wins; 50 is sorted first.
        self.assertEqual(hs.validate_actions(data, notes),
                         [{"at": 50, "pos": 10}, {"at": 100, "pos": 100}, {"at": 200, "pos": 100}, {"at": 300, "pos": 0}])
        self.assertEqual(len(notes), 3)
        self.assertEqual(data["actions"][0], {"at": 100.4, "pos": 50.0})  # the input is not modified
        clean = []
        hs.validate_actions({"actions": [{"at": 1, "pos": 2}, {"at": 3, "pos": 4}]}, clean)
        self.assertEqual(clean, [])

    def test_drive_map_longest_prefix_unc_and_fallback(self):
        hs.set_drive_map({"X:": "/mnt/net/share3", "Y:": "/mnt/net/share2/VR", "O:": "/mnt/net/share2",
                          "\\\\server\\share4": "/mnt/net/share4"})
        try:
            with mock.patch.object(hs.os, "name", "posix"):
                self.assertEqual(str(hs.video_path(r"X:\scripts\SBS\a b.mp4")), "/mnt/net/share3/scripts/SBS/a b.mp4")
                self.assertEqual(str(hs.video_path("y:/clip.mp4")), "/mnt/net/share2/VR/clip.mp4")
                self.assertEqual(str(hs.video_path(r"O:\VR\clip.mp4")), "/mnt/net/share2/VR/clip.mp4")
                self.assertEqual(str(hs.video_path(r"\\SERVER\share4\media\p.mp4")), "/mnt/net/share4/media/p.mp4")
                self.assertEqual(str(hs.video_path(r"D:\clips\movie.mp4")), "/mnt/d/clips/movie.mp4")
                self.assertEqual(str(hs.video_path(r"XX:\nope.mp4")), r"XX:\nope.mp4")
        finally:
            hs.set_drive_map({})

    def test_drive_map_file_errors_are_loud(self):
        with tempfile.TemporaryDirectory() as temp:
            missing = Path(temp) / "none.json"
            self.assertEqual(hs.load_drive_map(missing), {})
            bad = Path(temp) / "bad.json"
            bad.write_text("{ nope")
            with self.assertRaises(hs.HandyError):
                hs.load_drive_map(bad)
            bad.write_text(json.dumps({"X:": "relative/path"}))
            with self.assertRaises(hs.HandyError):
                hs.load_drive_map(bad)

    def test_windows_path_and_variant_candidates(self):
        with mock.patch.object(hs.os, "name", "posix"):
            self.assertEqual(str(hs.video_path(r"D:\clips\movie.mp4")), "/mnt/d/clips/movie.mp4")
        with tempfile.TemporaryDirectory() as temp:
            video = Path(temp) / "movie.mp4"
            (Path(temp) / "movie.Bounce.funscript").write_text("{}")
            selected, variants = hs.script_candidates(str(video))
            self.assertEqual(selected.name, "movie.funscript")
            self.assertEqual([p.name for p in variants], ["movie.Bounce.funscript"])
            with self.assertRaises(hs.HandyError):
                hs.script_candidates(str(video), "../escape")

    def test_keys_require_owned_exact_0600_and_redact(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "keys.json"
            path.write_text(json.dumps({"api_key": SECRET_API, "connection_key": SECRET_CONNECTION}))
            path.chmod(0o600)
            self.assertEqual(hs.read_keys(path), (SECRET_API, "", SECRET_CONNECTION))
            path.chmod(0o640)
            with self.assertRaises(hs.HandyError):
                hs.read_keys(path)
            self.assertNotIn(SECRET_API, hs.redact(f"X-Api-Key: {SECRET_API}", (SECRET_API,)))

    def test_add_chunks_100_and_keeps_absolute_tail_index(self):
        client = hs.HspClient("", "", dry_run=True, emit=lambda _: None)
        calls = []
        client._request = lambda method, path, body=None, auth=True: calls.append((method, path, body)) or ({}, {})
        points = [{"at": i * 10, "pos": i % 101} for i in range(205)]
        client.add(points)
        self.assertEqual([len(call[2]["points"]) for call in calls], [100, 100, 5])
        self.assertEqual([call[2]["tail_point_stream_index"] for call in calls], [100, 200, 205])
        client.add(points[120:130], stream_start=120)
        self.assertEqual(calls[-1][2]["tail_point_stream_index"], 130)

    def test_lookahead_tops_up_without_evicting_unplayed_points(self):
        client = FakeClient(max_points=4)
        bridge = hs.Bridge(client, monotonic=lambda: 0.0)
        bridge.actions = [{"at": i * 1000, "pos": i} for i in range(20)]
        bridge.times = [a["at"] for a in bridge.actions]
        bridge._feed(0, reset=True)
        self.assertEqual(len(client.calls[-1][1]), 4)
        bridge._feed(1000)
        # Keep the predecessor and 100ms jitter margin until it is safely used.
        self.assertLessEqual(len(client.calls[-1][1]), 1)
        self.assertEqual(client.calls[-1][3], 4)
        bridge._feed(5000)
        self.assertEqual(len(client.calls[-1][1]), 4)

    def test_seek_flushes_and_refills_at_global_script_index(self):
        with tempfile.TemporaryDirectory() as temp:
            video = Path(temp) / "movie.mp4"
            script = Path(temp) / "movie.funscript"
            script.write_text(json.dumps({"actions": [{"at": i * 1000, "pos": i} for i in range(20)]}))
            client = FakeClient()
            bridge = hs.Bridge(client, monotonic=lambda: 0.0)
            bridge.process({"seq": 1, "video": str(video), "t": 0, "playing": True, "rate": 1.0})
            client.calls.clear()
            bridge.process({"seq": 2, "video": str(video), "t": 5000, "playing": True, "rate": 1.0})
            names = [call[0] for call in client.calls]
            self.assertIn("flush", names)
            add = next(call for call in client.calls if call[0] == "add")
            self.assertGreaterEqual(add[3], 4)
            self.assertIn(("play", 5000, 1.0), client.calls)

    def test_pause_resume_rate_and_offset_calls(self):
        with tempfile.TemporaryDirectory() as temp:
            video = Path(temp) / "movie.mp4"
            Path(temp, "movie.funscript").write_text(json.dumps({"actions": [{"at": 0, "pos": 0}, {"at": 1000, "pos": 100}, {"at": 2000, "pos": 0}]}))
            client = FakeClient()
            bridge = hs.Bridge(client, offset_ms=250, monotonic=lambda: 0.0)
            bridge.process({"seq": 1, "video": str(video), "t": 0, "playing": True, "rate": 1.0})
            self.assertIn(("play", 250, 1.0), client.calls)
            bridge.process({"seq": 2, "video": str(video), "t": 0, "playing": False, "rate": 1.0})
            self.assertIn(("pause",), client.calls)
            bridge.process({"seq": 3, "video": str(video), "t": 0, "playing": True, "rate": 1.0})
            self.assertIn(("resume", False), client.calls)
            bridge.process({"seq": 4, "video": str(video), "t": 0, "playing": True, "rate": 1.25})
            self.assertIn(("play", 250, 1.25), client.calls)

    def test_negative_offset_waits_until_script_clock_is_nonnegative(self):
        with tempfile.TemporaryDirectory() as temp:
            video = Path(temp) / "movie.mp4"
            Path(temp, "movie.funscript").write_text(json.dumps({"actions": [{"at": 0, "pos": 0}, {"at": 1000, "pos": 100}]}))
            client = FakeClient()
            bridge = hs.Bridge(client, offset_ms=-1000, monotonic=lambda: 0.0)
            bridge.process({"seq": 1, "video": str(video), "t": 0, "playing": True, "rate": 1.0})
            self.assertNotIn("play", [call[0] for call in client.calls])
            bridge.process({"seq": 2, "video": str(video), "t": 1000, "playing": True, "rate": 1.0})
            self.assertIn(("play", 0, 1.0), client.calls)

    def test_new_file_stops_previous_and_missing_or_bad_script_reports_once(self):
        messages = []
        client = FakeClient()
        bridge = hs.Bridge(client, emit=messages.append, monotonic=lambda: 0.0)
        with tempfile.TemporaryDirectory() as temp:
            missing = str(Path(temp) / "missing.mp4")
            bridge.process({"seq": 1, "video": missing, "t": 0, "playing": True, "rate": 1.0})
            bridge.process({"seq": 2, "video": missing, "t": 1, "playing": True, "rate": 1.0})
            self.assertEqual(sum("No funscript" in m for m in messages), 1)
            bad = Path(temp) / "bad.mp4"
            Path(temp, "bad.funscript").write_text("not json")
            bridge.process({"seq": 3, "video": str(bad), "t": 0, "playing": True, "rate": 1.0})
            bridge.process({"seq": 4, "video": str(bad), "t": 1, "playing": True, "rate": 1.0})
            self.assertEqual(sum("could not read funscript" in m.lower() and "idle" in m.lower() for m in messages), 1)

    def test_udp_message_rejects_invalid_types_and_accepts_windows(self):
        valid = {"v": 1, "seq": 1, "video": r"D:\clips\movie.mp4", "t": 0, "playing": True, "rate": 1.0}
        self.assertIsNotNone(hs.validate_message(json.dumps(valid).encode()))
        for change in ({"v": True}, {"seq": True}, {"t": -1}, {"playing": 1}, {"rate": float("nan")}, {"video": 12}):
            bad = dict(valid)
            bad.update(change)
            self.assertIsNone(hs.validate_message(json.dumps(bad).encode()))

    def test_stale_sequence_is_ignored(self):
        client = FakeClient()
        bridge = hs.Bridge(client)
        bridge.process({"seq": 5, "video": "C:\\movie.mp4", "t": 0, "playing": False, "rate": 1.0})
        bridge.process({"seq": 4, "video": "C:\\movie.mp4", "t": 10000, "playing": True, "rate": 1.0})
        self.assertEqual(bridge.last["seq"], 5)

    def test_starvation_recovery_refills_from_live_position(self):
        now = [0.0]
        client = FakeClient(max_points=10)
        with tempfile.TemporaryDirectory() as temp:
            video = Path(temp) / "dense.mp4"
            Path(temp, "dense.funscript").write_text(json.dumps({"actions": [
                {"at": i * 100, "pos": i % 101} for i in range(200)
            ]}))
            bridge = hs.Bridge(client, monotonic=lambda: now[0])
            bridge.process({"seq": 1, "video": str(video), "t": 0, "playing": True, "rate": 1.0})
            first_add = next(c for c in client.calls if c[0] == "add")
            self.assertLessEqual(first_add[1][-1]["at"], 1000)
            now[0] = 2.0
            bridge.process({"seq": 2, "video": str(video), "t": 2000, "playing": True, "rate": 1.0})
            add_calls = [c for c in client.calls if c[0] == "add"]
            self.assertGreaterEqual(sum(c[0] == "flush" for c in client.calls), 2)
            self.assertEqual(add_calls[-1][1][0]["at"], 2000)

    def test_receiver_drains_newest_and_invalid_traffic_does_not_reset_timeout(self):
        with tempfile.TemporaryDirectory() as temp:
            first = Path(temp) / "first.mp4"
            second = Path(temp) / "second.mp4"
            for stem, pos in (("first", 10), ("second", 20)):
                Path(temp, stem + ".funscript").write_text(json.dumps({"actions": [{"at": 0, "pos": pos}, {"at": 1000, "pos": 100}]}))
            def packet(seq, path):
                return json.dumps({"v": 1, "seq": seq, "video": str(path), "t": 0,
                                   "playing": True, "rate": 1.0}).encode()
            ticks = [0.0]
            class Receiver:
                def __init__(self): self.index = 0; self.closed = False
                def bind(self, address): self.address = address
                def settimeout(self, value): pass
                def setblocking(self, value): pass
                def close(self): self.closed = True
                def recvfrom(self, size):
                    self.index += 1
                    if self.index == 1: return packet(1, first), ("127.0.0.1", 1234)
                    if self.index == 2: return packet(3, second), ("127.0.0.1", 1234)
                    if self.index == 3: return packet(2, first), ("127.0.0.1", 1234)
                    if self.index == 4: return b"not json", ("127.0.0.1", 1234)
                    if self.index in (5, 7, 9, 11): raise BlockingIOError
                    if self.index == 6:
                        ticks[0] = 0.75
                        return packet(2, first), ("127.0.0.1", 1234)
                    if self.index in (8, 10):
                        ticks[0] += 0.75
                        return b"not json", ("127.0.0.1", 1234)
                    raise KeyboardInterrupt
            receiver = Receiver()
            def monotonic():
                return ticks[0]
            client = FakeClient()
            args = SimpleNamespace(variant=None, offset_ms=0, port=17777, bind="127.0.0.1")
            with mock.patch.object(hs.socket, "socket", return_value=receiver):
                hs.serve(args, client, monotonic=monotonic)
            self.assertTrue(receiver.closed)
            self.assertEqual(receiver.address, ("127.0.0.1", 17777))
            first_add = next(c for c in client.calls if c[0] == "add")
            self.assertEqual(first_add[1][0]["pos"], 20)
            # The higher queued sequence selects the second sidecar; stale and
            # malformed packets do not select the first or keep its stream alive.
            self.assertEqual(sum(c[0] == "setup" for c in client.calls), 1)
            self.assertEqual(sum(c[0] == "stop" for c in client.calls), 3)

    def test_drift_triggers_filtered_synctime(self):
        now = [0.0]
        client = FakeClient()
        bridge = hs.Bridge(client, monotonic=lambda: now[0])
        with tempfile.TemporaryDirectory() as temp:
            video = Path(temp) / "movie.mp4"
            Path(temp, "movie.funscript").write_text(json.dumps({"actions": [{"at": 0, "pos": 0}, {"at": 1000, "pos": 100}, {"at": 2000, "pos": 0}]}))
            bridge.process({"seq": 1, "video": str(video), "t": 0, "playing": True, "rate": 1.0})
            now[0] = 2.1
            bridge.process({"seq": 2, "video": str(video), "t": 3000, "playing": True, "rate": 1.0})
            self.assertIn(("synctime", 3000), client.calls)

    def test_median_server_time_uses_30_samples(self):
        client = hs.HspClient(dry_run=True, emit=lambda _: None)
        samples = iter([{"server_time": value} for value in range(100, 130)])
        client._request = lambda method, path, body=None, auth=True: (next(samples), {})
        times = iter([0.0, 0.0] * 30)
        client.clock = lambda: next(times)
        self.assertEqual(client.sync_server_time(), 114.5)

    def test_429_honors_retry_after_and_redacts_error_lines(self):
        headers = {"Retry-After": "3", "X-RateLimit-Remaining": "0", "X-RateLimit-Reset": "4"}
        opener = QueueOpener(error_response(429, headers, SECRET_API.encode()), Response({"result": {}}))
        sleeps, logs = [], []
        client = hs.HspClient(SECRET_API, SECRET_CONNECTION, opener=opener,
                              sleep=sleeps.append, emit=logs.append)
        client._request("PUT", "/hsp/add", {"points": []})
        self.assertEqual(len(sleeps), 1)
        self.assertAlmostEqual(sleeps[0], 4.0, places=3)
        self.assertTrue(all(SECRET_API not in line and SECRET_CONNECTION not in line for line in logs))
        self.assertEqual(len(opener.requests), 2)

    def test_server_clock_body_is_projected_after_rate_limit_wait(self):
        now = [10.0]
        waits = []
        def sleep(seconds):
            waits.append(seconds)
            now[0] += seconds
        headers = {"Retry-After": "3", "X-RateLimit-Remaining": "0", "X-RateLimit-Reset": "4"}
        opener = QueueOpener(error_response(429, headers), Response({"result": "ok"}))
        client = hs.HspClient(SECRET_API, SECRET_CONNECTION, opener=opener,
                              clock=lambda: now[0], sleep=sleep)
        client._request("PUT", "/hsp/play", lambda: {"server_time": round(now[0] * 1000)})
        self.assertEqual(len(waits), 1)
        self.assertAlmostEqual(waits[0], 4.0, places=3)
        self.assertEqual(json.loads(opener.requests[-1].data), {"server_time": 14000})

    def test_device_error_and_transport_error_do_not_echo_secrets(self):
        logs = []
        opener = QueueOpener(Response({"error": {"code": 1001, "message": SECRET_API}}))
        client = hs.HspClient(SECRET_API, SECRET_CONNECTION, opener=opener, emit=logs.append)
        with self.assertRaises(hs.HandyError) as caught:
            client._request("PUT", "/hsp/setup", {})
        self.assertNotIn(SECRET_API, str(caught.exception))
        bad = QueueOpener(RuntimeError(SECRET_CONNECTION))
        client.opener = bad
        with self.assertRaises(hs.HandyError) as caught:
            client._request("PUT", "/hsp/setup", {})
        self.assertNotIn(SECRET_CONNECTION, str(caught.exception))

    def test_stop_before_setup_is_fine_but_other_device_errors_still_raise(self):
        not_set_up = {"error": {"code": 100004, "name": "Illegal state", "message": "Mode specific setup required first."}}
        client = hs.HspClient(SECRET_API, SECRET_CONNECTION, opener=QueueOpener(Response(not_set_up)), emit=lambda _: None)
        client.stop()  # HSP was never set up: nothing is playing, so this is not an error
        client.opener = QueueOpener(Response({"error": {"code": 1001, "name": "Device not connected"}}))
        with self.assertRaises(hs.HandyError) as caught:
            client.stop()
        self.assertEqual(caught.exception.code, 1001)
        client.opener = QueueOpener(Response(not_set_up))
        with self.assertRaises(hs.HandyError) as caught:
            client._request("PUT", "/hsp/setup", {})
        self.assertIn("PUT /hsp/setup", str(caught.exception))
        self.assertIn("100004 Illegal state", str(caught.exception))

    def test_a_handy_that_is_not_connected_at_launch_is_waited_for_not_given_up_on(self):
        told, slept = [], []

        class Offline:
            def __init__(self, failures):
                self.failures = list(failures)
                self.tries = 0

            def clock_sync(self):
                self.tries += 1
                if self.failures:
                    raise self.failures.pop(0)

        off = hs.HandyError("Handy returned a device error on GET /hstp/clocksync?s=true: 1001 Device not connected", code=1001)
        client = Offline([off, off, off])
        hs.wait_for_handy(client, emit=told.append, sleep=slept.append)
        self.assertEqual((client.tries, slept), (4, [3.0, 3.0, 3.0]))
        self.assertEqual(len(told), 2, told)  # says it is waiting once, and when it got through
        self.assertIn("not connected", told[0])
        self.assertIn("is connected", told[1])
        # a real problem (a refused key, say) still ends the bridge at once
        refused = Offline([hs.HandyError("Handy request failed with HTTP 401")])
        with self.assertRaises(hs.HandyError):
            hs.wait_for_handy(refused, emit=told.append, sleep=slept.append)
        self.assertEqual(refused.tries, 1)
        # and a Handy that is there needs no words
        quiet = []
        hs.wait_for_handy(Offline([]), emit=quiet.append, sleep=slept.append)
        self.assertEqual(quiet, [])

    def test_a_handy_error_is_reported_and_the_bridge_starts_over_after_a_pause(self):
        with tempfile.TemporaryDirectory() as temp:
            video = Path(temp) / "movie.mp4"
            (Path(temp) / "movie.funscript").write_text(json.dumps({"actions": [{"at": i * 1000, "pos": i} for i in range(20)]}))
            now = [0.0]
            messages = []
            failures = [hs.HandyError("Handy returned a device error on PUT /hsp/setup: 1001 Device not connected", code=1001)]

            class Flaky(FakeClient):
                def setup(self):
                    if failures:
                        raise failures.pop()
                    super().setup()

            client = Flaky()
            bridge = hs.Bridge(client, emit=messages.append, monotonic=lambda: now[0])
            msg = {"seq": 1, "video": str(video), "t": 0, "playing": True, "rate": 1.0}
            self.assertFalse(bridge.process_safely(msg))  # fails, is reported, does not raise
            self.assertTrue(any(m.startswith("Handy error, starting over in 3 s") and "Device not connected" in m for m in messages))
            now[0] = 1.0
            self.assertFalse(bridge.process_safely({**msg, "seq": 2}))  # still inside the pause: ignored
            self.assertNotIn(("setup",), client.calls)
            now[0] = 4.0
            self.assertTrue(bridge.process_safely({**msg, "seq": 3}))  # starts over and works
            self.assertIn(("setup",), client.calls)
            self.assertTrue(any(c[0] == "play" for c in client.calls))

    def test_set_stroke_sends_fractions_to_the_slider_stroke_endpoint(self):
        opener = QueueOpener(Response({"result": {}}))
        client = hs.HspClient(SECRET_API, SECRET_CONNECTION, opener=opener, emit=lambda _: None)
        client.set_stroke(0.2, 0.8)
        request = opener.requests[-1]
        self.assertTrue(request.full_url.endswith("/slider/stroke"))
        self.assertEqual(request.get_method(), "PUT")
        self.assertEqual(json.loads(request.data), {"min": 0.2, "max": 0.8})

    def test_validate_message_keeps_a_good_haptics_block_and_drops_a_bad_one(self):
        base = {"v": 1, "seq": 1, "video": "/tmp/a.mp4", "t": 0, "playing": True, "rate": 1.0}
        good = {"offset_ms": -120, "stroke_min": 20, "stroke_max": 80, "invert": True}
        self.assertEqual(hs.validate_message(json.dumps({**base, "haptics": good}).encode())["haptics"], good)
        self.assertIsNone(hs.validate_message(json.dumps(base).encode())["haptics"], "an older player sends none")
        for bad in ({**good, "stroke_min": 90}, {**good, "offset_ms": 99999}, {**good, "invert": 1},
                    {**good, "stroke_max": 100.5}, {**good, "stroke_min": 50, "stroke_max": 50}, "x"):
            with self.subTest(bad=bad):
                message = hs.validate_message(json.dumps({**base, "haptics": bad}).encode())
                self.assertIsNotNone(message, "the position is still good")
                self.assertIsNone(message["haptics"])

    def test_haptics_set_stroke_once_then_offset_and_invert_resync_playback(self):
        with tempfile.TemporaryDirectory() as temp:
            video = Path(temp) / "movie.mp4"
            other = Path(temp) / "other.mp4"
            for name in ("movie", "other"):
                (Path(temp) / f"{name}.funscript").write_text(
                    json.dumps({"actions": [{"at": i * 1000, "pos": i * 5} for i in range(20)]}))
            client, messages = FakeClient(), []
            bridge = hs.Bridge(client, emit=messages.append, monotonic=lambda: 0.0)
            h = {"offset_ms": 0, "stroke_min": 20, "stroke_max": 80, "invert": False}
            msg = {"seq": 1, "video": str(video), "t": 0, "playing": True, "rate": 1.0, "haptics": h}
            bridge.process(msg)
            self.assertEqual([c for c in client.calls if c[0] == "stroke"], [("stroke", 0.2, 0.8)])
            self.assertTrue(any(m.startswith("haptics: offset +0 ms, stroke 20-80%, invert off") for m in messages))

            # Offset: stroke is not sent again; playback stops, refills and plays again at the shifted script time.
            client.calls.clear()
            bridge.process({**msg, "seq": 2, "t": 100, "haptics": {**h, "offset_ms": 500}})
            self.assertEqual([c[0] for c in client.calls if c[0] in ("stop", "flush", "play", "stroke")],
                             ["stop", "flush", "play"])
            self.assertEqual([c for c in client.calls if c[0] == "play"][0][1], 600)

            # Invert: the script in memory is flipped and playback re-synced; turning it off flips it back.
            client.calls.clear()
            bridge.process({**msg, "seq": 3, "t": 200, "haptics": {**h, "offset_ms": 500, "invert": True}})
            self.assertEqual(bridge.actions[3]["pos"], 100 - 15)
            self.assertIn(("flush",), client.calls)
            bridge.process({**msg, "seq": 4, "t": 300, "haptics": {**h, "offset_ms": 500, "invert": True}})
            bridge.process({**msg, "seq": 5, "t": 400, "haptics": {**h, "offset_ms": 500, "invert": False}})
            self.assertEqual(bridge.actions[3]["pos"], 15)

            # A video opened while inverted is loaded inverted; a new stroke range is sent.
            client.calls.clear()
            inverted = {**h, "invert": True, "stroke_min": 10}
            bridge.process({**msg, "seq": 6, "video": str(other), "t": 0, "haptics": inverted})
            self.assertEqual(bridge.actions[3]["pos"], 100 - 15)
            self.assertIn(("stroke", 0.1, 0.8), client.calls)

    # ── auto mode ──────────────────────────────────────────────────────────────

    @staticmethod
    def _strokes(actions):
        """(amplitude, seconds, speed in units/s) for every move between two points."""
        out = []
        for a, b in zip(actions, actions[1:]):
            dt = (b["at"] - a["at"]) / 1000.0
            if b["pos"] != a["pos"]:
                out.append((abs(b["pos"] - a["pos"]), dt, abs(b["pos"] - a["pos"]) / dt))
        return out

    def test_auto_generator_is_repeatable_and_never_asks_more_than_the_handy_can_do(self):
        data = ha.load_presets()
        cap = data["handy_max_speed"]
        self.assertGreaterEqual(len(data["presets"]), 4)
        for p in data["presets"]:
            with self.subTest(preset=p["id"]):
                params = ha.validate_params(ha.defaults(p), data)
                self.assertIsNotNone(params)
                a = ha.generate(data, params, 5, 5 * 60 * 1000)
                self.assertEqual(a, ha.generate(data, params, 5, 5 * 60 * 1000), "same seed, same script")
                self.assertNotEqual(a, ha.generate(data, params, 6, 5 * 60 * 1000))
                self.assertGreaterEqual(a[-1]["at"], 5 * 60 * 1000)
                self.assertEqual(a[0]["at"], 0)
                if p["id"] != "random":  # a random script starts in its first scene's own stroke window
                    self.assertEqual(a[0], {"at": 0, "pos": 0})
                self.assertTrue(all(y["at"] > x["at"] for x, y in zip(a, a[1:])), "times strictly increase")
                self.assertTrue(all(0 <= x["pos"] <= 100 and type(x["pos"]) is int for x in a))
                fast = [sp for _, _, sp in self._strokes(a) if sp > cap * 1.08]  # a little room for whole-number rounding
                self.assertEqual(fast, [], "no stroke faster than the Handy's top speed")

    def test_auto_generator_honours_the_sliders(self):
        data = ha.load_presets()
        cap = data["handy_max_speed"]
        base = ha.defaults(ha.preset(data, "tease"))
        until = 10 * 60 * 1000

        window = ha.generate(data, ha.validate_params({**base, "stroke": [20, 80]}, data), 1, until)
        self.assertTrue(all(20 <= x["pos"] <= 80 for x in window), "patterns stay inside the stroke range")

        slow = ha.generate(data, ha.validate_params({**base, "speed": [20, 30]}, data), 1, until)
        speeds = sorted(sp for _, _, sp in self._strokes(slow))
        self.assertLess(speeds[len(speeds) // 2], cap * 0.35, "the speed range slows every pattern down")

        long_only = ha.generate(data, ha.validate_params({**base, "min_stroke": 70}, data), 1, until)
        amps = sorted(a for a, _, _ in self._strokes(long_only))
        self.assertGreaterEqual(amps[len(amps) // 10], 60, "(almost) no stroke shorter than the minimum")

        def holds(actions):
            return [(b["at"] - a["at"]) / 1000.0 for a, b in zip(actions, actions[1:])
                    if a["pos"] == b["pos"] and b["at"] - a["at"] >= 1000]
        self.assertEqual(holds(ha.generate(data, ha.validate_params({**base, "pause_s": [0, 0]}, data), 1, until)), [],
                         "pause range 0-0 means no pauses")
        pauses = holds(ha.generate(data, ha.validate_params({**base, "pause_s": [2, 3]}, data), 1, until))
        self.assertTrue(pauses and all(1.9 <= v <= 3.1 for v in pauses), pauses[:5])

    def test_auto_params_are_checked(self):
        data = ha.load_presets()
        good = ha.defaults(ha.preset(data, "steady"))
        self.assertIsNotNone(ha.validate_params(good, data))
        for bad in ({**good, "preset": "nope"}, {**good, "speed": [50, 52]}, {**good, "speed": [50]}, {**good, "stroke": [0, 101]},
                    {**good, "min_stroke": -1}, {**good, "min_stroke": True}, {**good, "pattern_s": [5, 1]},
                    {**good, "pause_s": [0, float("nan")]}, {**good, "pause_s": "x"}, "x", None):
            with self.subTest(bad=bad):
                self.assertIsNone(ha.validate_params(bad, data))
        with tempfile.TemporaryDirectory() as temp:
            (Path(temp) / "p.json").write_text("{}")
            with self.assertRaises(RuntimeError):
                ha.load_presets(Path(temp) / "p.json")
            with self.assertRaises(RuntimeError):
                ha.load_presets(Path(temp) / "missing.json")

    def _auto_bridge(self, temp, with_script):
        video = Path(temp) / "movie.mp4"
        if with_script:
            (Path(temp) / "movie.funscript").write_text(
                json.dumps({"actions": [{"at": i * 1000, "pos": (i % 2) * 100} for i in range(30)]}))
        client, messages = FakeClient(), []
        return video, client, messages, hs.Bridge(client, emit=messages.append, monotonic=lambda: 0.0)

    @staticmethod
    def _msg(video, seq, auto, t=0, **extra):
        h = {"offset_ms": 0, "stroke_min": 0, "stroke_max": 100, "invert": False}
        if auto is not None:
            h["auto"] = auto
        return {"seq": seq, "video": str(video), "t": t, "playing": True, "rate": 1.0, "haptics": h, **extra}

    def test_auto_mode_fills_in_for_a_video_without_a_funscript(self):
        data = ha.load_presets()
        auto = {"mode": "no_script", **ha.defaults(ha.preset(data, "steady"))}
        with tempfile.TemporaryDirectory() as temp:
            video, client, messages, bridge = self._auto_bridge(temp, with_script=False)
            bridge.process(self._msg(video, 1, auto))
            self.assertGreater(len(bridge.actions), 1000)
            self.assertIn(("setup",), client.calls)
            self.assertTrue(any(c[0] == "add" for c in client.calls), "points were sent to the Handy")
            self.assertTrue(any(c[0] == "play" for c in client.calls))
            self.assertTrue(any(m.startswith("auto mode: steady (no funscript), seed ") for m in messages), messages)

    def test_random_style_changes_style_stays_in_the_limits_and_follows_its_seed(self):
        data = ha.load_presets()
        params = {**ha.defaults(ha.preset(data, "random")), "speed": [30.0, 80.0], "stroke": [10.0, 90.0], "pause_s": [0.0, 0.0]}
        hour = 3600 * 1000
        scenes = ha.random_scenes(data, params, 777, hour)
        self.assertGreaterEqual(len(scenes), 14, "scenes of 1 to 4 minutes fill the hour")
        self.assertGreaterEqual(len({sc["preset"] for sc, _, _ in scenes}), 4, "it visits several styles")
        self.assertTrue(all(a[0]["preset"] != b[0]["preset"] for a, b in zip(scenes, scenes[1:])), "never the same style twice in a row")
        for sc, seconds, _ in scenes:
            self.assertTrue(60 <= seconds <= 240)
            self.assertTrue(30 <= sc["speed"][0] and sc["speed"][1] <= 80 and sc["speed"][1] - sc["speed"][0] >= 5, sc["speed"])
            self.assertTrue(10 <= sc["stroke"][0] and sc["stroke"][1] <= 90 and sc["stroke"][1] - sc["stroke"][0] >= 5, sc["stroke"])
            self.assertEqual(sc["pause_s"], [0.0, 0.0], "no pauses was asked for")
        self.assertEqual(ha.validate_params({**params, "preset": "random"}, data) is not None, True, "the page's values pass the bridge's check")

        script = ha.generate(data, params, 777, hour)
        self.assertEqual(script, ha.generate(data, params, 777, hour), "the same seed is the same script")
        self.assertNotEqual(script, ha.generate(data, params, 778, hour), "another seed is another script")
        self.assertGreaterEqual(script[-1]["at"], hour)
        cap = data["handy_max_speed"]
        for a, b in zip(script, script[1:]):
            self.assertGreater(b["at"], a["at"])
            self.assertLessEqual(abs(b["pos"] - a["pos"]) / ((b["at"] - a["at"]) / 1000.0), cap * 0.801 * 1.25 + 8,
                                 "no stroke, and no join between two scenes, asks for more than the Handy's top speed")
        self.assertTrue(all(0 <= a["pos"] <= 100 for a in script))

    def test_a_pinned_seed_repeats_the_script_and_a_video_without_one_gets_its_own(self):
        data = ha.load_presets()
        base = {"mode": "no_script", **ha.defaults(ha.preset(data, "random"))}
        with tempfile.TemporaryDirectory() as temp:
            video, client, messages, bridge = self._auto_bridge(temp, with_script=False)
            bridge.process(self._msg(video, 1, {**base, "seed": 424242}))
            first = list(bridge.actions)
            self.assertTrue(any(m.endswith("seed 424242") for m in messages), messages)
            bridge.process(self._msg(video, 2, {**base, "seed": 515151}, t=500))
            self.assertNotEqual(bridge.actions, first, "a new pinned seed rebuilds the script at once")
            bridge.process(self._msg(video, 3, {**base, "seed": 424242}, t=1000))
            self.assertEqual(bridge.actions, first, "the same pinned seed is the same script")
            bridge.process(self._msg(video, 4, {**base, "seed": 0}, t=1500))
            self.assertTrue(any("seed " in m and not m.endswith(("424242", "515151")) for m in messages[-4:]), messages[-4:])

    def test_auto_mode_still_works_when_the_videos_folder_cannot_be_read(self):
        # A Windows folder WSL answers with "Input/output error" (seen 2026-11-20 on a G: path): that ended the bridge.
        auto = {"mode": "no_script", **ha.defaults(ha.preset(ha.load_presets(), "steady"))}
        with tempfile.TemporaryDirectory() as temp:
            video, client, messages, bridge = self._auto_bridge(temp, with_script=False)
            real = hs.Path.is_file
            def broken(self):
                if self.suffix == ".funscript":
                    raise OSError(5, "Input/output error", str(self))
                return real(self)
            hs.Path.is_file = broken
            try:
                bridge.process(self._msg(video, 1, auto))
            finally:
                hs.Path.is_file = real
            self.assertGreater(len(bridge.actions), 1000, "auto mode took over")
            self.assertTrue(any("cannot look for a funscript" in m and "Input/output error" in m for m in messages), messages)

    def test_auto_mode_modes_decide_between_the_funscript_and_generated_strokes(self):
        data = ha.load_presets()
        params = ha.defaults(ha.preset(data, "steady"))
        with tempfile.TemporaryDirectory() as temp:
            video, client, messages, bridge = self._auto_bridge(temp, with_script=True)
            bridge.process(self._msg(video, 1, {"mode": "no_script", **params}))
            self.assertEqual(len(bridge.actions), 30, "no_script: the funscript wins when there is one")

            # switching to "always" while playing: generated strokes replace it and playback re-syncs at once
            client.calls.clear()
            bridge.process(self._msg(video, 2, {"mode": "always", **params}, t=100))
            self.assertGreater(len(bridge.actions), 1000)
            self.assertEqual([c[0] for c in client.calls if c[0] in ("stop", "flush", "play")], ["stop", "flush", "play"])

            # and back to off: the funscript is back
            client.calls.clear()
            bridge.process(self._msg(video, 3, {"mode": "off", **params}, t=200))
            self.assertEqual(len(bridge.actions), 30)
            self.assertIn(("flush",), client.calls)

            # a player that sends no auto block at all behaves as before
            bridge.process(self._msg(video, 4, None, t=300))
            self.assertEqual(len(bridge.actions), 30)

    def test_auto_mode_with_unusable_settings_is_off_and_says_so(self):
        data = ha.load_presets()
        with tempfile.TemporaryDirectory() as temp:
            video, client, messages, bridge = self._auto_bridge(temp, with_script=False)
            bridge.process(self._msg(video, 1, {"mode": "always", **{**ha.defaults(ha.preset(data, "steady")), "preset": "nope"}}))
            self.assertEqual(bridge.actions, [])
            self.assertTrue(any("not valid" in m for m in messages), messages)
            self.assertTrue(any("idle" in m for m in messages))

    def test_auto_mode_turned_on_for_a_video_that_is_already_idle_sets_up_the_handy(self):
        data = ha.load_presets()
        with tempfile.TemporaryDirectory() as temp:
            video, client, messages, bridge = self._auto_bridge(temp, with_script=False)
            bridge.process(self._msg(video, 1, None))
            self.assertEqual(bridge.actions, [])
            client.calls.clear()
            bridge.process(self._msg(video, 2, {"mode": "no_script", **ha.defaults(ha.preset(data, "flutter"))}, t=500))
            self.assertGreater(len(bridge.actions), 1000)
            self.assertEqual(client.calls[0], ("setup",))
            self.assertTrue(any(c[0] == "play" for c in client.calls))

    def test_dry_run_logs_redacted_exact_headers_without_socket(self):
        logs = []
        client = hs.DryClient(emit=logs.append)
        client.api_key, client.connection_key, client.bearer_token = SECRET_API, SECRET_CONNECTION, ""
        client.secrets = (SECRET_API, SECRET_CONNECTION)
        with mock.patch.object(hs.socket, "socket", side_effect=AssertionError("network socket opened")):
            client._request("PUT", "/hsp/setup", {})
        self.assertIn("X-Api-Key", logs[0])
        self.assertIn("X-Connection-Key", logs[0])
        self.assertNotIn(SECRET_API, logs[0])
        self.assertNotIn(SECRET_CONNECTION, logs[0])

    def test_redirect_handler_refuses_redirects_and_fixed_origin(self):
        handler = hs.NoRedirect()
        request = __import__("urllib.request").request.Request("https://www.handyfeeling.com/")
        self.assertIsNone(handler.redirect_request(request, io.BytesIO(), 302, "found", {}, "https://evil.invalid/"))
        with self.assertRaises(hs.HandyError):
            hs.HspClient(base_url="https://evil.invalid/api")


class MockHspHandler(BaseHTTPRequestHandler):
    calls = []
    rate_hits = 0

    def log_message(self, fmt, *args):
        return

    def _reply(self, data, status=200):
        raw = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("X-RateLimit-Limit", "240")
        self.send_header("X-RateLimit-Remaining", "200")
        self.send_header("X-RateLimit-Reset", "60")
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        type(self).calls.append(("GET", self.path, dict(self.headers), None))
        if self.path == "/servertime":
            self._reply({"server_time": 1000})
        else:
            self._reply({"result": {"clock_offset": 0, "rtd": 1}})

    def do_PUT(self):
        size = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(size)) if size else None
        type(self).calls.append(("PUT", self.path, dict(self.headers), body))
        if self.path == "/rate-test" and type(self).rate_hits == 0:
            type(self).rate_hits += 1
            raw = b'{"error":{"code":429}}'
            self.send_response(429)
            self.send_header("Retry-After", "1")
            self.send_header("X-RateLimit-Remaining", "0")
            self.send_header("X-RateLimit-Reset", "1")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)
            return
        if self.path == "/rate-test":
            self._reply({"result": "ok"})
            return
        if self.path == "/hsp/setup":
            self._reply({"result": {"max_points": 100, "tail_point_stream_index": 0}})
        else:
            self._reply({"result": {}})


def _local_server():
    MockHspHandler.calls = []
    MockHspHandler.rate_hits = 0
    server = ThreadingHTTPServer(("127.0.0.1", 0), MockHspHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread


class LocalMockTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server, cls.thread = _local_server()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join(timeout=2)

    def setUp(self):
        MockHspHandler.calls = []
        MockHspHandler.rate_hits = 0

    def test_real_urllib_transport_to_mock_server(self):
        root = "http://127.0.0.1:%d" % self.server.server_port
        client = hs.HspClient(SECRET_API, SECRET_CONNECTION, base_url=root)
        client.sync_server_time(samples=1)
        client.clock_sync()
        client.setup()
        client.add([{"at": i, "pos": i % 100} for i in range(205)])
        client.play(10, 1.0)
        self.assertEqual([len(c[3]["points"]) for c in MockHspHandler.calls if c[1] == "/hsp/add"], [100, 100, 5])
        self.assertEqual(MockHspHandler.calls[0][1], "/servertime")
        auth_call = next(c for c in MockHspHandler.calls if c[1] == "/hsp/setup")
        self.assertEqual(auth_call[2]["X-Api-Key"], SECRET_API)
        self.assertEqual(auth_call[2]["X-Connection-Key"], SECRET_CONNECTION)

    def test_mock_server_rate_limit_retries_after_backoff(self):
        root = "http://127.0.0.1:%d" % self.server.server_port
        sleeps = []
        client = hs.HspClient(SECRET_API, SECRET_CONNECTION, base_url=root, sleep=sleeps.append)
        client._request("PUT", "/rate-test", {})
        self.assertEqual(len(sleeps), 1)
        self.assertAlmostEqual(sleeps[0], 1.0, places=2)
        self.assertEqual(sum(c[1] == "/rate-test" for c in MockHspHandler.calls), 2)

    def test_bridge_transitions_seek_pause_resume_and_new_file_on_mock(self):
        root = "http://127.0.0.1:%d" % self.server.server_port
        now = [0.0]
        client = hs.HspClient(SECRET_API, SECRET_CONNECTION, base_url=root,
                              clock=lambda: 1000.0 + now[0], sleep=lambda _: None)
        bridge = hs.Bridge(client, offset_ms=25, monotonic=lambda: now[0])
        with tempfile.TemporaryDirectory() as temp:
            video = Path(temp) / "movie.mp4"
            Path(temp, "movie.funscript").write_text(json.dumps({"actions": [
                {"at": i * 500, "pos": i % 101} for i in range(50)
            ]}))
            bridge.process({"seq": 1, "video": str(video), "t": 0, "playing": True, "rate": 1.0})
            now[0] = 0.25
            bridge.process({"seq": 2, "video": str(video), "t": 250, "playing": False, "rate": 1.0})
            now[0] = 0.5
            bridge.process({"seq": 3, "video": str(video), "t": 250, "playing": True, "rate": 1.0})
            now[0] = 0.75
            bridge.process({"seq": 4, "video": str(video), "t": 5000, "playing": True, "rate": 1.0})
            paths = [call[1] for call in MockHspHandler.calls]
            self.assertIn("/hsp/pause", paths)
            self.assertIn("/hsp/resume", paths)
            self.assertIn("/hsp/flush", paths)
            self.assertGreaterEqual(paths.count("/hsp/add"), 2)
            play = [call[3] for call in MockHspHandler.calls if call[1] == "/hsp/play"][-1]
            self.assertEqual(play["start_time"], 5025)
            other = Path(temp) / "other.mp4"
            Path(temp, "other.funscript").write_text(json.dumps({"actions": [{"at": 0, "pos": 0}, {"at": 1000, "pos": 100}]}))
            before = paths.count("/hsp/setup")
            now[0] = 1.0
            bridge.process({"seq": 5, "video": str(other), "t": 0, "playing": True, "rate": 1.0})
            self.assertGreater(sum(c[1] == "/hsp/setup" for c in MockHspHandler.calls), before)

    def test_missing_and_bad_scripts_idle_once_against_mock(self):
        root = "http://127.0.0.1:%d" % self.server.server_port
        client = hs.HspClient(SECRET_API, SECRET_CONNECTION, base_url=root)
        messages = []
        bridge = hs.Bridge(client, emit=messages.append)
        with tempfile.TemporaryDirectory() as temp:
            missing = str(Path(temp) / "missing.mp4")
            bridge.process({"seq": 1, "video": missing, "t": 0, "playing": False, "rate": 1.0})
            bridge.process({"seq": 2, "video": missing, "t": 0, "playing": False, "rate": 1.0})
            bad = Path(temp) / "bad.mp4"
            Path(temp, "bad.funscript").write_text("{")
            bridge.process({"seq": 3, "video": str(bad), "t": 0, "playing": False, "rate": 1.0})
            bridge.process({"seq": 4, "video": str(bad), "t": 0, "playing": False, "rate": 1.0})
        self.assertEqual(sum("No funscript" in m for m in messages), 1)
        self.assertEqual(sum("could not read funscript" in m.lower() and "idle" in m.lower() for m in messages), 1)


def run(offline=False):
    if offline:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(HandySyncTests)
        result_stream = unittest.TextTestRunner(verbosity=2).run(suite)
        return 0 if result_stream.wasSuccessful() else 1
    try:
        server, thread = _local_server()
    except OSError as exc:
        print("BLOCKED: mock HSP server cannot bind loopback in this sandbox (%s)" % type(exc).__name__)
        print("Run tools/handy-sync selftest --offline for socket-free coverage.")
        return 2
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)
    suite = unittest.TestSuite()
    suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(HandySyncTests))
    suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(LocalMockTests))
    result_stream = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result_stream.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(run("--offline" in sys.argv))
