//! Value extraction from tokio-postgres rows to serde_json::Value.
//!
//! Replicates the exact type mapping of the built-in driver's
//! `src-tauri/src/drivers/postgres/extract/` system. Every PG type must
//! produce byte-identical JSON to the builtin — the parity tests enforce this.

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use tokio_postgres::types::{FromSql, Kind, Type};
use tokio_postgres::Row;
use uuid::Uuid;

/// JavaScript's Number.MAX_SAFE_INTEGER (2^53 - 1).
const JS_MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// Extract a single column value from a row as a JSON value.
/// Matches the builtin driver's extraction behavior exactly.
pub fn extract_value(row: &Row, index: usize) -> JsonValue {
    let col_type = row.columns()[index].type_().clone();

    // NULL check: try to get as Option first
    match col_type {
        ref t if *t == Type::BOOL => try_extract::<bool>(row, index, |v| JsonValue::Bool(v)),
        ref t if *t == Type::INT2 => try_extract::<i16>(row, index, |v| JsonValue::from(v)),
        ref t if *t == Type::INT4 => try_extract::<i32>(row, index, |v| JsonValue::from(v)),
        ref t if *t == Type::INT8 => try_extract::<i64>(row, index, |v| i64_to_json(v)),
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
        ref t if *t == Type::NUMERIC => try_extract::<Decimal>(row, index, |v| {
            JsonValue::String(v.to_string())
        }),
        ref t if *t == Type::TEXT || *t == Type::VARCHAR || *t == Type::BPCHAR || *t == Type::NAME => {
            try_extract::<String>(row, index, JsonValue::String)
        }
        ref t if *t == Type::UUID => try_extract::<Uuid>(row, index, |v| {
            JsonValue::String(v.to_string())
        }),
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
            JsonValue::String(format!(
                "BLOB:{}:application/octet-stream:{}",
                v.len(),
                b64
            ))
        }),
        ref t if *t == Type::INET || *t == Type::CIDR => {
            try_extract::<CidrOrInet>(row, index, JsonValue::from)
        }
        ref t if *t == Type::MACADDR => try_extract::<MacAddr>(row, index, JsonValue::from),
        ref t if *t == Type::OID => try_extract::<u32>(row, index, |v| JsonValue::from(v)),
        ref t if *t == Type::INT4_RANGE || *t == Type::INT8_RANGE || *t == Type::NUM_RANGE
            || *t == Type::TS_RANGE || *t == Type::TSTZ_RANGE || *t == Type::DATE_RANGE =>
        {
            try_extract_range(row, index)
        }
        ref t if *t == Type::INT2_ARRAY => try_extract::<Vec<i16>>(row, index, |v| {
            JsonValue::Array(v.into_iter().map(JsonValue::from).collect())
        }),
        ref t if *t == Type::INT4_ARRAY => try_extract::<Vec<i32>>(row, index, |v| {
            JsonValue::Array(v.into_iter().map(JsonValue::from).collect())
        }),
        ref t if *t == Type::INT8_ARRAY => try_extract::<Vec<i64>>(row, index, |v| {
            JsonValue::Array(v.into_iter().map(i64_to_json).collect())
        }),
        ref t if *t == Type::TEXT_ARRAY || *t == Type::VARCHAR_ARRAY => {
            try_extract::<Vec<String>>(row, index, |v| {
                JsonValue::Array(v.into_iter().map(JsonValue::String).collect())
            })
        }
        ref t if *t == Type::FLOAT4_ARRAY => try_extract::<Vec<f32>>(row, index, |v| {
            JsonValue::Array(
                v.into_iter()
                    .map(|f| {
                        serde_json::Number::from_f64(f as f64)
                            .map(JsonValue::Number)
                            .unwrap_or(JsonValue::Null)
                    })
                    .collect(),
            )
        }),
        ref t if *t == Type::FLOAT8_ARRAY => try_extract::<Vec<f64>>(row, index, |v| {
            JsonValue::Array(
                v.into_iter()
                    .map(|f| {
                        serde_json::Number::from_f64(f)
                            .map(JsonValue::Number)
                            .unwrap_or(JsonValue::Null)
                    })
                    .collect(),
            )
        }),
        ref t if *t == Type::BOOL_ARRAY => try_extract::<Vec<bool>>(row, index, |v| {
            JsonValue::Array(v.into_iter().map(JsonValue::Bool).collect())
        }),
        // For types not explicitly handled (ranges, composites, geometric, etc.),
        // fall back to text representation via the Display trait on the raw bytes.
        _ => {
            // Try as string — many types have text representations
            match row.try_get::<_, String>(index) {
                Ok(s) => JsonValue::String(s),
                Err(_) => JsonValue::Null,
            }
        }
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

/// Helper: try to extract a typed value from the row, returning JsonValue::Null
/// on any failure (NULL column, type mismatch, etc.).
fn try_extract<'a, T>(
    row: &'a Row,
    index: usize,
    map: impl FnOnce(T) -> JsonValue,
) -> JsonValue
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
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let subtype = match ty.kind() {
            Kind::Range(t) => t.clone(),
            _ => return Err("expected a range type".into()),
        };

        if raw.is_empty() {
            return Err("empty range buffer".into());
        }
        let flag = raw[0];
        let mut buf = &raw[1..];

        // RANGE_EMPTY flag bit 0
        if (flag & 1) == 1 {
            return Ok(Self("empty".to_string()));
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
            match extract_range_bound(&subtype, &mut buf) {
                Some(s) => out.push_str(&s),
                None => {
                    out.push_str("null, null");
                    out.push(upper_char);
                    return Ok(Self(out));
                }
            }
        }
        out.push_str(", ");

        // RANGE_UB_INF flag bit 4 — upper bound is unbounded (nothing pushed).
        if flag & (1 << 4) == 0 {
            if let Some(s) = extract_range_bound(&subtype, &mut buf) {
                out.push_str(&s);
            } else {
                out.push_str("null");
            }
        }
        out.push(upper_char);

        Ok(Self(out))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.kind(), Kind::Range(_))
    }
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

/// Format a raw byte buffer as JSON for the subset of simple PG types that
/// can appear as range bounds in this plugin's test corpus (integers,
/// numeric, date/timestamp). Falls back to Null for anything else.
fn extract_simple_from_bytes(ty: &Type, buf: &[u8]) -> JsonValue {
    match *ty {
        Type::INT4 => i32::from_sql(ty, buf).map(JsonValue::from).unwrap_or(JsonValue::Null),
        Type::INT8 => i64::from_sql(ty, buf).map(i64_to_json).unwrap_or(JsonValue::Null),
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
            let unit = if v.years == 1 || v.years == -1 { "year" } else { "years" };
            s.push_str(&format!("{} {} ", v.years, unit));
        }
        if v.months != 0 {
            let unit = if v.months == 1 || v.months == -1 { "month" } else { "months" };
            s.push_str(&format!("{} {} ", v.months, unit));
        }
        if v.days != 0 {
            let unit = if v.days == 1 || v.days == -1 { "day" } else { "days" };
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
