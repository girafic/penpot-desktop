//! Cognitect Transit (JSON, non-verbose) reader/writer.
//!
//! Implements the subset needed to round-trip Penpot `.penpot` archives.
//! Spec: <https://github.com/cognitect/transit-format>
//!
//! Wire highlights:
//! - Maps encoded as arrays with sentinel: `["^ ", "k1", v1, "k2", v2, ...]`
//! - Cache codes (`^N`) replace repeated keys/tags using base-44 alphabet
//!   `!`–`~`. Strings of length ≥4 enter the cache on first emission.
//! - Scalar tags via `~`-prefix: `~:keyword`, `~$symbol`, `~uUUID`,
//!   `~iLargeInt`, `~mEpochMillis`, `~tISO8601`, `~?T/F`, `~bBASE64`,
//!   `~cChar`, `~~text` (escape `~`), `~^text` (escape `^`).
//! - Composite tags via `{"~#tag": rep}`: `~#set`, `~#list`, `~#cmap`,
//!   plus Penpot-specific `~#point`, `~#matrix`, `~#ordered-set`.

use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::Value as JsonValue;

const CACHE_BASE: u8 = b'!';
const CACHE_DIGITS: usize = 44;
const CACHE_SIZE: usize = CACHE_DIGITS * CACHE_DIGITS;
const MIN_SIZE_CACHEABLE: usize = 4;

/// Penpot/transit value tree.
///
/// Modelled to round-trip Penpot data without losing tag information.
/// Convert to/from `serde_json::Value` via [`Value::to_json`] /
/// [`Value::from_json`] when interfacing with non-transit code paths.
#[derive(Clone, Debug)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<str>),
    Keyword(Rc<str>),
    Symbol(Rc<str>),
    Uuid(uuid::Uuid),
    Inst(chrono::DateTime<chrono::Utc>),
    Vec(Vec<Value>),
    Set(Vec<Value>),
    OrderedSet(Vec<Value>),
    /// Pairs preserved in insertion order.
    Map(Vec<(Value, Value)>),
    Point { x: f64, y: f64 },
    Matrix([f64; 6]),
    /// Tag we don't model explicitly. `tag` is stored without the `~#` prefix.
    Tagged { tag: Rc<str>, rep: Box<Value> },
}

// Helper accessors are part of the public API even though the current
// callers don't exercise all of them — they're required for the Phase
// 5 transit-on-the-wire path and for tests.
#[allow(dead_code)]
impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) | Value::Keyword(s) | Value::Symbol(s) => Some(s.as_ref()),
            _ => None,
        }
    }

    pub fn as_keyword(&self) -> Option<&str> {
        match self {
            Value::Keyword(s) => Some(s.as_ref()),
            _ => None,
        }
    }

    pub fn as_uuid(&self) -> Option<uuid::Uuid> {
        match self {
            Value::Uuid(u) => Some(*u),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&[(Value, Value)]> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_vec(&self) -> Option<&[Value]> {
        match self {
            Value::Vec(v) => Some(v),
            _ => None,
        }
    }

    /// Map-lookup by keyword key (e.g. `"id"` matches `~:id`).
    pub fn get(&self, key: &str) -> Option<&Value> {
        let entries = self.as_map()?;
        for (k, v) in entries {
            if let Some(s) = k.as_keyword() {
                if s == key {
                    return Some(v);
                }
            }
        }
        None
    }

    /// Convert to plain JSON, dropping tag information (keywords lose `:`).
    /// Used to bridge transit data into the JSON-only RPC pipeline.
    pub fn to_json(&self) -> JsonValue {
        match self {
            Value::Nil => JsonValue::Null,
            Value::Bool(b) => JsonValue::Bool(*b),
            Value::Int(i) => JsonValue::Number((*i).into()),
            Value::Float(f) => serde_json::Number::from_f64(*f)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null),
            Value::Str(s) => JsonValue::String(s.to_string()),
            Value::Keyword(s) => JsonValue::String(s.to_string()),
            Value::Symbol(s) => JsonValue::String(s.to_string()),
            Value::Uuid(u) => JsonValue::String(u.to_string()),
            Value::Inst(d) => JsonValue::String(d.to_rfc3339()),
            Value::Vec(items) | Value::Set(items) | Value::OrderedSet(items) => {
                JsonValue::Array(items.iter().map(Value::to_json).collect())
            }
            Value::Map(entries) => {
                let mut obj = serde_json::Map::new();
                for (k, v) in entries {
                    let key = k
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| serde_json::to_string(&k.to_json()).unwrap_or_default());
                    obj.insert(key, v.to_json());
                }
                JsonValue::Object(obj)
            }
            Value::Point { x, y } => JsonValue::Array(vec![
                serde_json::Number::from_f64(*x)
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null),
                serde_json::Number::from_f64(*y)
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null),
            ]),
            Value::Matrix(m) => JsonValue::Array(
                m.iter()
                    .map(|f| {
                        serde_json::Number::from_f64(*f)
                            .map(JsonValue::Number)
                            .unwrap_or(JsonValue::Null)
                    })
                    .collect(),
            ),
            Value::Tagged { rep, .. } => rep.to_json(),
        }
    }

    /// Convert plain JSON into a [`Value`] (everything becomes Str/Map/Vec —
    /// keyword tags etc. are lost). Useful when ingesting non-transit data
    /// and re-emitting it through the transit writer.
    pub fn from_json(v: &JsonValue) -> Self {
        match v {
            JsonValue::Null => Value::Nil,
            JsonValue::Bool(b) => Value::Bool(*b),
            JsonValue::Number(n) => n
                .as_i64()
                .map(Value::Int)
                .or_else(|| n.as_f64().map(Value::Float))
                .unwrap_or(Value::Nil),
            JsonValue::String(s) => Value::Str(Rc::from(s.as_str())),
            JsonValue::Array(items) => Value::Vec(items.iter().map(Value::from_json).collect()),
            JsonValue::Object(obj) => Value::Map(
                obj.iter()
                    .map(|(k, v)| (Value::Str(Rc::from(k.as_str())), Value::from_json(v)))
                    .collect(),
            ),
        }
    }
}

// ───────────────────────── Reader ─────────────────────────

/// Cached entry — keeps both the parsed Value (for direct re-emission)
/// and the original string body (so we can rebuild a string when the
/// receiver asks for it as a different type).
#[derive(Clone)]
struct CacheEntry {
    value: Value,
}

#[derive(Default)]
pub struct Reader {
    cache: Vec<CacheEntry>,
}

impl Reader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, s: &str) -> Result<Value, ReadError> {
        self.cache.clear();
        let parsed: JsonValue = serde_json::from_str(s).map_err(ReadError::Json)?;
        Ok(self.decode(&parsed))
    }

    #[allow(dead_code)]
    pub fn read_value(&mut self, v: &JsonValue) -> Value {
        self.cache.clear();
        self.decode(v)
    }

    fn decode(&mut self, v: &JsonValue) -> Value {
        match v {
            JsonValue::Null => Value::Nil,
            JsonValue::Bool(b) => Value::Bool(*b),
            JsonValue::Number(n) => n
                .as_i64()
                .map(Value::Int)
                .or_else(|| n.as_f64().map(Value::Float))
                .unwrap_or(Value::Nil),
            JsonValue::String(s) => self.decode_str(s),
            JsonValue::Array(arr) => self.decode_arr(arr),
            JsonValue::Object(obj) => self.decode_obj(obj),
        }
    }

    fn decode_str(&mut self, s: &str) -> Value {
        let bytes = s.as_bytes();
        // Cache lookup ^N (but keep "^ " as the map sentinel — handled in decode_arr).
        if bytes.len() >= 2 && bytes[0] == b'^' && bytes[1] != b' ' {
            if let Some(entry) = self.lookup_cache(s) {
                return entry.value;
            }
            // Fall through and treat as a literal string if cache miss.
        }
        if let Some(rest) = s.strip_prefix("~~") {
            return Value::Str(Rc::from(format!("~{rest}").as_str()));
        }
        if let Some(rest) = s.strip_prefix("~^") {
            return Value::Str(Rc::from(format!("^{rest}").as_str()));
        }
        if let Some(rest) = s.strip_prefix('~') {
            let bytes = rest.as_bytes();
            if bytes.is_empty() {
                return Value::Str(Rc::from(s));
            }
            let body = &rest[1..];
            let value = match bytes[0] {
                b':' => Value::Keyword(Rc::from(body)),
                b'$' => Value::Symbol(Rc::from(body)),
                b'?' => Value::Bool(body.starts_with('t')),
                b'u' => match uuid::Uuid::parse_str(body) {
                    Ok(u) => Value::Uuid(u),
                    Err(_) => Value::Str(Rc::from(s)),
                },
                b'i' => body
                    .parse::<i64>()
                    .map(Value::Int)
                    .unwrap_or_else(|_| Value::Str(Rc::from(s))),
                b'n' => body
                    .parse::<i64>()
                    .map(Value::Int)
                    .unwrap_or_else(|_| Value::Str(Rc::from(s))),
                b'd' => body
                    .parse::<f64>()
                    .map(Value::Float)
                    .unwrap_or_else(|_| Value::Str(Rc::from(s))),
                b'm' => body
                    .parse::<i64>()
                    .ok()
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map(Value::Inst)
                    .unwrap_or_else(|| Value::Str(Rc::from(s))),
                b't' => chrono::DateTime::parse_from_rfc3339(body)
                    .ok()
                    .map(|d| Value::Inst(d.into()))
                    .unwrap_or_else(|| Value::Str(Rc::from(s))),
                _ => Value::Tagged {
                    tag: Rc::from(&rest[..1]),
                    rep: Box::new(Value::Str(Rc::from(body))),
                },
            };
            // Tagged scalars whose source string is ≥ MIN_SIZE_CACHEABLE go
            // into the cache so subsequent `^N` references resolve correctly.
            if s.len() >= MIN_SIZE_CACHEABLE {
                self.cache_value(value.clone());
            }
            return value;
        }
        let value = Value::Str(Rc::from(s));
        if s.len() >= MIN_SIZE_CACHEABLE {
            self.cache_value(value.clone());
        }
        value
    }

    fn lookup_cache(&self, code: &str) -> Option<CacheEntry> {
        let bytes = code.as_bytes();
        let idx = if bytes.len() == 2 {
            (bytes[1].checked_sub(CACHE_BASE)? as usize) * CACHE_DIGITS
        } else if bytes.len() == 3 {
            (bytes[1].checked_sub(CACHE_BASE)? as usize) * CACHE_DIGITS
                + (bytes[2].checked_sub(CACHE_BASE)? as usize)
        } else {
            return None;
        };
        self.cache.get(idx).cloned()
    }

    fn cache_value(&mut self, value: Value) {
        if self.cache.len() < CACHE_SIZE {
            self.cache.push(CacheEntry { value });
        }
    }

    fn decode_arr(&mut self, arr: &[JsonValue]) -> Value {
        // Map sentinel
        if let Some(JsonValue::String(s)) = arr.first() {
            if s == "^ " {
                let mut entries = Vec::with_capacity((arr.len() - 1) / 2);
                let mut it = arr[1..].iter();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    let key = self.decode(k);
                    let val = self.decode(v);
                    entries.push((key, val));
                }
                return Value::Map(entries);
            }
        }
        Value::Vec(arr.iter().map(|v| self.decode(v)).collect())
    }

    fn decode_obj(&mut self, obj: &serde_json::Map<String, JsonValue>) -> Value {
        if obj.len() == 1 {
            let (k, v) = obj.iter().next().unwrap();
            if let Some(tag) = k.strip_prefix("~#") {
                let rep = self.decode(v);
                return self.dispatch_tag(tag, rep);
            }
        }
        // Plain JSON object — rare in genuine transit data, but shows up in
        // mixed payloads. Treat keys as strings.
        Value::Map(
            obj.iter()
                .map(|(k, v)| (Value::Str(Rc::from(k.as_str())), self.decode(v)))
                .collect(),
        )
    }

    fn dispatch_tag(&mut self, tag: &str, rep: Value) -> Value {
        match tag {
            "'" => rep,
            "set" => Value::Set(into_vec(rep)),
            "list" => Value::Vec(into_vec(rep)),
            "cmap" => {
                let items = into_vec(rep);
                let mut entries = Vec::with_capacity(items.len() / 2);
                let mut it = items.into_iter();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    entries.push((k, v));
                }
                Value::Map(entries)
            }
            "ordered-set" => Value::OrderedSet(into_vec(rep)),
            "point" => {
                let v = into_vec(rep);
                if v.len() >= 2 {
                    Value::Point {
                        x: as_f64(&v[0]),
                        y: as_f64(&v[1]),
                    }
                } else {
                    Value::Tagged {
                        tag: Rc::from(tag),
                        rep: Box::new(Value::Vec(v)),
                    }
                }
            }
            "matrix" => {
                let v = into_vec(rep);
                if v.len() == 6 {
                    let mut m = [0.0; 6];
                    for (i, x) in v.iter().enumerate() {
                        m[i] = as_f64(x);
                    }
                    Value::Matrix(m)
                } else {
                    Value::Tagged {
                        tag: Rc::from(tag),
                        rep: Box::new(Value::Vec(v)),
                    }
                }
            }
            other => Value::Tagged {
                tag: Rc::from(other),
                rep: Box::new(rep),
            },
        }
    }
}

fn into_vec(v: Value) -> Vec<Value> {
    match v {
        Value::Vec(items) | Value::Set(items) | Value::OrderedSet(items) => items,
        other => vec![other],
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        Value::Int(i) => *i as f64,
        _ => 0.0,
    }
}

// ───────────────────────── Writer ─────────────────────────

#[derive(Default)]
pub struct Writer {
    /// String → cache code (e.g. "^!"). Populated lazily on first emit.
    cache: BTreeMap<String, String>,
    next_index: usize,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode `value` as a transit+JSON string.
    pub fn write(&mut self, value: &Value) -> String {
        self.cache.clear();
        self.next_index = 0;
        let json = self.encode(value);
        json.to_string()
    }

    fn encode(&mut self, value: &Value) -> JsonValue {
        match value {
            Value::Nil => JsonValue::Null,
            Value::Bool(b) => JsonValue::Bool(*b),
            Value::Int(i) => JsonValue::Number((*i).into()),
            Value::Float(f) => serde_json::Number::from_f64(*f)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null),
            Value::Str(s) => JsonValue::String(self.encode_str_literal(s)),
            Value::Keyword(s) => JsonValue::String(self.encode_tag_value(":", s)),
            Value::Symbol(s) => JsonValue::String(format!("~${s}")),
            Value::Uuid(u) => JsonValue::String(format!("~u{}", u)),
            Value::Inst(d) => JsonValue::String(format!("~m{}", d.timestamp_millis())),
            Value::Vec(items) => {
                JsonValue::Array(items.iter().map(|v| self.encode(v)).collect())
            }
            Value::Set(items) => self.tagged_array("set", items),
            Value::OrderedSet(items) => self.tagged_array("ordered-set", items),
            Value::Map(entries) => self.encode_map(entries),
            Value::Point { x, y } => self.tagged_array(
                "point",
                &[Value::Float(*x), Value::Float(*y)],
            ),
            Value::Matrix(m) => self.tagged_array(
                "matrix",
                &m.iter().map(|f| Value::Float(*f)).collect::<Vec<_>>(),
            ),
            Value::Tagged { tag, rep } => {
                let mut obj = serde_json::Map::new();
                obj.insert(format!("~#{tag}"), self.encode(rep));
                JsonValue::Object(obj)
            }
        }
    }

    fn encode_map(&mut self, entries: &[(Value, Value)]) -> JsonValue {
        let mut arr = Vec::with_capacity(1 + entries.len() * 2);
        arr.push(JsonValue::String("^ ".into()));
        for (k, v) in entries {
            arr.push(self.encode(k));
            arr.push(self.encode(v));
        }
        JsonValue::Array(arr)
    }

    fn tagged_array(&mut self, tag: &str, items: &[Value]) -> JsonValue {
        let mut obj = serde_json::Map::new();
        obj.insert(
            format!("~#{tag}"),
            JsonValue::Array(items.iter().map(|v| self.encode(v)).collect()),
        );
        JsonValue::Object(obj)
    }

    /// Encode a tag-prefixed string (e.g. `~:keyword`), with cache substitution
    /// on the *full* tagged form.
    fn encode_tag_value(&mut self, prefix: &str, body: &str) -> String {
        let full = format!("~{prefix}{body}");
        self.cache_or_emit(full)
    }

    fn encode_str_literal(&mut self, s: &str) -> String {
        // Escape ambiguous prefixes
        let escaped = if s.starts_with('~') {
            format!("~~{}", &s[1..])
        } else if s.starts_with('^') && s != "^ " {
            format!("~^{}", &s[1..])
        } else {
            s.to_string()
        };
        if escaped.len() >= MIN_SIZE_CACHEABLE {
            self.cache_or_emit(escaped)
        } else {
            escaped
        }
    }

    fn cache_or_emit(&mut self, s: String) -> String {
        if let Some(code) = self.cache.get(&s) {
            return code.clone();
        }
        if self.next_index < CACHE_SIZE {
            let idx = self.next_index;
            self.next_index += 1;
            let code = encode_cache_index(idx);
            self.cache.insert(s.clone(), code);
        }
        s
    }
}

fn encode_cache_index(idx: usize) -> String {
    let hi = idx / CACHE_DIGITS;
    let lo = idx % CACHE_DIGITS;
    if hi == 0 {
        let mut out = String::with_capacity(2);
        out.push('^');
        out.push((CACHE_BASE + lo as u8) as char);
        out
    } else {
        let mut out = String::with_capacity(3);
        out.push('^');
        out.push((CACHE_BASE + hi as u8) as char);
        out.push((CACHE_BASE + lo as u8) as char);
        out
    }
}

// ───────────────────────── Errors ─────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("invalid JSON: {0}")]
    Json(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keyword() {
        let mut r = Reader::new();
        let v = r.read("\"~:foo\"").unwrap();
        assert!(matches!(v, Value::Keyword(ref s) if s.as_ref() == "foo"));
    }

    #[test]
    fn round_trip_uuid() {
        let mut r = Reader::new();
        let v = r
            .read("\"~u00000000-0000-0000-0000-000000000001\"")
            .unwrap();
        assert!(matches!(v, Value::Uuid(_)));
    }

    #[test]
    fn parse_map_sentinel() {
        let mut r = Reader::new();
        let v = r.read("[\"^ \", \"~:a\", 1, \"~:b\", 2]").unwrap();
        assert!(v.get("a").is_some());
        assert!(v.get("b").is_some());
    }

    #[test]
    fn parse_set() {
        let mut r = Reader::new();
        let v = r.read("{\"~#set\": [1, 2, 3]}").unwrap();
        assert!(matches!(v, Value::Set(ref items) if items.len() == 3));
    }

    #[test]
    fn parse_point_and_matrix() {
        let mut r = Reader::new();
        let p = r.read("{\"~#point\": [1.5, 2.5]}").unwrap();
        assert!(matches!(p, Value::Point { x, y } if x == 1.5 && y == 2.5));
        let m = r.read("{\"~#matrix\": [1, 0, 0, 1, 0, 0]}").unwrap();
        assert!(matches!(m, Value::Matrix(_)));
    }

    #[test]
    fn write_then_read_keyword() {
        let mut w = Writer::new();
        let s = w.write(&Value::Keyword(Rc::from("hello")));
        let mut r = Reader::new();
        let v = r.read(&s).unwrap();
        assert_eq!(v.as_keyword(), Some("hello"));
    }

    #[test]
    fn write_then_read_map_with_keyword_keys() {
        let m = Value::Map(vec![
            (Value::Keyword(Rc::from("id")), Value::Int(7)),
            (
                Value::Keyword(Rc::from("name")),
                Value::Str(Rc::from("foo")),
            ),
        ]);
        let mut w = Writer::new();
        let s = w.write(&m);
        let mut r = Reader::new();
        let v = r.read(&s).unwrap();
        assert_eq!(v.get("id").and_then(|v| match v {
            Value::Int(i) => Some(*i),
            _ => None,
        }), Some(7));
    }

    #[test]
    fn cache_round_trip() {
        // First emission of "longish" repeats — second should be a cache code.
        let arr = Value::Vec(vec![
            Value::Keyword(Rc::from("longish-keyword")),
            Value::Keyword(Rc::from("longish-keyword")),
        ]);
        let mut w = Writer::new();
        let s = w.write(&arr);
        let mut r = Reader::new();
        let v = r.read(&s).unwrap();
        let items = match v {
            Value::Vec(items) => items,
            _ => panic!("not a vec"),
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_keyword(), Some("longish-keyword"));
        assert_eq!(items[1].as_keyword(), Some("longish-keyword"));
    }

    #[test]
    fn escape_tilde_and_caret() {
        let mut w = Writer::new();
        let v = Value::Str(Rc::from("~tilde"));
        let s = w.write(&v);
        let mut r = Reader::new();
        let back = r.read(&s).unwrap();
        assert_eq!(back.as_str(), Some("~tilde"));
    }
}
