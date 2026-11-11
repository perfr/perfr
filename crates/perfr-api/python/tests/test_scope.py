# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""What name each form of ``perfr.scope`` records.

Runs without ``libperfr``: the calls that would reach it are replaced by
ones that note what they were given.

    cd crates/perfr-api/python && python3 -m unittest discover -s tests
"""

import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

import perfr_api as perfr  # noqa: E402


class ScopeNames(unittest.TestCase):
    def setUp(self):
        self.started = []
        self.stopped = []

        def start(kind):
            def record(name):
                self.started.append((kind, name))
                return len(self.started)

            return record

        for target, stub in (
            ("start", start("sync")),
            ("start_async", start("async")),
            ("stop", self.stopped.append),
        ):
            patcher = mock.patch.object(perfr, target, stub)
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_a_bare_decorator_records_the_function_s_qualified_name(self):
        @perfr.scope
        def render(frame):
            return frame * 2

        self.assertEqual(render(21), 42)
        self.assertEqual(self.started, [("sync", b"ScopeNames.test_a_bare_decorator_records_the_function_s_qualified_name.<locals>.render")])
        self.assertEqual(self.stopped, [1])
        self.assertEqual(render.__name__, "render", "the wrapper passes for the function")

    def test_a_decorator_called_without_a_name_does_too(self):
        @perfr.scope()
        def update():
            pass

        update()
        self.assertEqual(self.started[0][1].rsplit(b".", 1)[-1], b"update")

    def test_a_bare_async_decorator_records_the_qualified_name_on_the_async_call(self):
        @perfr.scope_async
        def job():
            pass

        job()
        self.assertEqual(self.started[0][0], "async")
        self.assertEqual(self.started[0][1].rsplit(b".", 1)[-1], b"job")

    def test_a_named_decorator_records_the_name_given(self):
        @perfr.scope("render pass")
        def render():
            pass

        render()
        render()
        self.assertEqual(self.started, [("sync", b"render pass")] * 2)
        self.assertEqual(self.stopped, [1, 2])

    def test_a_decorated_function_that_raises_still_stops_its_scope(self):
        @perfr.scope
        def fails():
            raise ValueError("no")

        with self.assertRaises(ValueError):
            fails()
        self.assertEqual(self.stopped, [1])

    def test_a_context_manager_records_its_name_and_keeps_the_handle(self):
        with perfr.scope("update") as s:
            self.assertEqual(s.handle, 1)
        with perfr.scope(b"bytes are passed through"):
            pass
        self.assertEqual(self.started, [("sync", b"update"), ("sync", b"bytes are passed through")])
        self.assertEqual(self.stopped, [1, 2])

    def test_a_context_manager_without_a_name_records_an_empty_one(self):
        # There is no function to name it after.
        with perfr.scope():
            pass
        self.assertEqual(self.started, [("sync", b"")])


if __name__ == "__main__":
    unittest.main()
