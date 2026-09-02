import importlib.machinery
import importlib.util
import json
import pathlib
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[3]
PYTHON = ROOT / "bindings" / "python" / "python"
# Cargo names the cdylib per platform; none of these is a suffix importlib recognises as an
# extension module, so the loader is named explicitly rather than inferred from the file name.
CDYLIB = {"linux": "lib_native.so", "darwin": "lib_native.dylib", "win32": "_native.dll"}
if sys.platform not in CDYLIB:
    raise RuntimeError(f"no cdylib name known for {sys.platform}")
NATIVE = ROOT / "target" / "debug" / CDYLIB[sys.platform]

sys.path.insert(0, str(PYTHON))
spec = importlib.util.spec_from_file_location(
    "turndb._native",
    NATIVE,
    loader=importlib.machinery.ExtensionFileLoader("turndb._native", str(NATIVE)),
)
native = importlib.util.module_from_spec(spec)
sys.modules["turndb._native"] = native
spec.loader.exec_module(native)

import turndb  # noqa: E402

CONF = ROOT / "conformance" / "v1"
CORPUS = json.loads((CONF / "corpus.json").read_text())
CAPABILITY_SCHEMA = json.loads((CONF / "capabilities.schema.json").read_text())


def view_for(source):
    views = [view for view in CORPUS["views"] if view["source"] == source]
    assert len(views) == 1
    return views[0]


def operation(record):
    return {
        "kind": "put",
        "id": record["id"],
        "attrs": record["attrs"],
        "contents": [
            {"name": content["name"], "base64": content["base64"]}
            for content in record["contents"]
        ],
    }


def assert_projected(test, row, expected, request):
    selected_attrs = set(request.get("attrs", []))
    test.assertEqual(
        row["attrs"],
        [attribute for attribute in expected["attrs"] if attribute["name"] in selected_attrs],
    )
    selected_contents = request.get("contents", [])
    test.assertEqual(len(row["contents"]), len(selected_contents))
    for projected, selected in zip(row["contents"], selected_contents):
        content = next(
            (candidate for candidate in expected["contents"] if candidate["name"] == selected["name"]),
            None,
        )
        test.assertEqual(projected["name"], selected["name"])
        test.assertEqual(projected["present"], content is not None)
        if content is None:
            test.assertNotIn("base64", projected)
        else:
            if selected["mode"] == "bytes":
                test.assertEqual(projected["base64"], content["base64"])
            else:
                test.assertNotIn("base64", projected)


def assert_query(test, handle, source, query):
    pages = []
    ids = []
    cursor = None
    while True:
        request = dict(query["request"])
        if cursor is not None:
            request["cursor"] = cursor
        page = handle.scan(request)
        pages.append(page)
        ids.extend(row["id"] for row in page["rows"])
        expected = {record["id"]: record for record in view_for(source)["records"]}
        for row in page["rows"]:
            assert_projected(test, row, expected[row["id"]], query["request"])
        cursor = page.get("next") if query.get("paginate") else None
        if cursor is None:
            break
    test.assertEqual(ids, query["expectedIds"], query["name"])

    if query.get("assertMetadataOnlyIo"):
        for page in pages:
            test.assertEqual(page["stats"]["io"]["foldBlocksTouched"], "0")
            test.assertEqual(page["stats"]["io"]["foldStoredBytesRead"], "0")
            test.assertEqual(page["stats"]["reconstructedBytes"], "0")
    if query["name"] == "content-budget-refuses-to-truncate":
        test.assertTrue(all(len(page["rows"]) == 1 for page in pages))
    if query.get("assertCursorDamageRejected"):
        first = handle.scan(query["request"])
        damaged = first["next"][:-1] + ("B" if first["next"].endswith("A") else "A")
        with test.assertRaises(turndb.InvalidArgumentError):
            handle.scan({**query["request"], "cursor": damaged})
    if query.get("assertCursorMismatchRejected"):
        first = handle.scan(query["request"])
        with test.assertRaises(turndb.InvalidArgumentError):
            handle.scan({**query["request"], "direction": "forward", "cursor": first["next"]})


def assert_source(test, handle, source):
    view = view_for(source)
    attrs = sorted({attribute["name"] for record in view["records"] for attribute in record["attrs"]})
    contents = sorted({content["name"] for record in view["records"] for content in record["contents"]})
    page = handle.scan(
        {
            "contractVersion": 1,
            "limit": 100,
            "attrs": attrs,
            "contents": [{"name": name, "mode": "bytes"} for name in contents],
        }
    )
    test.assertEqual([row["id"] for row in page["rows"]], [record["id"] for record in view["records"]])
    expected = {record["id"]: record for record in view["records"]}
    for row in page["rows"]:
        test.assertEqual(row["attrs"], expected[row["id"]]["attrs"])
    for query in [query for query in CORPUS["queries"] if query["source"] == source]:
        assert_query(test, handle, source, query)


class ConformanceTest(unittest.TestCase):
    def test_capabilities(self):
        profile = turndb.capabilities()
        for field in CAPABILITY_SCHEMA["required"]:
            self.assertIn(field, profile)
        self.assertEqual(profile["contractVersion"], 1)
        self.assertEqual(profile["profile"], "native")
        self.assertEqual(profile["binding"], "python")
        self.assertEqual(profile["writerExclusion"], "os_enforced")
        self.assertIn("scan", profile["operations"])

    def test_writer_snapshots_and_queries_replay_shared_corpus(self):
        with tempfile.TemporaryDirectory(prefix="turndb-python-conformance-") as root:
            path = str(pathlib.Path(root) / "fixture.turndb")
            store = turndb.Store.open(path)
            snapshots = []
            try:
                with self.assertRaises(turndb.InvalidArgumentError):
                    store.write(
                        [
                            {
                                "kind": "put",
                                "id": "bad-float",
                                "attrs": [
                                    {
                                        "name": "float",
                                        "value": {"type": "f64", "bitsHex": "7FF8000000000001"},
                                    }
                                ],
                                "contents": [],
                            }
                        ]
                    )
                for step in CORPUS["steps"]:
                    if step["action"] == "apply":
                        store.write(
                            [operation(record) for record in step["puts"]]
                            + [{"kind": "delete", "id": item} for item in step["deletes"]]
                        )
                    elif step["action"] == "sync":
                        store.sync()
                    elif step["action"] == "flush":
                        store.flush()
                    elif step["action"] == "captureSnapshot":
                        snapshot = store.snapshot()
                        snapshots.append(snapshot)
                        assert_source(self, snapshot, step["name"])
                    elif step["action"] == "assertWriter":
                        assert_source(self, store, step["name"])
                assert_source(self, snapshots[0], "snapshot-v1")
                assert_source(self, snapshots[1], "snapshot-v2")
                self.assertEqual(store.verify()["state"], "valid")
                self.assertGreater(int(store.space_usage()["total"]["logicalBytes"]), 0)
                sealed_path = str(pathlib.Path(root) / "sealed.turndb")
                sealed = store.seal(sealed_path)
                self.assertGreater(int(sealed["bytes"]), 0)
                sealed_snapshot = turndb.Snapshot.open(sealed_path)
                try:
                    assert_source(self, sealed_snapshot, "snapshot-v2")
                finally:
                    sealed_snapshot.close()
            finally:
                for snapshot in snapshots:
                    snapshot.close()
                store.close(durable=True)

    def test_checked_in_physical_fixture(self):
        with tempfile.TemporaryDirectory(prefix="turndb-python-fixture-") as root:
            target = pathlib.Path(root) / "fixture.turndb"
            target.write_bytes(bytes.fromhex((CONF / "fixture.turndb.hex").read_text()))
            snapshot = turndb.Snapshot.open(str(target))
            try:
                assert_source(self, snapshot, "snapshot-v2")
            finally:
                snapshot.close()


if __name__ == "__main__":
    unittest.main()
