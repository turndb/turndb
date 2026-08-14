import json
import asyncio
import pathlib
import tempfile
import unittest

from bindings.python.tests.test_conformance import ROOT, turndb


class TraceMappingTest(unittest.TestCase):
    def test_shared_mapping_vector(self):
        vector = json.loads((ROOT / "conformance" / "v1" / "trace-mapping.json").read_text())
        record = turndb.map_normalized_span(vector["span"])
        self.assertEqual(record["id"], vector["expected"]["id"])
        self.assertEqual([attribute["name"] for attribute in record["attrs"]], vector["expected"]["attributeNames"])
        self.assertEqual(
            {content["name"]: content["base64"] for content in record["contents"]},
            vector["expected"]["contents"],
        )

    def test_refuses_noncanonical_ids_and_unordered_time(self):
        vector = json.loads((ROOT / "conformance" / "v1" / "trace-mapping.json").read_text())
        with self.assertRaises(ValueError):
            turndb.map_normalized_span({**vector["span"], "traceId": vector["span"]["traceId"].upper()})
        with self.assertRaises(ValueError):
            turndb.map_normalized_span({**vector["span"], "endTimeUnixNano": "1"})

    def test_exporter_writes_a_reader_visible_local_file(self):
        vector = json.loads((ROOT / "conformance" / "v1" / "trace-mapping.json").read_text())

        class Context:
            trace_id = int(vector["span"]["traceId"], 16)
            span_id = int(vector["span"]["spanId"], 16)

        class Named:
            name = "INTERNAL"

        class Status:
            status_code = Named()
            description = None

        class Span:
            name = vector["span"]["name"]
            parent = None
            kind = Named()
            start_time = int(vector["span"]["startTimeUnixNano"])
            end_time = int(vector["span"]["endTimeUnixNano"])
            status = Status()
            attributes = {"agent.framework": "test"}
            events = []
            links = []

            @staticmethod
            def get_span_context():
                return Context()

        with tempfile.TemporaryDirectory(prefix="turndb-python-otel-") as root:
            path = str(pathlib.Path(root) / "agent.turndb")
            exporter = turndb.TurnDbSpanExporter(path, flush_every_spans=1)
            self.assertEqual(exporter.export([Span()]), turndb.otel.SpanExportResult.SUCCESS)
            exporter.shutdown()
            snapshot = turndb.Snapshot.open(path)
            try:
                page = snapshot.scan({"contractVersion": 1, "limit": 10, "attrs": ["otel.name"]})
                self.assertEqual([row["id"] for row in page["rows"]], [vector["expected"]["id"]])
            finally:
                snapshot.close()

    def test_thin_gen_ai_wrappers_preserve_sync_and_async_client_behavior(self):
        observed = {}

        class Span:
            def set_attribute(self, name, value):
                observed.setdefault("output", {})[name] = value

            def set_status(self, status):
                observed["status"] = status

            def record_exception(self, error):
                observed["exception"] = error

        class Scope:
            def __enter__(self):
                return Span()

            def __exit__(self, *_):
                observed["exits"] = observed.get("exits", 0) + 1

        class Tracer:
            def start_as_current_span(self, name, **options):
                observed["name"] = name
                observed["options"] = options
                return Scope()

        response = {"output": [{"role": "assistant", "content": "hello"}]}
        result = turndb.trace_gen_ai_call(
            Tracer(),
            lambda: response,
            operation_name="chat",
            provider_name="test",
            model="small",
            input_messages=[{"role": "user", "content": "hi"}],
            output_messages=lambda value: value["output"],
        )
        self.assertIs(result, response)
        self.assertEqual(observed["name"], "chat small")
        self.assertEqual(observed["options"]["attributes"]["gen_ai.provider.name"], "test")
        self.assertEqual(
            observed["options"]["attributes"]["gen_ai.input.messages"],
            '[{"content":"hi","role":"user"}]',
        )
        self.assertEqual(
            observed["output"]["gen_ai.output.messages"],
            '[{"content":"hello","role":"assistant"}]',
        )
        self.assertEqual(observed["exits"], 1)

        async def invoke():
            async def call():
                return response

            return await turndb.trace_gen_ai_call_async(
                Tracer(), call, operation_name="chat", output_messages=lambda value: value["output"]
            )

        self.assertIs(asyncio.run(invoke()), response)
        self.assertEqual(observed["exits"], 2)


if __name__ == "__main__":
    unittest.main()
