//! A small, dependency-free JSON value with a strict parser and a deterministic writer.
//!
//! The bridge talks to SolidCraft, Inventor exports and Laser Cut Workbench through JSON only, and
//! must build offline and for WebAssembly, so it carries its own JSON rather than pulling in a
//! dependency tree. Objects keep their insertion order so generated scripts diff cleanly.

use std::fmt::{self, Write as _};

/// A JSON value. Objects keep insertion order.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Json {
    #[default]
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

/// A parse failure with the byte offset where it happened.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON error at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Deepest nesting accepted by the parser (hostile input must not overflow the stack).
const MAX_DEPTH: usize = 512;

impl Json {
    /// An empty object.
    pub fn obj() -> Json {
        Json::Obj(Vec::new())
    }

    /// An empty array.
    pub fn arr() -> Json {
        Json::Arr(Vec::new())
    }

    /// Parse a complete JSON document (a UTF-8 BOM is accepted and skipped).
    pub fn parse(text: &str) -> Result<Json, ParseError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut p = Parser { s: text.as_bytes(), i: 0, depth: 0 };
        p.ws();
        let v = p.value()?;
        p.ws();
        if p.i != p.s.len() {
            return Err(p.err("trailing characters after the JSON value"));
        }
        Ok(v)
    }

    /// Builder: set `key` on an object (replacing an existing key). Non-objects become objects.
    pub fn with(mut self, key: &str, value: impl Into<Json>) -> Json {
        self.set(key, value);
        self
    }

    /// Set `key` on an object, replacing an existing key in place.
    pub fn set(&mut self, key: &str, value: impl Into<Json>) {
        let value = value.into();
        if !matches!(self, Json::Obj(_)) {
            *self = Json::obj();
        }
        if let Json::Obj(items) = self {
            if let Some(slot) = items.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
            } else {
                items.push((key.to_string(), value));
            }
        }
    }

    /// Append to an array. Non-arrays become arrays.
    pub fn push(&mut self, value: impl Into<Json>) {
        if !matches!(self, Json::Arr(_)) {
            *self = Json::arr();
        }
        if let Json::Arr(items) = self {
            items.push(value.into());
        }
    }

    /// Remove `key` from an object, returning its value.
    pub fn remove(&mut self, key: &str) -> Option<Json> {
        if let Json::Obj(items) = self {
            let at = items.iter().position(|(k, _)| k == key)?;
            return Some(items.remove(at).1);
        }
        None
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Obj(items) => items.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Follow a `/`-separated path of object keys and array indices ("a/0/b").
    pub fn pointer(&self, path: &str) -> Option<&Json> {
        let mut cur = self;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = match cur {
                Json::Obj(_) => cur.get(part)?,
                Json::Arr(items) => items.get(part.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }
    /// A non-negative integer that fits in u64 exactly.
    pub fn as_u64(&self) -> Option<u64> {
        let n = self.as_f64()?;
        (n >= 0.0 && n.fract() == 0.0 && n <= 9.007_199_254_740_992e15).then_some(n as u64)
    }

    /// Convenience accessors on an object's member.
    pub fn str_of(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Json::as_str)
    }
    pub fn f64_of(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(Json::as_f64)
    }
    pub fn bool_of(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(Json::as_bool)
    }
    pub fn arr_of(&self, key: &str) -> &[Json] {
        self.get(key).and_then(Json::as_array).unwrap_or(&[])
    }
    /// A fixed-size numeric array member (e.g. a point).
    pub fn vec_of<const N: usize>(&self, key: &str) -> Option<[f64; N]> {
        to_array::<N>(self.get(key)?)
    }

    /// Compact output.
    pub fn to_compact(&self) -> String {
        let mut s = String::new();
        write_value(&mut s, self, None, 0);
        s
    }

    /// Indented output (two spaces), with short numeric arrays kept on one line.
    pub fn to_pretty(&self) -> String {
        let mut s = String::new();
        write_value(&mut s, self, Some(2), 0);
        s
    }
}

/// A numeric JSON array as a fixed-size array.
pub fn to_array<const N: usize>(v: &Json) -> Option<[f64; N]> {
    let items = v.as_array()?;
    if items.len() != N {
        return None;
    }
    let mut out = [0.0; N];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item.as_f64().filter(|x| x.is_finite())?;
    }
    Some(out)
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_compact())
    }
}

macro_rules! from_num {
    ($($t:ty),*) => {$(impl From<$t> for Json { fn from(v: $t) -> Json { Json::Num(v as f64) } })*};
}
from_num!(f64, f32, i32, i64, u32, u64, usize);

impl From<bool> for Json {
    fn from(v: bool) -> Json {
        Json::Bool(v)
    }
}
impl From<&str> for Json {
    fn from(v: &str) -> Json {
        Json::Str(v.to_string())
    }
}
impl From<String> for Json {
    fn from(v: String) -> Json {
        Json::Str(v)
    }
}
impl From<&String> for Json {
    fn from(v: &String) -> Json {
        Json::Str(v.clone())
    }
}
impl<T: Into<Json>> From<Vec<T>> for Json {
    fn from(v: Vec<T>) -> Json {
        Json::Arr(v.into_iter().map(Into::into).collect())
    }
}
impl<T: Into<Json> + Copy, const N: usize> From<[T; N]> for Json {
    fn from(v: [T; N]) -> Json {
        Json::Arr(v.iter().map(|x| (*x).into()).collect())
    }
}
impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(v: Option<T>) -> Json {
        v.map_or(Json::Null, Into::into)
    }
}

/// Build an object: `obj!{"a" => 1, "b" => "x"}`.
#[macro_export]
macro_rules! obj {
    () => { $crate::Json::obj() };
    ($($k:expr => $v:expr),+ $(,)?) => {{
        let mut o = $crate::Json::obj();
        $( o.set($k, $v); )+
        o
    }};
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    depth: usize,
}

impl Parser<'_> {
    fn err(&self, m: &str) -> ParseError {
        ParseError { offset: self.i, message: m.to_string() }
    }
    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.s.get(self.i) {
            self.i += 1;
        }
    }
    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }
    fn value(&mut self) -> Result<Json, ParseError> {
        match self.s.get(self.i) {
            None => Err(self.err("unexpected end of input")),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') if self.eat("true") => Ok(Json::Bool(true)),
            Some(b'f') if self.eat("false") => Ok(Json::Bool(false)),
            Some(b'n') if self.eat("null") => Ok(Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }
    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        Ok(())
    }
    fn object(&mut self) -> Result<Json, ParseError> {
        self.enter()?;
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.eat("}") {
            self.depth -= 1;
            return Ok(Json::Obj(items));
        }
        loop {
            self.ws();
            if self.s.get(self.i) != Some(&b'"') {
                return Err(self.err("expected a string key"));
            }
            let k = self.string()?;
            self.ws();
            if !self.eat(":") {
                return Err(self.err("expected ':'"));
            }
            self.ws();
            let v = self.value()?;
            // Last duplicate wins, as most parsers do.
            if let Some(slot) = items.iter_mut().find(|(key, _): &&mut (String, Json)| *key == k) {
                slot.1 = v;
            } else {
                items.push((k, v));
            }
            self.ws();
            if self.eat(",") {
                continue;
            }
            if self.eat("}") {
                break;
            }
            return Err(self.err("expected ',' or '}'"));
        }
        self.depth -= 1;
        Ok(Json::Obj(items))
    }
    fn array(&mut self) -> Result<Json, ParseError> {
        self.enter()?;
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.eat("]") {
            self.depth -= 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            if self.eat(",") {
                continue;
            }
            if self.eat("]") {
                break;
            }
            return Err(self.err("expected ',' or ']'"));
        }
        self.depth -= 1;
        Ok(Json::Arr(items))
    }
    fn hex4(&mut self) -> Result<u32, ParseError> {
        let h = self.s.get(self.i..self.i + 4).ok_or_else(|| self.err("short \\u escape"))?;
        let h = std::str::from_utf8(h).map_err(|_| self.err("bad \\u escape"))?;
        let v = u32::from_str_radix(h, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }
    fn string(&mut self) -> Result<String, ParseError> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let start = self.i;
            while let Some(&b) = self.s.get(self.i) {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.i += 1;
            }
            out.push_str(std::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err("invalid UTF-8"))?);
            match self.s.get(self.i) {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = *self.s.get(self.i).ok_or_else(|| self.err("unterminated escape"))?;
                    self.i += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                if !self.eat("\\u") {
                                    return Err(self.err("lone surrogate"));
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(self.err("bad surrogate pair"));
                                }
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            }
                            out.push(char::from_u32(cp).ok_or_else(|| self.err("bad code point"))?);
                        }
                        _ => return Err(self.err("unknown escape")),
                    }
                }
                Some(_) => return Err(self.err("control character in string")),
            }
        }
    }
    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.i;
        if self.s.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        while let Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') = self.s.get(self.i) {
            self.i += 1;
        }
        let t = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err("bad number"))?;
        let n: f64 = t.parse().map_err(|_| ParseError { offset: start, message: format!("bad number `{t}`") })?;
        if !n.is_finite() {
            return Err(ParseError { offset: start, message: "number out of range".into() });
        }
        Ok(Json::Num(n))
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Shortest round-tripping text for a finite number; integers without a fraction.
pub fn format_number(n: f64) -> String {
    if !n.is_finite() {
        return "null".into();
    }
    if n == 0.0 {
        return "0".into();
    }
    if n.fract() == 0.0 && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }
    format!("{n}")
}

fn is_flat(items: &[Json]) -> bool {
    items.len() <= 16 && items.iter().all(|v| matches!(v, Json::Num(_) | Json::Bool(_) | Json::Null))
}

fn write_value(out: &mut String, v: &Json, indent: Option<usize>, level: usize) {
    let nl = |out: &mut String, lvl: usize| {
        if let Some(w) = indent {
            out.push('\n');
            out.extend(std::iter::repeat_n(' ', w * lvl));
        }
    };
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => out.push_str(&format_number(*n)),
        Json::Str(s) => write_str(out, s),
        Json::Arr(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            let flat = indent.is_none() || is_flat(items);
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if flat && indent.is_some() {
                        out.push(' ');
                    }
                }
                if !flat {
                    nl(out, level + 1);
                }
                write_value(out, item, indent, level + 1);
            }
            if !flat {
                nl(out, level);
            }
            out.push(']');
        }
        Json::Obj(items) => {
            if items.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                nl(out, level + 1);
                write_str(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, item, indent, level + 1);
            }
            nl(out, level);
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let t = r#"{"a":[1,2.5,-3e-7],"b":{"c":"x\"y\u00e9\ud83d\ude00"},"d":null,"e":true}"#;
        let v = Json::parse(t).unwrap();
        assert_eq!(Json::parse(&v.to_compact()).unwrap(), v);
        assert_eq!(Json::parse(&v.to_pretty()).unwrap(), v);
        assert_eq!(v.pointer("b/c").and_then(Json::as_str), Some("x\"yé😀"));
        assert_eq!(v.pointer("a/1").and_then(Json::as_f64), Some(2.5));
    }

    #[test]
    fn rejects_bad_input() {
        for bad in ["", "{", "[1,]", "{\"a\" 1}", "01x", "\"\\q\"", "[1] 2", "nul"] {
            assert!(Json::parse(bad).is_err(), "{bad}");
        }
        let deep = "[".repeat(10_000);
        assert!(Json::parse(&deep).is_err());
    }

    #[test]
    fn builds_objects() {
        let v = obj! {"x" => 1, "y" => vec![1.5, 2.0], "z" => "s"};
        assert_eq!(v.to_compact(), r#"{"x":1,"y":[1.5,2],"z":"s"}"#);
        assert_eq!(v.vec_of::<2>("y"), Some([1.5, 2.0]));
    }
}
