//! The report json writer.
//!
//! A minimal json value and its printing, kept to the
//! shapes the report carries and printed the way the
//! Python harness printed them.

pub(crate) fn opt_int(value: Option<i32>) -> JsonValue {
    match value {
        Some(v) => JsonValue::Int(v.into()),
        None => JsonValue::Null,
    }
}

pub(crate) fn opt_str(value: Option<&str>) -> JsonValue {
    match value {
        Some(v) => JsonValue::Str(v.to_string()),
        None => JsonValue::Null,
    }
}

pub(crate) fn opt_bool(value: Option<bool>) -> JsonValue {
    match value {
        Some(v) => JsonValue::Bool(v),
        None => JsonValue::Null,
    }
}

pub(crate) fn opt_float(value: Option<f64>) -> JsonValue {
    match value {
        Some(v) => JsonValue::Float(v),
        None => JsonValue::Null,
    }
}

/// A minimal json value used only to print reports the way the
/// Python harness printed them, with two space indentation and
/// non-ascii text escaped the way Python escapes it.
#[derive(Debug, Clone)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

pub fn dump_pretty(value: &JsonValue) -> String {
    let mut out = String::new();
    write_value(value, 0, &mut out);
    out
}

fn write_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_value(value: &JsonValue, depth: usize, out: &mut String) {
    match value {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(true) => out.push_str("true"),
        JsonValue::Bool(false) => out.push_str("false"),
        JsonValue::Int(v) => out.push_str(&v.to_string()),
        JsonValue::Float(v) => out.push_str(&py_float(*v)),
        JsonValue::Str(s) => write_json_string(s, out),
        JsonValue::List(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (index, item) in items.iter().enumerate() {
                write_indent(out, depth + 1);
                write_value(item, depth + 1, out);
                if index + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            write_indent(out, depth);
            out.push(']');
        }
        JsonValue::Object(fields) => {
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (index, (key, item)) in fields.iter().enumerate() {
                write_indent(out, depth + 1);
                write_json_string(key, out);
                out.push_str(": ");
                write_value(item, depth + 1, out);
                if index + 1 < fields.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            write_indent(out, depth);
            out.push('}');
        }
    }
}

/// Format a float the way Python prints one in json output, with
/// a trailing .0 on a whole value and the shortest round trip
/// decimal otherwise.
pub fn py_float(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e16 && value.is_finite() {
        format!("{:.1}", value)
    } else {
        format!("{}", value)
    }
}

/// Escape a string the way Python json escapes it, with every
/// non-ascii character written as a unicode escape.
pub fn write_json_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) < 0x7f => out.push(c),
            c => {
                let n = c as u32;
                if n <= 0xFFFF {
                    out.push_str(&format!("\\u{:04x}", n));
                } else {
                    let v = n - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xD800 + (v >> 10),
                        0xDC00 + (v & 0x3FF)
                    ));
                }
            }
        }
    }
    out.push('"');
}
