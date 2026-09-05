# General scalar field types

TurnDB attributes are an ordered list of `(name, value)` pairs. Names may repeat, order is preserved,
and one name may carry multiple types across or within records. Physical columns are homogeneous by
`(name, type)`; the row layout reconstructs the original ordered list.

The exhaustive value set is:

| value | Rust | JavaScript/Python boundary |
|---|---|---|
| UTF-8 string | `AttrValue::Str` | string |
| signed 64-bit integer | `AttrValue::Int` | bigint / integer |
| IEEE-754 binary64 | `AttrValue::Float` | number plus exact-bit lane where required |
| boolean | `AttrValue::Bool` | boolean |
| unsigned 64-bit integer | `AttrValue::UInt` | bigint / non-negative integer |
| arbitrary bytes | `AttrValue::Bytes` | Buffer/Uint8Array / bytes |
| UTC Unix nanoseconds | `AttrValue::TimestampNs` | signed exact integer |
| explicit null | `AttrValue::Null` | null |

Missing and explicit null are different. Exact integers never pass through JavaScript `number`.
Floats persist and return their raw bits, preserving negative zero and NaN payloads. Bindings refuse
contradictory dual representations rather than guessing.

The wire tags and widths are normative in [`FORMAT.md`](../FORMAT.md#attributes).
