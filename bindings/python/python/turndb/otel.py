"""OpenTelemetry span mapping and exporter for TurnDB."""

from __future__ import annotations

import base64
import json
import struct
import threading
import time
from typing import Any, Awaitable, Callable, Iterable, Mapping, TypeVar

from ._native import Store

try:
    from opentelemetry.trace import SpanKind, Status, StatusCode
except ImportError:  # Client-call wrappers also remain usable with a duck-typed tracer.
    SpanKind = None
    Status = None
    StatusCode = None

try:
    from opentelemetry.sdk.trace.export import SpanExportResult
except ImportError:  # The store SDK does not force the OTel SDK into every environment.
    class SpanExportResult:
        SUCCESS = 0
        FAILURE = 1


CONTENT_ATTRIBUTES = {
    "gen_ai.system_instructions",
    "gen_ai.input.messages",
    "gen_ai.output.messages",
    "gen_ai.tool.definitions",
}
KIND = ["INTERNAL", "SERVER", "CLIENT", "PRODUCER", "CONSUMER"]
STATUS = ["UNSET", "OK", "ERROR"]


def _time_ns(value: Any) -> int:
    if isinstance(value, str):
        value = int(value)
    elif isinstance(value, (tuple, list)) and len(value) == 2:
        value = int(value[0]) * 1_000_000_000 + int(value[1])
    elif not isinstance(value, int):
        raise TypeError("span time must be int, decimal text, or (seconds, nanoseconds)")
    return value


def _scalar(value: Any) -> dict[str, Any]:
    if isinstance(value, str):
        return {"type": "string", "value": value}
    if isinstance(value, bool):
        return {"type": "bool", "value": value}
    if isinstance(value, int):
        return {"type": "i64", "decimal": str(value)}
    if isinstance(value, float):
        bits = struct.unpack(">Q", struct.pack(">d", value))[0]
        return {"type": "f64", "bitsHex": f"{bits:016x}"}
    if isinstance(value, bytes):
        return {"type": "binary", "base64": base64.b64encode(value).decode("ascii")}
    if value is None:
        return {"type": "null"}
    raise TypeError(f"OpenTelemetry attribute is not a scalar: {type(value).__name__}")


def _attribute(name: str, value: Any) -> dict[str, Any]:
    return {"name": name, "value": _scalar(value)}


def _stable_json(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def _hex(value: Any, length: int, field: str) -> str:
    if not isinstance(value, str) or len(value) != length or any(c not in "0123456789abcdef" for c in value):
        raise ValueError(f"{field} must be {length} lowercase hexadecimal digits")
    return value


def map_normalized_span(span: Mapping[str, Any]) -> dict[str, Any]:
    """Map the language-neutral trace-contract shape to one canonical Tier-1 write."""
    trace_id = _hex(span["traceId"], 32, "traceId")
    span_id = _hex(span["spanId"], 16, "spanId")
    parent = span.get("parentSpanId")
    if parent:
        parent = _hex(parent, 16, "parentSpanId")
    start = _time_ns(span["startTimeUnixNano"])
    end = _time_ns(span["endTimeUnixNano"])
    if start < 0 or end < start:
        raise ValueError("span times must be non-negative and ordered")
    status = span.get("status") or {}
    attrs = [
        _attribute("otel.trace_id", trace_id),
        _attribute("otel.span_id", span_id),
        *([_attribute("otel.parent_span_id", parent)] if parent else []),
        _attribute("otel.name", str(span["name"])),
        _attribute("otel.kind", str(span.get("kind", "INTERNAL"))),
        {"name": "otel.start_time_unix_nano", "value": {"type": "timestampNs", "decimal": str(start)}},
        {"name": "otel.end_time_unix_nano", "value": {"type": "timestampNs", "decimal": str(end)}},
        _attribute("otel.duration_ns", end - start),
        _attribute("otel.status.code", str(status.get("code", "UNSET"))),
        *([_attribute("otel.status.message", str(status["message"]))] if status.get("message") else []),
    ]
    contents: list[dict[str, str]] = []
    attributes = span.get("attributes") or []
    pairs = attributes.items() if isinstance(attributes, Mapping) else attributes
    for name, value in pairs:
        if name in CONTENT_ATTRIBUTES:
            payload = value.encode() if isinstance(value, str) else _stable_json(value)
            contents.append({"name": name, "base64": base64.b64encode(payload).decode("ascii")})
        elif isinstance(value, (tuple, list)):
            attrs.extend(_attribute(name, item) for item in value)
        else:
            attrs.append(_attribute(name, value))
    if "events" in span:
        contents.append({"name": "otel.events", "base64": base64.b64encode(_stable_json(span["events"])).decode("ascii")})
    if "links" in span:
        contents.append({"name": "otel.links", "base64": base64.b64encode(_stable_json(span["links"])).decode("ascii")})
    return {
        "kind": "put",
        "id": f"span/{trace_id}/{start:020d}/{span_id}",
        "attrs": attrs,
        "contents": contents,
    }


def _plain(value: Any) -> Any:
    if hasattr(value, "__dict__"):
        return {key: _plain(item) for key, item in vars(value).items() if not key.startswith("_")}
    if isinstance(value, Mapping):
        return {key: _plain(item) for key, item in value.items()}
    if isinstance(value, (tuple, list)):
        return [_plain(item) for item in value]
    return value


def map_readable_span(span: Any) -> dict[str, Any]:
    context = span.get_span_context()
    parent = getattr(span, "parent", None)
    kind = getattr(span, "kind", "INTERNAL")
    code = getattr(getattr(span, "status", None), "status_code", "UNSET")
    return map_normalized_span(
        {
            "traceId": f"{context.trace_id:032x}",
            "spanId": f"{context.span_id:016x}",
            "parentSpanId": f"{parent.span_id:016x}" if parent else None,
            "name": span.name,
            "kind": getattr(kind, "name", KIND[kind] if isinstance(kind, int) and kind < len(KIND) else str(kind)),
            "startTimeUnixNano": span.start_time,
            "endTimeUnixNano": span.end_time,
            "status": {
                "code": getattr(code, "name", STATUS[code] if isinstance(code, int) and code < len(STATUS) else str(code)),
                "message": getattr(getattr(span, "status", None), "description", None),
            },
            "attributes": dict(span.attributes or {}),
            "events": [_plain(event) for event in span.events],
            "links": [_plain(link) for link in span.links],
        }
    )


T = TypeVar("T")


def _call_attributes(
    operation_name: str,
    provider_name: str | None,
    model: str | None,
    input_messages: Any,
    attributes: Mapping[str, Any] | None,
) -> dict[str, Any]:
    if not isinstance(operation_name, str) or not operation_name:
        raise TypeError("operation_name must be a non-empty string")
    out = dict(attributes or {})
    out["gen_ai.operation.name"] = operation_name
    if provider_name is not None:
        out["gen_ai.provider.name"] = provider_name
    if model is not None:
        out["gen_ai.request.model"] = model
    if input_messages is not None:
        out["gen_ai.input.messages"] = (
            input_messages if isinstance(input_messages, str) else _stable_json(input_messages).decode()
        )
    return out


def _finish_call_span(span: Any, selected: Any) -> None:
    if selected is not None:
        span.set_attribute(
            "gen_ai.output.messages",
            selected if isinstance(selected, str) else _stable_json(selected).decode(),
        )
    if Status is not None:
        span.set_status(Status(StatusCode.OK))


def _fail_call_span(span: Any, error: BaseException) -> None:
    if hasattr(span, "record_exception"):
        span.record_exception(error)
    if Status is not None:
        span.set_status(Status(StatusCode.ERROR, str(error)))


def trace_gen_ai_call(
    tracer: Any,
    call: Callable[[], T],
    *,
    operation_name: str,
    provider_name: str | None = None,
    model: str | None = None,
    span_name: str | None = None,
    input_messages: Any = None,
    output_messages: Any | Callable[[T], Any] = None,
    attributes: Mapping[str, Any] | None = None,
) -> T:
    """Run one synchronous provider-client call inside a canonical gen_ai CLIENT span."""
    span_attributes = _call_attributes(operation_name, provider_name, model, input_messages, attributes)
    name = span_name or f"{operation_name}{'' if model is None else f' {model}'}"
    kwargs = {"attributes": span_attributes}
    if SpanKind is not None:
        kwargs["kind"] = SpanKind.CLIENT
    kwargs["record_exception"] = False
    kwargs["set_status_on_exception"] = False
    with tracer.start_as_current_span(name, **kwargs) as span:
        try:
            result = call()
            selected = output_messages(result) if callable(output_messages) else output_messages
            _finish_call_span(span, selected)
            return result
        except BaseException as error:
            _fail_call_span(span, error)
            raise


async def trace_gen_ai_call_async(
    tracer: Any,
    call: Callable[[], Awaitable[T]],
    *,
    operation_name: str,
    provider_name: str | None = None,
    model: str | None = None,
    span_name: str | None = None,
    input_messages: Any = None,
    output_messages: Any | Callable[[T], Any] = None,
    attributes: Mapping[str, Any] | None = None,
) -> T:
    """Run one asynchronous provider-client call inside a canonical gen_ai CLIENT span."""
    span_attributes = _call_attributes(operation_name, provider_name, model, input_messages, attributes)
    name = span_name or f"{operation_name}{'' if model is None else f' {model}'}"
    kwargs = {"attributes": span_attributes}
    if SpanKind is not None:
        kwargs["kind"] = SpanKind.CLIENT
    kwargs["record_exception"] = False
    kwargs["set_status_on_exception"] = False
    with tracer.start_as_current_span(name, **kwargs) as span:
        try:
            result = await call()
            selected = output_messages(result) if callable(output_messages) else output_messages
            _finish_call_span(span, selected)
            return result
        except BaseException as error:
            _fail_call_span(span, error)
            raise


class TurnDbSpanExporter:
    """Duck-types OpenTelemetry's SpanExporter without making it a required dependency."""

    def __init__(
        self,
        path_or_store: str | Store,
        *,
        durable_every_export: bool = True,
        flush_every_spans: int = 512,
        flush_interval_seconds: float = 5.0,
    ) -> None:
        self._store = Store.open(path_or_store) if isinstance(path_or_store, str) else path_or_store
        self._owns_store = isinstance(path_or_store, str)
        self._durable = durable_every_export
        self._flush_every = flush_every_spans
        self._flush_interval = flush_interval_seconds
        self._pending = 0
        self._last_flush = time.monotonic()
        self._lock = threading.Lock()
        self._stopped = False

    def export(self, spans: Iterable[Any]) -> Any:
        try:
            with self._lock:
                if self._stopped:
                    return SpanExportResult.FAILURE
                operations = [map_readable_span(span) for span in spans]
                self._store.write(operations, durable=self._durable)
                self._pending += len(operations)
                if self._pending >= self._flush_every or time.monotonic() - self._last_flush >= self._flush_interval:
                    if not self._durable:
                        self._store.sync()
                    self._store.flush()
                    self._pending = 0
                    self._last_flush = time.monotonic()
            return SpanExportResult.SUCCESS
        except Exception:
            return SpanExportResult.FAILURE

    def force_flush(self, timeout_millis: int = 30_000) -> bool:
        del timeout_millis
        with self._lock:
            self._store.sync()
            self._store.flush()
            self._pending = 0
            self._last_flush = time.monotonic()
        return True

    def shutdown(self) -> None:
        with self._lock:
            if self._stopped:
                return
            self._store.sync()
            self._store.flush()
            if self._owns_store:
                self._store.close(durable=True)
            self._stopped = True
