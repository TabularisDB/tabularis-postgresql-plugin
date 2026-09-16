//! Value extraction from tokio-postgres rows to serde_json::Value.
//!
//! Replicates the exact type mapping of the built-in driver's
//! `src-tauri/src/drivers/postgres/extract/` system. Every PG type must
//! produce byte-identical JSON to the builtin — the parity tests enforce this.

use std::collections::HashMap;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use tokio_postgres::types::{FromSql, Kind, Type};
use tokio_postgres::Row;
use uuid::Uuid;

/// JavaScript's Number.MAX_SAFE_INTEGER (2^53 - 1).
pub(crate) const JS_MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// Mirror of [`JS_MAX_SAFE_INTEGER`] for unsigned values (XID8's wire format
/// is unsigned). Matches the builtin driver's `common/safe_int.rs::JS_MAX_SAFE_UINT`.
const JS_MAX_SAFE_UINT: u64 = 9_007_199_254_740_991;

/// Extract a single column value from a row as a JSON value.
/// Matches the builtin driver's extraction behavior exactly.
///
/// Dispatches on `ty.kind()` first — Simple/Enum/Array/Range — mirroring the
/// builtin driver's `extract/mod.rs` structure, so new types slot into the
/// bucket that matches how the builtin organizes `extract/simple.rs`,
/// `extract/array.rs`, `extract/range.rs`, etc. Each bucket below still runs
/// the exact same `Type::` equality checks (and falls through to the same
/// string-or-null fallback) the single flat match used before this
/// restructure — this change is purely structural.
pub fn extract_value(row: &Row, index: usize) -> JsonValue {
    let col_type = row.columns()[index].type_().clone();

    match col_type.kind() {
        Kind::Simple => extract_simple_kind(&col_type, row, index),
        Kind::Enum(_) => try_extract::<EnumLabel>(row, index, |v| JsonValue::String(v.0)),
        Kind::Array(_) => extract_array_kind(&col_type, row, index),
        Kind::Range(_) => extract_range_kind(&col_type, row, index),
        Kind::Multirange(_) => try_extract::<MultirangeValue>(row, index, |v| v.0),
        // Domain types unwrap to their base type and recurse — a domain
        // over int4 decodes exactly like a plain int4 column. Matches the
        // builtin's `Kind::Domain(ty) => simple::extract_or_null(ty, raw)`.
        Kind::Domain(ref base) => extract_simple_kind(base, row, index),
        Kind::Composite(_) => try_extract::<CompositeValue>(row, index, |v| v.0),
        // Anything not yet modeled above falls through to the same
        // string-or-null fallback every unmatched type used before this
        // restructure.
        _ => extract_string_or_null_fallback(row, index),
    }
}

/// `Kind::Simple` bucket: scalar types with no element/subtype (everything
/// except enum/array/range/composite/domain/multirange). Every arm here is
/// unchanged from the pre-restructure flat match.
fn extract_simple_kind(col_type: &Type, row: &Row, index: usize) -> JsonValue {
    match *col_type {
        ref t if *t == Type::BOOL => try_extract::<bool>(row, index, JsonValue::Bool),
        ref t if *t == Type::INT2 => try_extract::<i16>(row, index, JsonValue::from),
        ref t if *t == Type::INT4 => try_extract::<i32>(row, index, JsonValue::from),
        ref t if *t == Type::INT8 => try_extract::<i64>(row, index, i64_to_json),
        ref t if *t == Type::FLOAT4 => try_extract::<f32>(row, index, |v| {
            serde_json::Number::from_f64(v as f64)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null)
        }),
        ref t if *t == Type::FLOAT8 => try_extract::<f64>(row, index, |v| {
            serde_json::Number::from_f64(v)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null)
        }),
        ref t if *t == Type::NUMERIC => {
            try_extract::<Decimal>(row, index, |v| JsonValue::String(v.to_string()))
        }
        ref t
            if *t == Type::TEXT
                || *t == Type::VARCHAR
                || *t == Type::BPCHAR
                || *t == Type::NAME =>
        {
            try_extract::<String>(row, index, JsonValue::String)
        }
        ref t if *t == Type::UUID => {
            try_extract::<Uuid>(row, index, |v| JsonValue::String(v.to_string()))
        }
        ref t if *t == Type::DATE => try_extract::<NaiveDate>(row, index, |v| {
            JsonValue::String(v.format("%Y-%m-%d").to_string())
        }),
        ref t if *t == Type::TIME => try_extract::<NaiveTime>(row, index, |v| {
            JsonValue::String(v.format("%H:%M:%S").to_string())
        }),
        ref t if *t == Type::TIMETZ => try_extract::<TimeTz>(row, index, JsonValue::from),
        ref t if *t == Type::INTERVAL => try_extract::<Interval>(row, index, JsonValue::from),
        ref t if *t == Type::TIMESTAMP => try_extract::<NaiveDateTime>(row, index, |v| {
            JsonValue::String(v.format("%Y-%m-%d %H:%M:%S").to_string())
        }),
        ref t if *t == Type::TIMESTAMPTZ => {
            try_extract::<chrono::DateTime<chrono::Utc>>(row, index, |v| {
                JsonValue::String(v.format("%Y-%m-%d %H:%M:%S").to_string())
            })
        }
        ref t if *t == Type::JSON || *t == Type::JSONB => {
            try_extract::<serde_json::Value>(row, index, |v| v)
        }
        ref t if *t == Type::BYTEA => try_extract::<Vec<u8>>(row, index, |v| {
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &v);
            JsonValue::String(format!("BLOB:{}:application/octet-stream:{}", v.len(), b64))
        }),
        ref t if *t == Type::INET || *t == Type::CIDR => {
            try_extract::<CidrOrInet>(row, index, JsonValue::from)
        }
        ref t if *t == Type::MACADDR => try_extract::<MacAddr>(row, index, JsonValue::from),
        ref t if *t == Type::MACADDR8 => try_extract::<MacAddr8>(row, index, JsonValue::from),
        ref t if *t == Type::BIT || *t == Type::VARBIT => {
            try_extract::<BitOrVarBit>(row, index, JsonValue::from)
        }
        ref t if *t == Type::XID => try_extract::<Xid>(row, index, JsonValue::from),
        ref t if *t == Type::CID => try_extract::<Cid>(row, index, JsonValue::from),
        ref t if *t == Type::TID => try_extract::<Tid>(row, index, JsonValue::from),
        ref t if *t == Type::XID8 => try_extract::<Xid8>(row, index, JsonValue::from),
        ref t if *t == Type::REGPROC => try_extract::<RegProc>(row, index, JsonValue::from),
        ref t if *t == Type::REGPROCEDURE => {
            try_extract::<RegProcedure>(row, index, JsonValue::from)
        }
        ref t if *t == Type::REGOPER => try_extract::<RegOper>(row, index, JsonValue::from),
        ref t if *t == Type::REGOPERATOR => try_extract::<RegOperator>(row, index, JsonValue::from),
        ref t if *t == Type::REGCLASS => try_extract::<RegClass>(row, index, JsonValue::from),
        ref t if *t == Type::REGTYPE => try_extract::<RegType>(row, index, JsonValue::from),
        ref t if *t == Type::REGCONFIG => try_extract::<RegConfig>(row, index, JsonValue::from),
        ref t if *t == Type::REGDICTIONARY => {
            try_extract::<RegDictionary>(row, index, JsonValue::from)
        }
        ref t if *t == Type::REGNAMESPACE => {
            try_extract::<RegNamespace>(row, index, JsonValue::from)
        }
        ref t if *t == Type::REGROLE => try_extract::<RegRole>(row, index, JsonValue::from),
        ref t if *t == Type::REGCOLLATION => {
            try_extract::<RegCollation>(row, index, JsonValue::from)
        }
        ref t if *t == Type::OID => try_extract::<u32>(row, index, JsonValue::from),
        ref t if *t == Type::MONEY => try_extract::<Money>(row, index, JsonValue::from),
        ref t if *t == Type::POINT => try_extract::<Point>(row, index, JsonValue::from),
        ref t if *t == Type::LSEG => try_extract::<Lseg>(row, index, JsonValue::from),
        ref t if *t == Type::BOX => try_extract::<PgBox>(row, index, JsonValue::from),
        ref t if *t == Type::POLYGON => try_extract::<Polygon>(row, index, JsonValue::from),
        ref t if *t == Type::PATH => try_extract::<Path>(row, index, JsonValue::from),
        ref t if *t == Type::LINE => try_extract::<Line>(row, index, JsonValue::from),
        ref t if *t == Type::CIRCLE => try_extract::<Circle>(row, index, JsonValue::from),
        ref t if *t == Type::XML => try_extract::<Xml>(row, index, JsonValue::from),
        ref t if *t == Type::REFCURSOR => try_extract::<RefCursor>(row, index, JsonValue::from),
        ref t if *t == Type::PG_NODE_TREE => try_extract::<PgNodeTree>(row, index, JsonValue::from),
        ref t if *t == Type::JSONPATH => try_extract::<JsonPath>(row, index, JsonValue::from),
        ref t if *t == Type::TS_VECTOR => try_extract::<TsVector>(row, index, JsonValue::from),
        ref t if *t == Type::TSQUERY => try_extract::<TsQuery>(row, index, JsonValue::from),
        ref t if *t == Type::GTS_VECTOR => try_extract::<GtsVector>(row, index, JsonValue::from),
        ref t if *t == Type::PG_LSN => try_extract::<PgLsn>(row, index, JsonValue::from),
        ref t if *t == Type::TXID_SNAPSHOT || *t == Type::PG_SNAPSHOT => {
            try_extract::<TxidSnapshotOrPgSnapshot>(row, index, JsonValue::from)
        }
        ref t if *t == Type::PG_MCV_LIST => try_extract::<PgMcvList>(row, index, JsonValue::from),
        ref t if *t == Type::PG_DEPENDENCIES => {
            try_extract::<PgDependencies>(row, index, JsonValue::from)
        }
        ref t if *t == Type::PG_NDISTINCT => {
            try_extract::<PgNdistinct>(row, index, JsonValue::from)
        }
        ref t if *t == Type::PG_BRIN_BLOOM_SUMMARY => {
            try_extract::<PgBrinBloomSummary>(row, index, JsonValue::from)
        }
        ref t if *t == Type::PG_BRIN_MINMAX_MULTI_SUMMARY => {
            try_extract::<PgBrinMinmaxMultiSummary>(row, index, JsonValue::from)
        }
        // pgvector types have dynamic OIDs (no `Type::` constant exists),
        // so they must be matched by name rather than by equality above —
        // same reasoning as the `hstore` arm just below.
        ref t if t.name() == "vector" => try_extract::<PgVector>(row, index, JsonValue::from),
        ref t if t.name() == "halfvec" => try_extract::<PgHalfVector>(row, index, JsonValue::from),
        ref t if t.name() == "sparsevec" => {
            try_extract::<PgSparseVector>(row, index, JsonValue::from)
        }
        // hstore is an extension type (no well-known OID), matched by name like
        // the builtin driver's `extract/simple.rs::extract_or_null`. tokio-postgres
        // decodes it natively as HashMap<String, Option<String>>.
        ref t if t.name() == "hstore" => {
            try_extract::<HashMap<String, Option<String>>>(row, index, |v| {
                serde_json::to_value(v).unwrap_or(JsonValue::Null)
            })
        }
        _ => extract_string_or_null_fallback(row, index),
    }
}

/// `Kind::Array(_)` bucket: the eight hardcoded fast-paths (checked against
/// the outer array `Type::` constant, unchanged from the pre-restructure
/// flat match), falling back to the generic per-element decoder for any
/// other array element type (e.g. `enum[]`, `hstore[]`).
fn extract_array_kind(col_type: &Type, row: &Row, index: usize) -> JsonValue {
    match *col_type {
        ref t if *t == Type::INT2_ARRAY => try_extract::<Vec<Option<i16>>>(row, index, |v| {
            JsonValue::Array(
                v.into_iter()
                    .map(|e| e.map(JsonValue::from).unwrap_or(JsonValue::Null))
                    .collect(),
            )
        }),
        ref t if *t == Type::INT4_ARRAY => try_extract::<Vec<Option<i32>>>(row, index, |v| {
            JsonValue::Array(
                v.into_iter()
                    .map(|e| e.map(JsonValue::from).unwrap_or(JsonValue::Null))
                    .collect(),
            )
        }),
        ref t if *t == Type::INT8_ARRAY => try_extract::<Vec<Option<i64>>>(row, index, |v| {
            JsonValue::Array(
                v.into_iter()
                    .map(|e| e.map(i64_to_json).unwrap_or(JsonValue::Null))
                    .collect(),
            )
        }),
        ref t if *t == Type::TEXT_ARRAY || *t == Type::VARCHAR_ARRAY => {
            try_extract::<Vec<Option<String>>>(row, index, |v| {
                JsonValue::Array(
                    v.into_iter()
                        .map(|e| e.map(JsonValue::String).unwrap_or(JsonValue::Null))
                        .collect(),
                )
            })
        }
        ref t if *t == Type::FLOAT4_ARRAY => try_extract::<Vec<Option<f32>>>(row, index, |v| {
            JsonValue::Array(
                v.into_iter()
                    .map(|e| {
                        e.and_then(|f| serde_json::Number::from_f64(f as f64))
                            .map(JsonValue::Number)
                            .unwrap_or(JsonValue::Null)
                    })
                    .collect(),
            )
        }),
        ref t if *t == Type::FLOAT8_ARRAY => try_extract::<Vec<Option<f64>>>(row, index, |v| {
            JsonValue::Array(
                v.into_iter()
                    .map(|e| {
                        e.and_then(serde_json::Number::from_f64)
                            .map(JsonValue::Number)
                            .unwrap_or(JsonValue::Null)
                    })
                    .collect(),
            )
        }),
        ref t if *t == Type::BOOL_ARRAY => try_extract::<Vec<Option<bool>>>(row, index, |v| {
            JsonValue::Array(
                v.into_iter()
                    .map(|e| e.map(JsonValue::Bool).unwrap_or(JsonValue::Null))
                    .collect(),
            )
        }),
        // Generic fallback for arrays whose element type isn't one of the
        // hardcoded fast-paths above (int2/int4/int8/float4/float8/bool/
        // text/varchar) — e.g. enum[] or hstore[]. tokio_postgres's built-in
        // `Vec<T>: FromSql` requires a single concrete `T`, which can't
        // express "decode each element the way `extract_value` would for a
        // scalar column of that type" — so this parses the array wire
        // format directly and recurses per-element, matching the builtin
        // driver's generic `Kind::Array` dispatch (`extract/mod.rs` +
        // `extract/array.rs::try_extract_elem`).
        _ => try_extract::<ArrayValue>(row, index, |v| v.0),
    }
}

/// `Kind::Range(_)` bucket. PostgreSQL's six built-in range types are
/// enumerated explicitly (unchanged from the pre-restructure flat match)
/// rather than accepting any `Kind::Range` generically — an
/// extension-defined range type falls to the same string-or-null fallback
/// every other unhandled type does, matching pre-restructure behavior
/// exactly (broadening this to "any Kind::Range" is new-type scope, not a
/// restructure).
fn extract_range_kind(col_type: &Type, row: &Row, index: usize) -> JsonValue {
    match *col_type {
        ref t
            if *t == Type::INT4_RANGE
                || *t == Type::INT8_RANGE
                || *t == Type::NUM_RANGE
                || *t == Type::TS_RANGE
                || *t == Type::TSTZ_RANGE
                || *t == Type::DATE_RANGE =>
        {
            try_extract_range(row, index)
        }
        _ => extract_string_or_null_fallback(row, index),
    }
}

/// Fallback for any type not explicitly handled: many types have text
/// representations, so try decoding as a plain string before giving up and
/// returning `Null`. This is the single fallback path every unmatched type
/// (in any `Kind`) reaches — unchanged from the pre-restructure catch-all.
fn extract_string_or_null_fallback(row: &Row, index: usize) -> JsonValue {
    match row.try_get::<_, String>(index) {
        Ok(s) => JsonValue::String(s),
        Err(_) => JsonValue::Null,
    }
}

/// Safely convert i64 to JSON: numbers within JS safe integer range are
/// JSON numbers; larger values become JSON strings to prevent precision loss.
fn i64_to_json(v: i64) -> JsonValue {
    if v.abs() <= JS_MAX_SAFE_INTEGER {
        JsonValue::from(v)
    } else {
        JsonValue::String(v.to_string())
    }
}

/// Safely convert u64 to JSON: mirrors [`i64_to_json`] for XID8's unsigned
/// wire format. Matches the builtin driver's `common/safe_int.rs::u64_to_json`.
fn u64_to_json(v: u64) -> JsonValue {
    if v <= JS_MAX_SAFE_UINT {
        JsonValue::from(v)
    } else {
        JsonValue::String(v.to_string())
    }
}

/// Helper: try to extract a typed value from the row, returning JsonValue::Null
/// on any failure (NULL column, type mismatch, etc.).
fn try_extract<'a, T>(row: &'a Row, index: usize, map: impl FnOnce(T) -> JsonValue) -> JsonValue
where
    T: tokio_postgres::types::FromSql<'a>,
{
    match row.try_get::<_, Option<T>>(index) {
        Ok(Some(v)) => map(v),
        Ok(None) => JsonValue::Null,
        Err(_) => {
            // Type mismatch — try string fallback
            match row.try_get::<_, Option<String>>(index) {
                Ok(Some(s)) => JsonValue::String(s),
                _ => JsonValue::Null,
            }
        }
    }
}

/// Extract a range-typed column (INT4RANGE, TSRANGE, etc.) using the generic
/// `Type::kind()` dispatch (matches the builtin's `Kind::Range(subtype)`
/// handling) rather than per-range-type constants, since range subtypes are
/// resolved dynamically from the column's element type.
fn try_extract_range(row: &Row, index: usize) -> JsonValue {
    match row.try_get::<_, Option<RangeValue>>(index) {
        Ok(Some(v)) => JsonValue::String(v.0),
        Ok(None) => JsonValue::Null,
        Err(_) => JsonValue::Null,
    }
}

/// Wraps the raw range wire format: 1 flag byte, then 0-2 length-prefixed
/// bound values (each a 4-byte big-endian length followed by that many
/// bytes), formatted as `"[lower, upper)"` (bracket/paren per bound
/// inclusivity) matching `src-tauri/src/drivers/postgres/extract/range.rs`.
struct RangeValue(String);

impl<'a> FromSql<'a> for RangeValue {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let subtype = match ty.kind() {
            Kind::Range(t) => t.clone(),
            _ => return Err("expected a range type".into()),
        };
        let mut buf = raw;
        match extract_range_or_null(&subtype, &mut buf) {
            JsonValue::String(s) => Ok(Self(s)),
            _ => Err("empty range buffer".into()),
        }
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.kind(), Kind::Range(_))
    }
}

/// Decode one range value from a mutable buffer, advancing `buf` past
/// everything it consumes — the shape every caller with a shared, advancing
/// buffer needs (both the scalar `RangeValue` wrapper above and
/// `Multirange`'s per-range loop, which must call this repeatedly without
/// re-slicing from scratch each time). Matches
/// `extract/range.rs::extract_or_null` in the builtin, including its
/// `Null` return for an empty buffer (used by both callers to signal
/// "nothing left to decode").
fn extract_range_or_null(subtype: &Type, buf: &mut &[u8]) -> JsonValue {
    if buf.is_empty() {
        return JsonValue::Null;
    }
    let flag = buf[0];
    *buf = &buf[1..];

    // RANGE_EMPTY flag bit 0
    if (flag & 1) == 1 {
        return JsonValue::String("empty".to_string());
    }

    let lower_char = if (flag & (1 << 1)) == 0 { '(' } else { '[' };
    let upper_char = if (flag & (1 << 2)) == 0 { ')' } else { ']' };

    let mut out = String::new();
    out.push(lower_char);

    // RANGE_LB_INF flag bit 3 — lower bound is unbounded (nothing pushed).
    if flag & (1 << 3) == 0 {
        // A present-but-unextractable lower bound short-circuits the
        // whole range to "null, null" and returns immediately — matches
        // the builtin's early-return on lower-bound extraction failure.
        match extract_range_bound(subtype, buf) {
            Some(s) => out.push_str(&s),
            None => {
                out.push_str("null, null");
                out.push(upper_char);
                return JsonValue::String(out);
            }
        }
    }
    out.push_str(", ");

    // RANGE_UB_INF flag bit 4 — upper bound is unbounded (nothing pushed).
    if flag & (1 << 4) == 0 {
        if let Some(s) = extract_range_bound(subtype, buf) {
            out.push_str(&s);
        } else {
            out.push_str("null");
        }
    }
    out.push(upper_char);

    JsonValue::String(out)
}

/// Read one length-prefixed bound value from a range buffer and format it
/// the same way `extract_value` would for a plain column of that subtype.
fn extract_range_bound(subtype: &Type, buf: &mut &[u8]) -> Option<String> {
    if buf.len() < 4 {
        return None;
    }
    let len = i32::from_be_bytes(buf[..4].try_into().ok()?);
    *buf = &buf[4..];
    if len < 0 {
        return None;
    }
    let len = len as usize;
    if buf.len() < len {
        return None;
    }
    let (value_buf, rest) = buf.split_at(len);
    *buf = rest;

    let json = extract_simple_from_bytes(subtype, value_buf);
    match json {
        JsonValue::Null => None,
        // Matches the builtin's `range.push_str(&val.to_string())`: calling
        // `.to_string()` on a serde_json::Value quotes strings (producing
        // `"2026-01-01 00:00:00"` inside the range) but leaves numbers bare
        // (producing `1` not `"1"`) — do not special-case String here.
        other => Some(other.to_string()),
    }
}

/// MULTIRANGE (`int4multirange`, `int8multirange`, `nummultirange`,
/// `tsmultirange`, `tstzmultirange`, `datemultirange`): 4-byte range count,
/// then that many ranges, each a 4-byte length prefix followed by that
/// many bytes in the same wire format `extract_range_or_null` already
/// decodes. Formatted as `"{range1,range2,...}"` (ranges joined by a bare
/// comma, no space — distinct from the `", "` separator used *inside* each
/// range between its own bounds). Matches
/// `extract/multi_range.rs::extract_or_null`.
pub(crate) struct MultirangeValue(JsonValue);

impl<'a> FromSql<'a> for MultirangeValue {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let subtype = match ty.kind() {
            Kind::Multirange(t) => t.clone(),
            _ => return Err("expected a multirange type".into()),
        };

        if raw.len() < 4 {
            return Ok(Self(JsonValue::Null));
        }
        let count = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let mut buf = &raw[4..];

        if count == 0 {
            return Ok(Self(JsonValue::from("{}")));
        }

        let mut ranges = String::from('{');

        for _ in 0..count - 1 {
            // 4-byte length prefix ahead of each range's own bytes — skip it.
            // The builtin's loop also pushes a comma unconditionally after each
            // non-last range and relies on the next iteration starting cleanly.
            // If the next range's length prefix is missing/truncated, the comma
            // already in `ranges` would produce invalid syntax (e.g. `"{[1, 5),}"`).
            // The builtin shares this truncation-path quirk, but unlike the builtin
            // we strip the trailing comma before closing — producing a valid, parseable
            // multirange string even for truncated wire data. This is a deliberate,
            // documented deviation from the builtin in an otherwise-unreachable
            // code path (the server always sends complete, atomic wire values).
            if buf.len() < 4 {
                if ranges.ends_with(',') {
                    ranges.pop();
                }
                ranges.push('}');
                return Ok(Self(JsonValue::String(ranges)));
            }
            buf = &buf[4..];

            match extract_range_or_null(&subtype, &mut buf) {
                JsonValue::String(r) => ranges.push_str(&r),
                other => ranges.push_str(&other.to_string()),
            }
            ranges.push(',');
        }

        // The final range has no trailing comma. Strip any leftover comma from
        // a prior loop iteration in case the final length prefix is missing.
        if buf.len() < 4 {
            if ranges.ends_with(',') {
                ranges.pop();
            }
            ranges.push('}');
            return Ok(Self(JsonValue::String(ranges)));
        }
        buf = &buf[4..];

        match extract_range_or_null(&subtype, &mut buf) {
            JsonValue::String(r) => ranges.push_str(&r),
            other => ranges.push_str(&other.to_string()),
        }
        ranges.push('}');

        Ok(Self(JsonValue::String(ranges)))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.kind(), Kind::Multirange(_))
    }
}

impl From<MultirangeValue> for JsonValue {
    fn from(v: MultirangeValue) -> Self {
        v.0
    }
}

/// COMPOSITE (any user-defined `CREATE TYPE ... AS (...)` row type, e.g.
/// `pg_type`'s row type itself, or a custom struct-like type): 4-byte
/// field count (unused here — the real field names/types come from
/// `Type::fields()`, known ahead of time from the column's type metadata,
/// not from the wire), then per field a 4-byte type OID (skipped — the
/// type is already known from `Field::type_()`) and a 4-byte
/// length-prefixed value (-1 length = NULL). Decodes each field the same
/// way `extract_simple_kind`/`extract_array_kind`/etc. would for a scalar
/// column of that type. A field whose value can't be extracted (buffer
/// truncated, unsupported type) gets `null`, and every field after it
/// also gets `null` rather than aborting the whole composite — matches
/// the builtin's "extract or fill nulls" recovery behavior. Matches
/// `extract/composite.rs::extract_or_null`.
pub(crate) struct CompositeValue(JsonValue);

impl<'a> FromSql<'a> for CompositeValue {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let fields = match ty.kind() {
            Kind::Composite(fields) => fields,
            _ => return Err("expected a composite type".into()),
        };

        if raw.is_empty() {
            return Ok(Self(JsonValue::Null));
        }

        let mut buf = raw;
        Ok(Self(extract_composite_fields(fields, &mut buf)))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.kind(), Kind::Composite(_))
    }
}

impl From<CompositeValue> for JsonValue {
    fn from(v: CompositeValue) -> Self {
        v.0
    }
}

fn extract_composite_fields(fields: &[tokio_postgres::types::Field], buf: &mut &[u8]) -> JsonValue {
    let mut map = serde_json::Map::with_capacity(fields.len());

    // Skip the 4-byte field count — the real field list comes from `fields`.
    if buf.len() < 4 {
        fill_composite_nulls(fields, &mut map);
        return JsonValue::Object(map);
    }
    *buf = &buf[4..];

    for (i, field) in fields.iter().enumerate() {
        // Skip the 4-byte field type OID — already known from `field.type_()`.
        if buf.len() < 4 {
            fill_composite_nulls(&fields[i..], &mut map);
            return JsonValue::Object(map);
        }
        *buf = &buf[4..];

        match extract_composite_field_value(field.type_(), buf) {
            Some(value) => {
                map.insert(field.name().to_string(), value);
            }
            None => {
                map.insert(field.name().to_string(), JsonValue::Null);
                if i + 1 < fields.len() {
                    fill_composite_nulls(&fields[i + 1..], &mut map);
                    return JsonValue::Object(map);
                }
            }
        }
    }

    JsonValue::Object(map)
}

fn fill_composite_nulls(
    fields: &[tokio_postgres::types::Field],
    map: &mut serde_json::Map<String, JsonValue>,
) {
    for field in fields {
        map.insert(field.name().to_string(), JsonValue::Null);
    }
}

/// Extract one composite field's length-prefixed value (-1 length = NULL,
/// returned as `None` so the caller can fill it and every later field with
/// `null` per the builtin's recovery behavior — see `extract_composite_fields`).
/// Delegates to `extract_kind_from_bytes` (shared with array-element
/// decoding) for the actual per-`Kind` dispatch, rather than duplicating
/// it — a composite field's type can be any `Kind` an array element can
/// be, plus `Composite` again (nested composites) and `Range`/
/// `Multirange` (PostgreSQL allows range/multirange-typed composite
/// fields; it does NOT allow them as array element types recursively in
/// the same way, but the dispatch function handles both callers' needs
/// identically either way).
fn extract_composite_field_value(field_type: &Type, buf: &mut &[u8]) -> Option<JsonValue> {
    if buf.len() < 4 {
        return None;
    }
    let len = i32::from_be_bytes(buf[..4].try_into().ok()?);
    *buf = &buf[4..];
    if len < 0 {
        return Some(JsonValue::Null);
    }
    let len = len as usize;
    if buf.len() < len {
        return None;
    }
    let (value_buf, rest) = buf.split_at(len);
    *buf = rest;

    Some(extract_kind_from_bytes(field_type, value_buf))
}

/// Format a raw byte buffer as JSON for the subset of simple PG types that
/// can appear as range bounds in this plugin's test corpus (integers,
/// numeric, date/timestamp). Falls back to Null for anything else.
fn extract_simple_from_bytes(ty: &Type, buf: &[u8]) -> JsonValue {
    match *ty {
        Type::INT4 => i32::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        Type::INT8 => i64::from_sql(ty, buf)
            .map(i64_to_json)
            .unwrap_or(JsonValue::Null),
        Type::NUMERIC => Decimal::from_sql(ty, buf)
            .map(|v| JsonValue::String(v.to_string()))
            .unwrap_or(JsonValue::Null),
        Type::DATE => NaiveDate::from_sql(ty, buf)
            .map(|v| JsonValue::String(v.format("%Y-%m-%d").to_string()))
            .unwrap_or(JsonValue::Null),
        Type::TIMESTAMP => NaiveDateTime::from_sql(ty, buf)
            .map(|v| JsonValue::String(v.format("%Y-%m-%d %H:%M:%S").to_string()))
            .unwrap_or(JsonValue::Null),
        Type::TIMESTAMPTZ => chrono::DateTime::<chrono::Utc>::from_sql(ty, buf)
            .map(|v| JsonValue::String(v.format("%Y-%m-%d %H:%M:%S").to_string()))
            .unwrap_or(JsonValue::Null),
        _ => JsonValue::Null,
    }
}

/// Wraps the raw wire format of a 1-D Postgres array whose element type
/// isn't one of the hardcoded fast-paths in `extract_value` (e.g. `enum[]`
/// or `hstore[]`). Format: 4-byte dimension count, 4-byte has-nulls flag,
/// 4-byte element type OID, then per dimension an 8-byte (length,
/// lower_bound) pair, then the elements themselves as length-prefixed
/// values (-1 length = NULL element). Decodes each element the same way
/// `extract_value` would for a scalar column of that type, matching the
/// builtin driver's generic `Kind::Array` dispatch
/// (`extract/mod.rs` + `extract/array.rs::try_extract_elem`). Multi-
/// dimensional arrays fall back to `Null`, consistent with this file's
/// existing hardcoded array arms (which only ever handle 1-D arrays).
pub(crate) struct ArrayValue(pub(crate) JsonValue);

impl<'a> FromSql<'a> for ArrayValue {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let elem_type = match ty.kind() {
            Kind::Array(t) => t.clone(),
            _ => return Err("expected an array type".into()),
        };

        if raw.len() < 12 {
            return Err("array buffer too short for header".into());
        }
        let dimensions = i32::from_be_bytes(raw[0..4].try_into().unwrap());
        if dimensions == 0 {
            return Ok(Self(JsonValue::Array(vec![])));
        }
        if dimensions != 1 {
            // Multi-dimensional arrays aren't modeled by this decoder —
            // fall back to Null rather than misinterpreting the layout.
            return Err("multi-dimensional array not supported".into());
        }

        let mut buf = &raw[12..];
        if buf.len() < 8 {
            return Err("array buffer too short for dimension header".into());
        }
        let len = i32::from_be_bytes(buf[0..4].try_into().unwrap());
        if len < 0 {
            return Err("invalid array dimension length".into());
        }
        buf = &buf[8..]; // skip length + lower_bound

        // Don't pre-allocate based on the claimed length: it's untrusted
        // (comes straight off the wire) and a truncated/malformed buffer
        // could claim up to i32::MAX elements while containing far fewer
        // bytes, turning a single bad row into a multi-gigabyte allocation
        // before the truncation check below ever runs. `Vec::new()` grows
        // by amortized doubling as elements are actually read, so the
        // allocation stays proportional to what's really in the buffer.
        let mut elements = Vec::new();
        for _ in 0..len {
            if buf.len() < 4 {
                return Err("array buffer truncated before element length".into());
            }
            let elem_len = i32::from_be_bytes(buf[0..4].try_into().unwrap());
            buf = &buf[4..];
            if elem_len < 0 {
                elements.push(JsonValue::Null);
                continue;
            }
            let elem_len = elem_len as usize;
            if buf.len() < elem_len {
                return Err("array buffer truncated before element value".into());
            }
            let (elem_buf, rest) = buf.split_at(elem_len);
            buf = rest;
            elements.push(extract_element_from_bytes(&elem_type, elem_buf));
        }

        Ok(Self(JsonValue::Array(elements)))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.kind(), Kind::Array(_))
    }
}

/// Decode one array element's raw bytes as JSON. A thin wrapper around
/// `extract_kind_from_bytes` — kept as a separate name for readability at
/// `ArrayValue::from_sql`'s call site.
fn extract_element_from_bytes(ty: &Type, buf: &[u8]) -> JsonValue {
    extract_kind_from_bytes(ty, buf)
}

/// Decode a raw byte buffer as JSON for *any* `Kind` — the shared dispatcher
/// behind both `ArrayValue`'s per-element decoding and
/// `CompositeValue`'s per-field decoding. A value nested inside an array or
/// a composite can be any of these `Kind`s (PostgreSQL allows composite
/// fields of array/range/multirange/domain/nested-composite type; it does
/// NOT allow an array's element type to itself be an array, but the
/// dispatch is identical either way, so one function serves both callers).
/// Mirrors the builtin's `try_extract_elem`/`try_extract_field`, which are
/// themselves near-identical `Kind` dispatches for the same reason.
fn extract_kind_from_bytes(ty: &Type, buf: &[u8]) -> JsonValue {
    match ty.kind() {
        Kind::Range(subtype) => {
            let mut inner_buf = buf;
            extract_range_or_null(subtype, &mut inner_buf)
        }
        Kind::Multirange(_) => MultirangeValue::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        Kind::Domain(base) => extract_kind_from_bytes(base, buf),
        Kind::Composite(fields) => {
            let mut inner_buf = buf;
            extract_composite_fields(fields, &mut inner_buf)
        }
        // Array-of-array is not a real PostgreSQL element type (multi-
        // dimensional arrays use a different wire encoding entirely, and
        // ArrayValue::from_sql already rejects `dimensions != 1` for
        // that), but dispatch here defensively rather than assuming a
        // caller never passes one.
        Kind::Array(_) => ArrayValue::from_sql(ty, buf)
            .map(|v| v.0)
            .unwrap_or(JsonValue::Null),
        _ => extract_simple_kind_from_bytes(ty, buf),
    }
}

/// `Kind::Simple` + `Kind::Enum` + hstore-by-name dispatch, from raw bytes
/// with no `Row`/`index` available (used by array elements and composite
/// fields, both of which only ever hand this function a length-delimited
/// value slice, not a whole row). Covers the exact same type set as
/// `extract_simple_kind` (the `Row`-based scalar-column dispatcher) — kept
/// as a separate function because every arm here calls `T::from_sql`
/// directly against a byte slice rather than `try_extract`'s
/// `row.try_get::<_, Option<T>>`.
fn extract_simple_kind_from_bytes(ty: &Type, buf: &[u8]) -> JsonValue {
    match ty {
        _ if *ty == Type::BOOL => bool::from_sql(ty, buf)
            .map(JsonValue::Bool)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::INT2 => i16::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::OID => u32::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::FLOAT4 => f32::from_sql(ty, buf)
            .map(|v| {
                serde_json::Number::from_f64(v as f64)
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null)
            })
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::FLOAT8 => f64::from_sql(ty, buf)
            .map(|v| {
                serde_json::Number::from_f64(v)
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null)
            })
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::TEXT
            || *ty == Type::VARCHAR
            || *ty == Type::BPCHAR
            || *ty == Type::NAME =>
        {
            String::from_sql(ty, buf)
                .map(JsonValue::String)
                .unwrap_or(JsonValue::Null)
        }
        _ if *ty == Type::UUID => Uuid::from_sql(ty, buf)
            .map(|v| JsonValue::String(v.to_string()))
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::TIMETZ => TimeTz::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::INTERVAL => Interval::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::JSON || *ty == Type::JSONB => {
            serde_json::Value::from_sql(ty, buf).unwrap_or(JsonValue::Null)
        }
        _ if *ty == Type::BYTEA => Vec::<u8>::from_sql(ty, buf)
            .map(|v| {
                let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &v);
                JsonValue::String(format!("BLOB:{}:application/octet-stream:{}", v.len(), b64))
            })
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::INET || *ty == Type::CIDR => CidrOrInet::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::MACADDR => MacAddr::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::MACADDR8 => MacAddr8::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::BIT || *ty == Type::VARBIT => BitOrVarBit::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::XID => Xid::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::CID => Cid::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::TID => Tid::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::XID8 => Xid8::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REGPROC => RegProc::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REGPROCEDURE => RegProcedure::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REGOPER => RegOper::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REGOPERATOR => RegOperator::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REGCLASS => RegClass::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REGTYPE => RegType::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REGCONFIG => RegConfig::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REGDICTIONARY => RegDictionary::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REGNAMESPACE => RegNamespace::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REGROLE => RegRole::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REGCOLLATION => RegCollation::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::MONEY => Money::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::POINT => Point::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::LSEG => Lseg::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::BOX => PgBox::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::POLYGON => Polygon::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::PATH => Path::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::LINE => Line::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::CIRCLE => Circle::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::XML => Xml::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::REFCURSOR => RefCursor::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::PG_NODE_TREE => PgNodeTree::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::JSONPATH => JsonPath::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::TS_VECTOR => TsVector::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::TSQUERY => TsQuery::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::GTS_VECTOR => GtsVector::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::PG_LSN => PgLsn::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::TXID_SNAPSHOT || *ty == Type::PG_SNAPSHOT => {
            TxidSnapshotOrPgSnapshot::from_sql(ty, buf)
                .map(JsonValue::from)
                .unwrap_or(JsonValue::Null)
        }
        _ if *ty == Type::PG_MCV_LIST => PgMcvList::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::PG_DEPENDENCIES => PgDependencies::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::PG_NDISTINCT => PgNdistinct::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::PG_BRIN_BLOOM_SUMMARY => PgBrinBloomSummary::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if *ty == Type::PG_BRIN_MINMAX_MULTI_SUMMARY => {
            PgBrinMinmaxMultiSummary::from_sql(ty, buf)
                .map(JsonValue::from)
                .unwrap_or(JsonValue::Null)
        }
        _ if matches!(ty.kind(), Kind::Enum(_)) => EnumLabel::from_sql(ty, buf)
            .map(|v| JsonValue::String(v.0))
            .unwrap_or(JsonValue::Null),
        // pgvector types have dynamic OIDs (no `Type::` constant exists),
        // so they must be matched by name rather than by equality above.
        _ if ty.name() == "vector" => PgVector::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if ty.name() == "halfvec" => PgHalfVector::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if ty.name() == "sparsevec" => PgSparseVector::from_sql(ty, buf)
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
        _ if ty.name() == "hstore" => HashMap::<String, Option<String>>::from_sql(ty, buf)
            .map(|v| serde_json::to_value(v).unwrap_or(JsonValue::Null))
            .unwrap_or(JsonValue::Null),
        _ => extract_simple_from_bytes(ty, buf),
    }
}

/// PostgreSQL enum wire format is just the label's UTF-8 bytes — no length
/// prefix, no OID-checked decoding. Matches
/// `src-tauri/src/drivers/postgres/extract/enum.rs::extract_or_null`.
pub(crate) struct EnumLabel(pub(crate) String);

impl<'a> FromSql<'a> for EnumLabel {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(std::str::from_utf8(raw)?.to_string()))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.kind(), Kind::Enum(_))
    }
}

/// MONEY: total value in cents (or the smallest fractional unit of the
/// database's locale). Wire format is identical to INT8 — a big-endian
/// i64 — so decoding just reinterprets those bytes and reuses `i64_to_json`
/// for the same JS-safe-integer stringification `INT8` gets. Matches
/// `src-tauri/src/drivers/postgres/extract/advanced_types.rs::Money`.
pub(crate) struct Money(i64);

impl<'a> FromSql<'a> for Money {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(<i64 as FromSql>::from_sql(&Type::INT8, raw)?))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::MONEY
    }
}

impl From<Money> for JsonValue {
    fn from(value: Money) -> Self {
        i64_to_json(value.0)
    }
}

/// TIMETZ: time-of-day + UTC offset. Wire format: 8-byte microseconds since
/// midnight (i64, always non-negative), then a 4-byte signed offset in
/// seconds (positive = west of UTC, hence the sign flip below). Matches
/// `src-tauri/src/drivers/postgres/extract/advanced_types.rs::TimeTz`.
struct TimeTz {
    hrs: u8,
    mins: u8,
    secs: u8,
    microseconds: u32,
    offset_sign: char,
    offset_hrs: u8,
    offset_mins: u8,
    offset_secs: u8,
}

impl<'a> FromSql<'a> for TimeTz {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 12 {
            return Err(format!("expected at least 12 bytes for TIMETZ, got {}", raw.len()).into());
        }
        let mut microseconds = i64::from_be_bytes(raw[0..8].try_into().unwrap());
        if microseconds < 0 {
            return Err("microseconds must not be negative for TIMETZ".into());
        }
        let hrs = (microseconds / (1_000_000 * 60 * 60)) as u8;
        microseconds %= 1_000_000 * 60 * 60;
        let mins = (microseconds / (1_000_000 * 60)) as u8;
        microseconds %= 1_000_000 * 60;
        let secs = (microseconds / 1_000_000) as u8;
        let microseconds = (microseconds % 1_000_000) as u32;

        let mut timezone_offset = i32::from_be_bytes(raw[8..12].try_into().unwrap());
        let offset_sign = if timezone_offset.is_positive() {
            '-'
        } else {
            timezone_offset = -timezone_offset;
            '+'
        };
        let offset_hrs = (timezone_offset / 3600) as u8;
        let remainder = timezone_offset % 3600;
        let offset_mins = (remainder / 60) as u8;
        let offset_secs = (remainder % 60) as u8;

        Ok(Self {
            hrs,
            mins,
            secs,
            microseconds,
            offset_sign,
            offset_hrs,
            offset_mins,
            offset_secs,
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TIMETZ
    }
}

impl From<TimeTz> for JsonValue {
    fn from(v: TimeTz) -> Self {
        let mut time = format!("{:02}:{:02}:{:02}", v.hrs, v.mins, v.secs);
        if v.microseconds > 0 {
            time.push('.');
            time.push_str(v.microseconds.to_string().trim_end_matches('0'));
        }
        time.push_str(&format!("{}{:02}", v.offset_sign, v.offset_hrs));
        if v.offset_mins > 0 {
            time.push_str(&format!(":{:02}", v.offset_mins));
        }
        if v.offset_secs > 0 {
            time.push_str(&format!(":{:02}", v.offset_secs));
        }
        JsonValue::String(time)
    }
}

/// INTERVAL: 8-byte microseconds, 4-byte days, 4-byte months (signed).
/// Matches `advanced_types.rs::Interval`.
struct Interval {
    years: i32,
    months: i8,
    days: i32,
    sign: char,
    hours: u8,
    minutes: u8,
    seconds: u8,
    microseconds: u32,
}

impl<'a> FromSql<'a> for Interval {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 16 {
            return Err(format!("expected 16 bytes for INTERVAL, got {}", raw.len()).into());
        }
        let mut microseconds = i64::from_be_bytes(raw[0..8].try_into().unwrap());
        let mut days = i32::from_be_bytes(raw[8..12].try_into().unwrap());
        let mut months = i32::from_be_bytes(raw[12..16].try_into().unwrap());
        let mut years = 0;

        if !(-11..=11).contains(&months) {
            years = months / 12;
            months %= 12;
        }

        let sign = if microseconds < 0 {
            microseconds = -microseconds;
            '-'
        } else {
            '+'
        };

        let mut hrs = microseconds / (1_000_000 * 60 * 60);
        microseconds %= 1_000_000 * 60 * 60;
        let mins = (microseconds / (1_000_000 * 60)) as u8;
        microseconds %= 1_000_000 * 60;
        let secs = (microseconds / 1_000_000) as u8;
        let microseconds = (microseconds % 1_000_000) as u32;

        if !(-23..=23).contains(&hrs) {
            days += (hrs / 24) as i32;
            hrs %= 24;
        }

        Ok(Self {
            years,
            months: months as i8,
            days,
            sign,
            hours: hrs as u8,
            minutes: mins,
            seconds: secs,
            microseconds,
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::INTERVAL
    }
}

impl From<Interval> for JsonValue {
    fn from(v: Interval) -> Self {
        let mut s = String::new();

        if v.years != 0 {
            let unit = if v.years == 1 || v.years == -1 {
                "year"
            } else {
                "years"
            };
            s.push_str(&format!("{} {} ", v.years, unit));
        }
        if v.months != 0 {
            let unit = if v.months == 1 || v.months == -1 {
                "month"
            } else {
                "months"
            };
            s.push_str(&format!("{} {} ", v.months, unit));
        }
        if v.days != 0 {
            let unit = if v.days == 1 || v.days == -1 {
                "day"
            } else {
                "days"
            };
            s.push_str(&format!("{} {} ", v.days, unit));
        }
        if v.hours != 0 || v.minutes != 0 || v.seconds != 0 || v.microseconds != 0 {
            if v.sign != '+' {
                s.push(v.sign);
            }
            s.push_str(&format!("{:02}:{:02}:{:02}", v.hours, v.minutes, v.seconds));
            if v.microseconds != 0 {
                s.push('.');
                s.push_str(v.microseconds.to_string().trim_end_matches('0'));
            }
        }

        JsonValue::String(s)
    }
}

/// INET/CIDR wire format: 1 byte family (2=IPv4, 3=IPv6), 1 byte netmask,
/// 1 byte is_cidr flag (ignored — INET and CIDR share this layout), 1 byte
/// address length, then the address bytes. Matches
/// `advanced_types.rs::CidrOrInet`.
struct CidrOrInet {
    addr: std::net::IpAddr,
    netmask: u8,
}

impl<'a> FromSql<'a> for CidrOrInet {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 8 {
            return Err("invalid buffer size for INET/CIDR".into());
        }
        let family = raw[0];
        let netmask = raw[1];
        let len = raw[3];

        match family {
            2 => {
                if netmask > 32 || len != 4 {
                    return Err("invalid IPv4 INET/CIDR buffer".into());
                }
                let octets: [u8; 4] = raw[4..8].try_into().unwrap();
                Ok(Self {
                    addr: std::net::IpAddr::from(octets),
                    netmask,
                })
            }
            3 => {
                if netmask > 128 || len != 16 || raw.len() < 20 {
                    return Err("invalid IPv6 INET/CIDR buffer".into());
                }
                let bytes: [u8; 16] = raw[4..20].try_into().unwrap();
                Ok(Self {
                    addr: std::net::IpAddr::from(bytes),
                    netmask,
                })
            }
            _ => Err(format!("unexpected INET/CIDR family byte: {family}").into()),
        }
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::INET || *ty == Type::CIDR
    }
}

impl From<CidrOrInet> for JsonValue {
    fn from(v: CidrOrInet) -> Self {
        JsonValue::String(format!("{}/{}", v.addr, v.netmask))
    }
}

/// MACADDR: exactly 6 raw bytes. Matches `advanced_types.rs::MacAddr`.
struct MacAddr {
    bytes: [u8; 6],
}

impl<'a> FromSql<'a> for MacAddr {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 6 {
            return Err(format!("expected 6 bytes for MACADDR, got {}", raw.len()).into());
        }
        let mut bytes = [0u8; 6];
        bytes.copy_from_slice(raw);
        Ok(Self { bytes })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::MACADDR
    }
}

impl From<MacAddr> for JsonValue {
    fn from(v: MacAddr) -> Self {
        JsonValue::String(format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            v.bytes[0], v.bytes[1], v.bytes[2], v.bytes[3], v.bytes[4], v.bytes[5]
        ))
    }
}

/// MACADDR8 (EUI-64): exactly 8 raw bytes. Matches
/// `extract/advanced_types.rs::MacAddr8`.
pub(crate) struct MacAddr8 {
    bytes: [u8; 8],
}

impl<'a> FromSql<'a> for MacAddr8 {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 8 {
            return Err(format!("expected 8 bytes for MACADDR8, got {}", raw.len()).into());
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(raw);
        Ok(Self { bytes })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::MACADDR8
    }
}

impl From<MacAddr8> for JsonValue {
    fn from(v: MacAddr8) -> Self {
        JsonValue::String(format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            v.bytes[0],
            v.bytes[1],
            v.bytes[2],
            v.bytes[3],
            v.bytes[4],
            v.bytes[5],
            v.bytes[6],
            v.bytes[7]
        ))
    }
}

/// BIT/VARBIT: 4-byte bit count, then the packed bits (padded to a byte
/// boundary), formatted as a string of '0'/'1' characters. Matches
/// `extract/advanced_types.rs::BitOrVarBit`.
pub(crate) struct BitOrVarBit {
    bits: String,
}

impl<'a> FromSql<'a> for BitOrVarBit {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 4 {
            return Err(format!(
                "expected at least 4 bytes for BIT/VARBIT, got {}",
                raw.len()
            )
            .into());
        }

        let bits_num = i32::from_be_bytes(raw[..4].try_into().unwrap()) as usize;
        let mut bits_len = bits_num / 8;
        let remainder = bits_num % 8;
        if remainder > 0 {
            bits_len += 1;
        }

        if raw.len() < 4 + bits_len {
            return Err(format!(
                "expected at least {} bytes for BIT/VARBIT, got {}",
                4 + bits_len,
                raw.len()
            )
            .into());
        }

        if bits_len == 0 {
            return Ok(Self {
                bits: String::new(),
            });
        }

        let mut bits = String::with_capacity(bits_num);
        for b in &raw[4..4 + bits_len - 1] {
            bits.push_str(&format!("{:08b}", b));
        }

        let last_byte = format!("{:08b}", raw[4 + bits_len - 1]);
        if remainder > 0 {
            // Remove the zero-padding PostgreSQL appends to fill the last byte.
            bits.push_str(&last_byte[..remainder]);
        } else {
            bits.push_str(&last_byte);
        }

        Ok(Self { bits })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::BIT || *ty == Type::VARBIT
    }
}

impl From<BitOrVarBit> for JsonValue {
    fn from(v: BitOrVarBit) -> Self {
        JsonValue::String(v.bits)
    }
}

/// System-identifier and object-reference ("Reg") types are all plain u32
/// OIDs under the hood. Matches the builtin driver's `advanced_types.rs`
/// `u32_wrapper!` macro — introduced here (the plugin's only macro) rather
/// than hand-writing eleven near-identical structs.
macro_rules! u32_oid_wrapper {
    ($name:ident, $pg_type:ident) => {
        pub(crate) struct $name(u32);

        impl<'a> FromSql<'a> for $name {
            fn from_sql(
                ty: &Type,
                raw: &[u8],
            ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
                Ok(Self(<u32 as FromSql>::from_sql(ty, raw)?))
            }

            fn accepts(ty: &Type) -> bool {
                *ty == Type::$pg_type
            }
        }

        impl From<$name> for JsonValue {
            fn from(v: $name) -> Self {
                JsonValue::from(v.0)
            }
        }
    };
}

u32_oid_wrapper!(Xid, XID);
u32_oid_wrapper!(Cid, CID);
u32_oid_wrapper!(RegProc, REGPROC);
u32_oid_wrapper!(RegProcedure, REGPROCEDURE);
u32_oid_wrapper!(RegOper, REGOPER);
u32_oid_wrapper!(RegOperator, REGOPERATOR);
u32_oid_wrapper!(RegClass, REGCLASS);
u32_oid_wrapper!(RegType, REGTYPE);
u32_oid_wrapper!(RegConfig, REGCONFIG);
u32_oid_wrapper!(RegDictionary, REGDICTIONARY);
u32_oid_wrapper!(RegNamespace, REGNAMESPACE);
u32_oid_wrapper!(RegRole, REGROLE);
u32_oid_wrapper!(RegCollation, REGCOLLATION);

/// XID8: an 8-byte transaction ID, wire-identical to INT8 but logically
/// unsigned — reinterpret the bits as u64 and use the same JS-safe-integer
/// stringification u64 gets. Matches `extract/advanced_types.rs::Xid8`.
pub(crate) struct Xid8(u64);

impl<'a> FromSql<'a> for Xid8 {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(<i64 as FromSql>::from_sql(ty, raw)? as u64))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::XID8
    }
}

impl From<Xid8> for JsonValue {
    fn from(v: Xid8) -> Self {
        u64_to_json(v.0)
    }
}

/// TID: a tuple identifier — 4-byte block number, 2-byte offset — formatted
/// as `"(block, offset)"`. Matches `extract/advanced_types.rs::Tid`.
pub(crate) struct Tid {
    block_num: u32,
    offset: u16,
}

impl<'a> FromSql<'a> for Tid {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 6 {
            return Err(format!("expected 6 bytes for TID, got {}", raw.len()).into());
        }
        Ok(Self {
            block_num: <u32 as FromSql>::from_sql(&Type::OID, &raw[..4])?,
            offset: <i16 as FromSql>::from_sql(&Type::INT2, &raw[4..])? as u16,
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TID
    }
}

impl From<Tid> for JsonValue {
    fn from(v: Tid) -> Self {
        JsonValue::String(format!("({}, {})", v.block_num, v.offset))
    }
}

/// POINT: two 8-byte big-endian floats (x, y), formatted as `"(x, y)"`.
/// The base geometric primitive every other geometric type below decodes
/// through. Matches `extract/advanced_types.rs::Point`.
pub(crate) struct Point {
    x: f64,
    y: f64,
}

impl Point {
    fn extract(raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 16 {
            return Err(format!("expected 16 bytes for Point, got {}", raw.len()).into());
        }
        Ok(Self {
            x: <f64 as FromSql>::from_sql(&Type::FLOAT8, &raw[..8])?,
            y: <f64 as FromSql>::from_sql(&Type::FLOAT8, &raw[8..])?,
        })
    }
}

impl<'a> FromSql<'a> for Point {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Point::extract(raw)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::POINT
    }
}

impl From<Point> for JsonValue {
    fn from(v: Point) -> Self {
        JsonValue::String(format!("({}, {})", v.x, v.y))
    }
}

/// LSEG: two consecutive 16-byte points, formatted as `"[(x1, y1), (x2, y2)]"`.
/// Matches `extract/advanced_types.rs::Lseg`.
pub(crate) struct Lseg {
    p1: Point,
    p2: Point,
}

impl<'a> FromSql<'a> for Lseg {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 32 {
            return Err(format!("expected 32 bytes for Lseg, got {}", raw.len()).into());
        }
        Ok(Self {
            p1: Point::extract(&raw[..16])?,
            p2: Point::extract(&raw[16..])?,
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::LSEG
    }
}

impl From<Lseg> for JsonValue {
    fn from(v: Lseg) -> Self {
        JsonValue::String(format!(
            "[({}, {}), ({}, {})]",
            v.p1.x, v.p1.y, v.p2.x, v.p2.y
        ))
    }
}

/// BOX: two consecutive 16-byte points (upper-right, lower-left), formatted
/// as `"((x1, y1), (x2, y2))"`. Named `PgBox` to avoid shadowing
/// `std::boxed::Box`, matching the builtin's own naming
/// (`extract/advanced_types.rs::PgBox`).
pub(crate) struct PgBox {
    upper_right: Point,
    lower_left: Point,
}

impl<'a> FromSql<'a> for PgBox {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 32 {
            return Err(format!("expected 32 bytes for Box, got {}", raw.len()).into());
        }
        Ok(Self {
            upper_right: Point::extract(&raw[..16])?,
            lower_left: Point::extract(&raw[16..])?,
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::BOX
    }
}

impl From<PgBox> for JsonValue {
    fn from(v: PgBox) -> Self {
        JsonValue::String(format!(
            "(({}, {}), ({}, {}))",
            v.upper_right.x, v.upper_right.y, v.lower_left.x, v.lower_left.y
        ))
    }
}

/// POLYGON: 4-byte point count, then that many consecutive 16-byte points,
/// formatted as `"((x1, y1), (x2, y2), ...)"`. Matches
/// `extract/advanced_types.rs::Polygon`. PostgreSQL requires at least one
/// point (there is no "empty polygon" literal), but the decoder does not
/// assume that — a zero-point buffer decodes to `"()"` rather than erroring.
pub(crate) struct Polygon {
    points: Vec<Point>,
}

impl<'a> FromSql<'a> for Polygon {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 4 {
            return Err(format!("expected at least 4 bytes for Polygon, got {}", raw.len()).into());
        }
        let num_points = i32::from_be_bytes(raw[..4].try_into().unwrap());
        if num_points < 0 {
            return Err(format!(
                "expected non-negative number of points for Polygon, got {}",
                num_points
            )
            .into());
        }
        let num_points = num_points as usize;
        if raw.len() < 4 + num_points * 16 {
            return Err(format!(
                "expected at least {} bytes for Polygon, got {}",
                4 + num_points * 16,
                raw.len()
            )
            .into());
        }
        let mut points = Vec::with_capacity(num_points);
        for chunk in raw[4..4 + num_points * 16].as_chunks::<16>().0 {
            points.push(Point::extract(chunk)?);
        }
        Ok(Self { points })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::POLYGON
    }
}

impl From<Polygon> for JsonValue {
    fn from(v: Polygon) -> Self {
        let mut s = String::with_capacity(2 + v.points.len() * 16);
        s.push('(');
        let mut first = true;
        for p in &v.points {
            if !first {
                s.push_str(", ");
            }
            first = false;
            s.push_str(&format!("({}, {})", p.x, p.y));
        }
        s.push(')');
        JsonValue::String(s)
    }
}

/// PATH: 1-byte closed/open flag (bit 0: 1 = closed, 0 = open), 4-byte point
/// count, then that many consecutive 16-byte points, formatted as
/// `"(...)"` when closed or `"[...]"` when open. Matches
/// `extract/advanced_types.rs::Path`.
pub(crate) struct Path {
    flag: u8,
    points: Vec<Point>,
}

impl<'a> FromSql<'a> for Path {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 5 {
            return Err(format!("expected at least 5 bytes for Path, got {}", raw.len()).into());
        }
        let flag = raw[0];
        let num_points = i32::from_be_bytes(raw[1..5].try_into().unwrap());
        if num_points < 0 {
            return Err(format!(
                "expected non-negative number of points for Path, got {}",
                num_points
            )
            .into());
        }
        let num_points = num_points as usize;
        if raw.len() < 5 + num_points * 16 {
            return Err(format!(
                "expected at least {} bytes for Path, got {}",
                5 + num_points * 16,
                raw.len()
            )
            .into());
        }
        let mut points = Vec::with_capacity(num_points);
        for chunk in raw[5..5 + num_points * 16].as_chunks::<16>().0 {
            points.push(Point::extract(chunk)?);
        }
        Ok(Self { flag, points })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::PATH
    }
}

impl From<Path> for JsonValue {
    fn from(v: Path) -> Self {
        let (opening, closing) = if v.flag & 0x01 == 1 {
            ('(', ')')
        } else {
            ('[', ']')
        };
        let mut s = String::with_capacity(2 + v.points.len() * 16);
        s.push(opening);
        let mut first = true;
        for p in &v.points {
            if !first {
                s.push_str(", ");
            }
            first = false;
            s.push_str(&format!("({}, {})", p.x, p.y));
        }
        s.push(closing);
        JsonValue::String(s)
    }
}

/// LINE: three 8-byte big-endian floats (Ax + By + C = 0 coefficients),
/// formatted as `"{A, B, C}"`. Matches `extract/advanced_types.rs::Line`.
pub(crate) struct Line {
    a: f64,
    b: f64,
    c: f64,
}

impl<'a> FromSql<'a> for Line {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 24 {
            return Err(format!("expected 24 bytes for Line, got {}", raw.len()).into());
        }
        Ok(Self {
            a: f64::from_sql(&Type::FLOAT8, &raw[..8])?,
            b: f64::from_sql(&Type::FLOAT8, &raw[8..16])?,
            c: f64::from_sql(&Type::FLOAT8, &raw[16..])?,
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::LINE
    }
}

impl From<Line> for JsonValue {
    fn from(v: Line) -> Self {
        JsonValue::String(format!("{{{}, {}, {}}}", v.a, v.b, v.c))
    }
}

/// CIRCLE: a 16-byte center point + an 8-byte radius, formatted as
/// `"<(x, y), r>"`. Matches `extract/advanced_types.rs::Circle`.
pub(crate) struct Circle {
    center: Point,
    radius: f64,
}

impl<'a> FromSql<'a> for Circle {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 24 {
            return Err(format!("expected 24 bytes for Circle, got {}", raw.len()).into());
        }
        Ok(Self {
            center: Point::extract(&raw[..16])?,
            radius: f64::from_sql(&Type::FLOAT8, &raw[16..])?,
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::CIRCLE
    }
}

impl From<Circle> for JsonValue {
    fn from(v: Circle) -> Self {
        JsonValue::String(format!("<({}, {}), {}>", v.center.x, v.center.y, v.radius))
    }
}

/// A wire format that's just the raw UTF-8 bytes of a value, no length
/// prefix or other framing. Matches the builtin driver's `utf8_wrapper!`
/// macro (`extract/advanced_types.rs`).
macro_rules! utf8_wrapper {
    ($name:ident, $pg_type:ident) => {
        pub(crate) struct $name(String);

        impl<'a> FromSql<'a> for $name {
            fn from_sql(
                _ty: &Type,
                raw: &[u8],
            ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
                Ok(Self(String::from_utf8(raw.to_vec())?))
            }

            fn accepts(ty: &Type) -> bool {
                *ty == Type::$pg_type
            }
        }

        impl From<$name> for JsonValue {
            fn from(v: $name) -> Self {
                JsonValue::String(v.0)
            }
        }
    };
}

utf8_wrapper!(Xml, XML);
utf8_wrapper!(RefCursor, REFCURSOR);
// ACLITEM is deliberately NOT implemented: PostgreSQL has no binary send
// function for it at all (confirmed live — the server itself rejects the
// query with "no binary output function available for type aclitem",
// SQLSTATE 42883, before any client-side decoding ever runs). Every
// tokio_postgres/deadpool-postgres query uses the binary protocol, so an
// ACLITEM/ACLITEM[] column can never reach either driver's decode layer —
// this matches the builtin's own FIXME comment in `advanced_types.rs`,
// which implements a struct for it anyway but notes it's unreachable.
utf8_wrapper!(PgNodeTree, PG_NODE_TREE);

/// JSONPATH: 1-byte version prefix, then the path's UTF-8 bytes. Matches
/// `extract/advanced_types.rs::JsonPath`. Without stripping the version
/// byte, the raw bytes still happen to decode as a `String` via
/// `tokio_postgres`'s built-in `FromSql` (JSONPATH is text-like), but with
/// the version byte prepended as a stray control character — a silent
/// corruption rather than a `null`, which is why this type needs an
/// explicit arm even though the generic string fallback "works" for it.
pub(crate) struct JsonPath {
    path: String,
}

impl<'a> FromSql<'a> for JsonPath {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.is_empty() {
            return Err("invalid JSON path".into());
        }
        let path = String::from_utf8(raw[1..].to_vec())?;
        Ok(Self { path })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::JSONPATH
    }
}

impl From<JsonPath> for JsonValue {
    fn from(v: JsonPath) -> Self {
        JsonValue::String(v.path)
    }
}

/// One lexeme entry within a TSVECTOR: a NUL-terminated text, then a
/// 2-byte position count, then that many 2-byte (weight: top 2 bits,
/// position: low 14 bits) entries. Matches
/// `extract/advanced_types.rs::Lexeme`.
struct Lexeme {
    text: String,
    positions_weights: Vec<(u16, char)>,
}

impl Lexeme {
    fn try_extract_from(buf: &mut &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let text = extract_nul_terminated_text(buf)?;

        if buf.len() < 2 {
            return Err("buf too short for position count".into());
        }
        let position_count = i16::from_be_bytes(buf[..2].try_into().unwrap());
        if position_count < 0 {
            return Err(format!(
                "expected non-negative position count, got: {}",
                position_count
            )
            .into());
        }
        if position_count == 0 {
            return Ok(Self {
                text,
                positions_weights: Vec::new(),
            });
        }
        *buf = &buf[2..];

        let position_count = position_count as usize;
        if buf.len() < position_count * 2 {
            return Err(format!(
                "buf too short for positions and weights: expected {} bytes, got {}",
                position_count * 2,
                buf.len()
            )
            .into());
        }

        let mut positions_weights = Vec::with_capacity(position_count);
        for _ in 0..position_count {
            let position_weight = u16::from_be_bytes(buf[..2].try_into().unwrap());
            let weight = match position_weight >> 14 {
                0 => 'D',
                1 => 'C',
                2 => 'B',
                3 => 'A',
                _ => unreachable!(),
            };
            let position = position_weight & 0x3FFF;
            *buf = &buf[2..];
            positions_weights.push((position, weight));
        }

        Ok(Self {
            text,
            positions_weights,
        })
    }
}

/// Read a NUL-terminated string from the front of `buf`, advancing `buf`
/// past the terminator. Shared by `Lexeme` and the TSQUERY operand parser
/// (both formats use this exact framing for lexeme text). Matches
/// `extract/advanced_types.rs::try_extract_lexeme_text`.
fn extract_nul_terminated_text(
    buf: &mut &[u8],
) -> Result<String, Box<dyn std::error::Error + Sync + Send>> {
    let nul_pos = buf
        .iter()
        .position(|b| *b == 0)
        .ok_or("lexeme string not terminated")?;
    let text = String::from_utf8(buf[..nul_pos].to_vec())?;
    *buf = &buf[nul_pos + 1..];
    Ok(text)
}

/// TSVECTOR: 4-byte lexeme count, then that many `Lexeme` entries.
/// Formatted as PostgreSQL's own `'lexeme':pos,pos ...` text form. Matches
/// `extract/advanced_types.rs::TsVector`.
pub(crate) struct TsVector {
    lexemes: Vec<Lexeme>,
}

impl<'a> FromSql<'a> for TsVector {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 4 {
            return Err(format!(
                "raw buffer too short for TsVector: expected at least 4 bytes, got {}",
                raw.len()
            )
            .into());
        }
        let count = i32::from_be_bytes(raw[..4].try_into().unwrap());
        if count == 0 {
            return Ok(Self {
                lexemes: Vec::new(),
            });
        }
        let mut buf = &raw[4..];
        let mut lexemes = Vec::with_capacity(count as usize);
        for _ in 0..count {
            lexemes.push(Lexeme::try_extract_from(&mut buf)?);
        }
        Ok(Self { lexemes })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TS_VECTOR
    }
}

impl From<TsVector> for JsonValue {
    fn from(v: TsVector) -> Self {
        let mut ss = Vec::with_capacity(v.lexemes.len());
        for lexeme in v.lexemes {
            let mut s = format!("'{}':", lexeme.text);
            let mut positions_weights = lexeme.positions_weights.into_iter();
            if let Some((position, weight)) = positions_weights.next() {
                s.push_str(&position.to_string());
                if weight != 'D' {
                    s.push(weight);
                }
            }
            for (position, weight) in positions_weights {
                s.push(',');
                s.push_str(&position.to_string());
                if weight != 'D' {
                    s.push(weight);
                }
            }
            ss.push(s);
        }
        JsonValue::String(ss.join(" "))
    }
}

/// TSQUERY: a 4-byte length prefix (ignored — the recursive tree below is
/// self-delimiting), then a binary tree of operand/operator nodes.
/// Genuinely complex enough that it's decoded straight to its text
/// representation rather than an intermediate structure. Matches
/// `extract/advanced_types.rs::TsQuery` + `try_extract_ts_query`.
pub(crate) struct TsQuery {
    query: String,
}

impl<'a> FromSql<'a> for TsQuery {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 4 {
            return Err(format!(
                "error extracting TsQuery: expected at least 4 bytes, got {}",
                raw.len()
            )
            .into());
        }
        let mut buf = &raw[4..];
        let query = extract_ts_query_node(&mut buf, 4)
            .map_err(|e| -> Box<dyn std::error::Error + Sync + Send> { e.into() })?;
        Ok(Self { query })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TSQUERY
    }
}

impl From<TsQuery> for JsonValue {
    fn from(v: TsQuery) -> Self {
        JsonValue::String(v.query)
    }
}

/// Recursively decode one TSQUERY tree node: `buf[0] == 1` is an operand
/// (lexeme + weight + prefix flag), `buf[0] == 2` is an operator (NOT/AND/
/// OR/phrase-distance) with left/right subtrees. `pre_lvl` is the parent
/// operator's precedence level, used to decide whether this subtree needs
/// parenthesizing when rendered — matches PostgreSQL's own `tsquery`
/// output rules. Matches `extract/advanced_types.rs::try_extract_ts_query`.
fn extract_ts_query_node(buf: &mut &[u8], pre_lvl: u8) -> Result<String, String> {
    if buf.is_empty() {
        return Err("fail to extract ts_query: buffer is empty".into());
    }

    match buf[0] {
        1 => {
            if buf.len() < 3 {
                return Err("fail to extract ts_query operand: buffer is too short".into());
            }
            let weight = match buf[1] {
                0 => None,
                1 => Some('D'),
                2 => Some('C'),
                4 => Some('B'),
                8 => Some('A'),
                _ => {
                    return Err(
                        "fail to extract ts_query operand weight: invalid weight value".into(),
                    )
                }
            };
            let prefixed = buf[2] == 1;
            *buf = &buf[3..];

            let lexeme = extract_nul_terminated_text(buf).map_err(|e| e.to_string())?;

            let mut s = format!("'{}'", lexeme);
            if prefixed {
                s.push_str(":*");
            }
            if let Some(weight) = weight {
                if prefixed {
                    s.push(weight);
                } else {
                    s.push(':');
                    s.push(weight);
                }
            }
            Ok(s)
        }
        2 => {
            let operator = *buf
                .get(1)
                .ok_or("fail to extract ts_query operator: buffer is too short")?;
            *buf = &buf[2..];

            let (cur_lvl, operator): (u8, String) = match operator {
                1 => {
                    let operand = extract_ts_query_node(buf, 1)?;
                    return Ok(format!("!{}", operand));
                }
                2 => (3, "&".into()),
                3 => (4, "|".into()),
                4 => {
                    if buf.len() < 2 {
                        return Err(
                            "fail to extract ts_query phrase operator distance: buffer is too short"
                                .into(),
                        );
                    }
                    let distance = i16::from_be_bytes(buf[..2].try_into().unwrap());
                    *buf = &buf[2..];
                    if distance == 1 {
                        (2, "<->".into())
                    } else {
                        (2, format!("<{}>", distance))
                    }
                }
                _ => {
                    return Err(format!(
                        "fail to extract ts_query operator: invalid operator expected 1, 2, 3, or 4, got: {}",
                        operator
                    ));
                }
            };

            let right_operand = extract_ts_query_node(buf, cur_lvl)?;
            let left_operand = extract_ts_query_node(buf, cur_lvl)?;

            if pre_lvl < cur_lvl {
                Ok(format!("({} {} {})", left_operand, operator, right_operand))
            } else {
                Ok(format!("{} {} {}", left_operand, operator, right_operand))
            }
        }
        other => Err(format!(
            "fail to extract ts_query: expected 1 or 2 got: {}",
            other
        )),
    }
}

/// GTSVECTOR (a signature-compressed index-internal representation of
/// TSVECTOR, distinct from TS_VECTOR itself): a 4-byte header + 1-byte
/// signature flag, rendered as a blob string since it has no meaningful
/// text form. Matches `extract/advanced_types.rs::GtsVector`.
pub(crate) struct GtsVector {
    header: [u8; 4],
    signature: u8,
}

impl<'a> FromSql<'a> for GtsVector {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 5 {
            return Err(format!(
                "fail to extract gts_vector: expected at least 5 bytes, got {}",
                raw.len()
            )
            .into());
        }
        Ok(Self {
            header: [raw[0], raw[1], raw[2], raw[3]],
            signature: raw[4],
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::GTS_VECTOR
    }
}

impl From<GtsVector> for JsonValue {
    fn from(v: GtsVector) -> Self {
        let bytes = [
            v.header[0],
            v.header[1],
            v.header[2],
            v.header[3],
            v.signature,
        ];
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
        JsonValue::String(format!("BLOB:{}:application/octet-stream:{}", 5, b64))
    }
}

/// PG_LSN: two 4-byte big-endian halves of a log sequence number,
/// formatted as uppercase-hex `"UPPER/LOWER"`. Matches
/// `extract/advanced_types.rs::PgLsn`.
pub(crate) struct PgLsn {
    upper: u32,
    lower: u32,
}

impl<'a> FromSql<'a> for PgLsn {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 8 {
            return Err(
                format!("fail to extract PgLsn: expected 8 bytes, got {}", raw.len()).into(),
            );
        }
        Ok(Self {
            upper: u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]),
            lower: u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]),
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::PG_LSN
    }
}

impl From<PgLsn> for JsonValue {
    fn from(v: PgLsn) -> Self {
        JsonValue::String(format!("{:X}/{:X}", v.upper, v.lower))
    }
}

/// TXID_SNAPSHOT / PG_SNAPSHOT (both share this wire layout): 4-byte
/// active-xid count, 8-byte xmin, 8-byte xmax, then that many 8-byte
/// active xids. Formatted as `"xmin:xmax:active,active,..."`. Matches
/// `extract/advanced_types.rs::TxidSnapshotOrPgSnapshot`.
pub(crate) struct TxidSnapshotOrPgSnapshot {
    xmin: i64,
    xmax: i64,
    active_xids: Vec<i64>,
}

impl<'a> FromSql<'a> for TxidSnapshotOrPgSnapshot {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 20 {
            return Err(format!(
                "fail to extract TxidSnapshotOrPgSnapshot: expected at least 20 bytes, got {}",
                raw.len()
            )
            .into());
        }
        let count = i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
        if count < 0 {
            return Err(format!(
                "fail to extract TxidSnapshot/PgSnapshot: count is negative: {}",
                count
            )
            .into());
        }
        let xmin = i64::from_be_bytes(raw[4..12].try_into().unwrap());
        let xmax = i64::from_be_bytes(raw[12..20].try_into().unwrap());
        let count = count as usize;
        if count == 0 {
            return Ok(Self {
                xmin,
                xmax,
                active_xids: Vec::new(),
            });
        }
        let chunks = raw[20..].as_chunks::<8>().0;
        if chunks.len() < count {
            return Err(format!(
                "fail to extract TxidSnapshot/PgSnapshot: expected {} 8-byte chunks, got {}",
                count,
                chunks.len()
            )
            .into());
        }
        let active_xids = chunks[..count]
            .iter()
            .map(|chunk| i64::from_be_bytes(*chunk))
            .collect();
        Ok(Self {
            xmin,
            xmax,
            active_xids,
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TXID_SNAPSHOT || *ty == Type::PG_SNAPSHOT
    }
}

impl From<TxidSnapshotOrPgSnapshot> for JsonValue {
    fn from(v: TxidSnapshotOrPgSnapshot) -> Self {
        JsonValue::String(format!(
            "{}:{}:{}",
            v.xmin,
            v.xmax,
            v.active_xids
                .into_iter()
                .map(|xid| xid.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ))
    }
}

/// A wire format that's just an opaque byte blob with no meaningful text
/// representation — encoded the same way this plugin's existing `BYTEA`
/// arm does (see `extract_simple_kind`'s `Type::BYTEA` arm): the full
/// base64 payload with a hardcoded `application/octet-stream` MIME type,
/// no truncation. Matches the builtin's `binary_wrapper!` macro
/// (`extract/advanced_types.rs`), used for internal planner-statistics
/// types too rarely queried directly to be worth a real text
/// representation.
macro_rules! binary_blob_wrapper {
    ($name:ident, $pg_type:ident) => {
        pub(crate) struct $name(Vec<u8>);

        impl<'a> FromSql<'a> for $name {
            fn from_sql(
                _ty: &Type,
                raw: &[u8],
            ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
                Ok(Self(raw.to_vec()))
            }

            fn accepts(ty: &Type) -> bool {
                *ty == Type::$pg_type
            }
        }

        impl From<$name> for JsonValue {
            fn from(v: $name) -> Self {
                let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &v.0);
                JsonValue::String(format!(
                    "BLOB:{}:application/octet-stream:{}",
                    v.0.len(),
                    b64
                ))
            }
        }
    };
}

binary_blob_wrapper!(PgMcvList, PG_MCV_LIST);
binary_blob_wrapper!(PgDependencies, PG_DEPENDENCIES);
binary_blob_wrapper!(PgNdistinct, PG_NDISTINCT);
binary_blob_wrapper!(PgBrinBloomSummary, PG_BRIN_BLOOM_SUMMARY);
binary_blob_wrapper!(PgBrinMinmaxMultiSummary, PG_BRIN_MINMAX_MULTI_SUMMARY);

// pgvector extension types (`vector`, `halfvec`, `sparsevec`). These are
// extension-defined base types with dynamic OIDs — not available as
// `Type::*` constants — so `accepts` matches on the type name instead,
// same as `hstore`. Rendered as pgvector's own canonical text form, since
// pgvector accepts that same text form back on input. Matches
// `extract/advanced_types.rs`'s pgvector section.

/// Format an `f32` the way pgvector's text output does: shortest
/// round-trippable decimal (e.g. `1.0` -> `"1"`, `1.5` -> `"1.5"`). Rust's
/// default `f32` formatter already produces the shortest round-trip
/// representation.
fn format_vector_float(value: f32) -> String {
    value.to_string()
}

/// pgvector `vector`: `int16 dim`, `int16 unused`, then `dim` big-endian
/// `float4` values.
pub(crate) struct PgVector(Vec<f32>);

impl<'a> FromSql<'a> for PgVector {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 4 {
            return Err(format!("expected at least 4 bytes for vector, got {}", raw.len()).into());
        }
        let dim = u16::from_be_bytes([raw[0], raw[1]]) as usize;
        let expected = 4 + dim * 4;
        if raw.len() < expected {
            return Err(format!(
                "vector of dim {dim} expects {expected} bytes, got {}",
                raw.len()
            )
            .into());
        }
        let mut values = Vec::with_capacity(dim);
        for i in 0..dim {
            let off = 4 + i * 4;
            values.push(f32::from_be_bytes([
                raw[off],
                raw[off + 1],
                raw[off + 2],
                raw[off + 3],
            ]));
        }
        Ok(Self(values))
    }

    fn accepts(ty: &Type) -> bool {
        ty.name() == "vector"
    }
}

impl From<PgVector> for JsonValue {
    fn from(v: PgVector) -> Self {
        let body =
            v.0.iter()
                .map(|f| format_vector_float(*f))
                .collect::<Vec<_>>()
                .join(",");
        JsonValue::String(format!("[{body}]"))
    }
}

/// Decode an IEEE 754 half-precision (`binary16`) value into `f32`.
fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = if (bits >> 15) & 1 == 1 {
        -1.0f32
    } else {
        1.0f32
    };
    let exp = (bits >> 10) & 0x1f;
    let mant = bits & 0x3ff;
    match exp {
        0 => sign * (mant as f32) * 2f32.powi(-24), // zero / subnormal
        0x1f if mant == 0 => sign * f32::INFINITY,
        0x1f => f32::NAN,
        _ => sign * (1.0 + (mant as f32) / 1024.0) * 2f32.powi(exp as i32 - 15),
    }
}

/// pgvector `halfvec`: `int16 dim`, `int16 unused`, then `dim` big-endian
/// `float2` (half-precision) values.
pub(crate) struct PgHalfVector(Vec<f32>);

impl<'a> FromSql<'a> for PgHalfVector {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 4 {
            return Err(format!("expected at least 4 bytes for halfvec, got {}", raw.len()).into());
        }
        let dim = u16::from_be_bytes([raw[0], raw[1]]) as usize;
        let expected = 4 + dim * 2;
        if raw.len() < expected {
            return Err(format!(
                "halfvec of dim {dim} expects {expected} bytes, got {}",
                raw.len()
            )
            .into());
        }
        let mut values = Vec::with_capacity(dim);
        for i in 0..dim {
            let off = 4 + i * 2;
            values.push(f16_bits_to_f32(u16::from_be_bytes([
                raw[off],
                raw[off + 1],
            ])));
        }
        Ok(Self(values))
    }

    fn accepts(ty: &Type) -> bool {
        ty.name() == "halfvec"
    }
}

impl From<PgHalfVector> for JsonValue {
    fn from(v: PgHalfVector) -> Self {
        let body =
            v.0.iter()
                .map(|f| format_vector_float(*f))
                .collect::<Vec<_>>()
                .join(",");
        JsonValue::String(format!("[{body}]"))
    }
}

/// pgvector `sparsevec`: `int32 dim`, `int32 nnz`, `int32 unused`, then
/// `nnz` big-endian `int32` indices (0-based on the wire), then `nnz`
/// big-endian `float4` values. Text form is `{i1:v1,i2:v2}/dim` with
/// 1-based indices — the wire's 0-based indices get `+1`'d when rendered.
pub(crate) struct PgSparseVector {
    dim: i32,
    entries: Vec<(i32, f32)>,
}

impl<'a> FromSql<'a> for PgSparseVector {
    fn from_sql(_ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 12 {
            return Err(format!(
                "expected at least 12 bytes for sparsevec, got {}",
                raw.len()
            )
            .into());
        }
        let dim = i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let nnz = i32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
        // raw[8..12] is the unused/reserved header field.
        let expected = 12 + nnz * 4 + nnz * 4;
        if raw.len() < expected {
            return Err(format!(
                "sparsevec with {nnz} entries expects {expected} bytes, got {}",
                raw.len()
            )
            .into());
        }
        let mut entries = Vec::with_capacity(nnz);
        let values_off = 12 + nnz * 4;
        for i in 0..nnz {
            let idx_off = 12 + i * 4;
            let index = i32::from_be_bytes([
                raw[idx_off],
                raw[idx_off + 1],
                raw[idx_off + 2],
                raw[idx_off + 3],
            ]);
            let val_off = values_off + i * 4;
            let value = f32::from_be_bytes([
                raw[val_off],
                raw[val_off + 1],
                raw[val_off + 2],
                raw[val_off + 3],
            ]);
            entries.push((index, value));
        }
        Ok(Self { dim, entries })
    }

    fn accepts(ty: &Type) -> bool {
        ty.name() == "sparsevec"
    }
}

impl From<PgSparseVector> for JsonValue {
    fn from(v: PgSparseVector) -> Self {
        let body = v
            .entries
            .iter()
            // pgvector prints indices 1-based; the wire format stores them 0-based.
            .map(|(idx, val)| format!("{}:{}", idx + 1, format_vector_float(*val)))
            .collect::<Vec<_>>()
            .join(",");
        JsonValue::String(format!("{{{body}}}/{}", v.dim))
    }
}
