//! Converts the ForeverCollect SavedVariables (`ForeverCollectDB = { ... }`) to JSON
//! without running Lua: the dump is read as data, never interpreted.
//!
//! Port of the Go parser the ingress used to run (`foreverdb-server`,
//! `services/ingress/cmd/ingress/lua.go`). Both sides share the same golden fixture:
//! `tests/fixtures/snapshot.lua` and `snapshot.json`; the server's copy of the JSON
//! lives in `services/ingress/cmd/ingress/testdata/`.
//!
//! Two properties must hold, or the worker rejects the snapshot: integral Lua numbers
//! become JSON integers (never `1617.0`), and tables densely indexed from 1 become
//! arrays, all others objects.

use serde_json::{Map, Value};

/// The client is built with `panic = "abort"`, so a stack overflow in the recursive
/// descent would kill the whole app. Real snapshots nest about 6 levels deep.
const MAX_DEPTH: usize = 200;

/// Above this limit an `f64` no longer represents integers exactly.
const MAX_EXACT_INTEGER: f64 = 9_007_199_254_740_992.0;

pub fn parse_forever_collect(source: &[u8]) -> Result<Value, String> {
    let mut parser = Parser::new(source)?;
    match &parser.current {
        Token::Ident(name) if name == "ForeverCollectDB" => {}
        _ => return Err(parser.error("expected the ForeverCollectDB assignment")),
    }
    parser.advance()?;
    parser.expect(&Token::Equals)?;
    parser.advance()?;
    let value = parser.parse_value()?;
    if parser.current != Token::Eof {
        return Err(parser.error("unexpected content after the assignment"));
    }

    let root = match normalize(value)? {
        Value::Object(map) => map,
        _ => return Err("ForeverCollectDB must be a table.".to_string()),
    };
    validate(&root)?;
    Ok(Value::Object(root))
}

fn validate(root: &Map<String, Value>) -> Result<(), String> {
    match root.get("schemaVersion").and_then(Value::as_f64) {
        Some(9.0) => {}
        _ => {
            return Err(
                "Unsupported schemaVersion; only version 9 is accepted. \
                 Please update the ForeverCollect addon."
                    .to_string(),
            )
        }
    }
    let catalogs = match root.get("catalogs") {
        Some(Value::Object(catalogs)) => catalogs.len(),
        // An empty Lua table has no keys and arrives as an empty list.
        Some(Value::Array(entries)) if entries.is_empty() => 0,
        _ => return Err("catalogs must be a table indexed by catalog key.".to_string()),
    };
    if catalogs == 0 {
        return Err(
            "The snapshot contains no catalogs; play a session with the addon \
             before uploading."
                .to_string(),
        );
    }
    if let Some(key) = root.get("latestCatalogKey") {
        if !key.is_string() {
            return Err("latestCatalogKey must be a string.".to_string());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Intermediate representation
// ---------------------------------------------------------------------------

/// A table key. Numeric keys carry their canonical decimal form, because `f64` is
/// neither `Eq` nor `Hash` in Rust — and because exactly this form later becomes the
/// JSON object key.
#[derive(Clone, PartialEq, Eq)]
enum LuaKey {
    Text(String),
    Number(String),
}

enum LuaValue {
    Table(Vec<(LuaKey, LuaValue)>),
    Str(String),
    Number(f64),
    Bool(bool),
    Nil,
}

fn normalize(value: LuaValue) -> Result<Value, String> {
    match value {
        LuaValue::Table(entries) => normalize_table(entries),
        LuaValue::Str(text) => Ok(Value::String(text)),
        LuaValue::Number(number) => number_to_json(number),
        LuaValue::Bool(flag) => Ok(Value::Bool(flag)),
        LuaValue::Nil => Ok(Value::Null),
    }
}

/// Tables densely indexed from 1 become JSON arrays, everything else objects — just
/// like `normalizeLuaValue` on the Go side.
fn normalize_table(entries: Vec<(LuaKey, LuaValue)>) -> Result<Value, String> {
    let mut indices = Vec::with_capacity(entries.len());
    let mut dense = !entries.is_empty();
    let mut maximum: i64 = 0;
    for (key, _) in &entries {
        match key {
            LuaKey::Number(text) => match text.parse::<i64>() {
                Ok(index) if index >= 1 => {
                    indices.push(index);
                    maximum = maximum.max(index);
                }
                _ => {
                    dense = false;
                    break;
                }
            },
            LuaKey::Text(_) => {
                dense = false;
                break;
            }
        }
    }

    // The keys are unique and all >= 1, so `maximum == len` means there are no gaps.
    if dense && maximum == entries.len() as i64 {
        let mut slots: Vec<Option<Value>> = (0..entries.len()).map(|_| None).collect();
        for ((_, value), index) in entries.into_iter().zip(indices) {
            slots[(index - 1) as usize] = Some(normalize(value)?);
        }
        return Ok(Value::Array(slots.into_iter().flatten().collect()));
    }

    let mut object = Map::new();
    for (key, value) in entries {
        let name = match key {
            LuaKey::Text(text) | LuaKey::Number(text) => text,
        };
        object.insert(name, normalize(value)?);
    }
    Ok(Value::Object(object))
}

/// Integral values become JSON integers. This is not cosmetic: the worker unmarshals
/// fields such as `questID` or `interfaceVersion` into Go `int`, and `encoding/json`
/// fails hard on `1617.0`.
fn number_to_json(number: f64) -> Result<Value, String> {
    if !number.is_finite() {
        return Err("number out of range.".to_string());
    }
    if number.fract() == 0.0 && number.abs() <= MAX_EXACT_INTEGER {
        return Ok(Value::Number((number as i64).into()));
    }
    serde_json::Number::from_f64(number)
        .map(Value::Number)
        .ok_or_else(|| "number out of range.".to_string())
}

/// Canonical decimal form of a number as it appears as an object key. Rust's
/// `Display` for `f64` behaves like Go's `FormatFloat(n, 'f', -1, 64)` here: the
/// shortest round-tripping form, never exponent notation, no `.0` on integers.
fn format_number(number: f64) -> String {
    format!("{number}")
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq)]
enum Token {
    Eof,
    Ident(String),
    Str(String),
    Number(f64),
    Bool(bool),
    Nil,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Equals,
    Comma,
    Semicolon,
}

impl Token {
    fn label(&self) -> &'static str {
        match self {
            Token::Eof => "end of file",
            Token::Ident(_) => "identifier",
            Token::Str(_) => "string",
            Token::Number(_) => "number",
            Token::Bool(_) => "boolean",
            Token::Nil => "nil",
            Token::LBrace => "{",
            Token::RBrace => "}",
            Token::LBracket => "[",
            Token::RBracket => "]",
            Token::Equals => "=",
            Token::Comma => ",",
            Token::Semicolon => ";",
        }
    }
}

struct Parser<'a> {
    lexer: Lexer<'a>,
    current: Token,
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a [u8]) -> Result<Self, String> {
        let mut parser = Parser {
            lexer: Lexer { source, pos: 0 },
            current: Token::Eof,
            pos: 0,
            depth: 0,
        };
        parser.advance()?;
        Ok(parser)
    }

    fn advance(&mut self) -> Result<(), String> {
        let (token, pos) = self.lexer.next()?;
        self.current = token;
        self.pos = pos;
        Ok(())
    }

    fn expect(&self, kind: &Token) -> Result<(), String> {
        if std::mem::discriminant(&self.current) == std::mem::discriminant(kind) {
            return Ok(());
        }
        Err(self.error(&format!("expected {}", kind.label())))
    }

    fn error(&self, message: &str) -> String {
        format!("Lua error at byte {}: {message}", self.pos)
    }

    fn parse_value(&mut self) -> Result<LuaValue, String> {
        match self.current.clone() {
            Token::LBrace => self.parse_table(),
            Token::Str(text) => {
                self.advance()?;
                Ok(LuaValue::Str(text))
            }
            Token::Number(number) => {
                self.advance()?;
                Ok(LuaValue::Number(number))
            }
            Token::Bool(flag) => {
                self.advance()?;
                Ok(LuaValue::Bool(flag))
            }
            Token::Nil => {
                self.advance()?;
                Ok(LuaValue::Nil)
            }
            _ => Err(self.error("expected a Lua literal")),
        }
    }

    fn parse_table(&mut self) -> Result<LuaValue, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("the snapshot is nested too deeply"));
        }
        self.advance()?;
        let mut entries: Vec<(LuaKey, LuaValue)> = Vec::new();
        let mut next_index = 1.0_f64;

        while self.current != Token::RBrace {
            if self.current == Token::Eof {
                return Err(self.error("unterminated table"));
            }

            let (key, value) = match self.current.clone() {
                Token::LBracket => {
                    self.advance()?;
                    let key = self.parse_value()?;
                    let key = make_key(&key).map_err(|e| self.error(&e))?;
                    self.expect(&Token::RBracket)?;
                    self.advance()?;
                    self.expect(&Token::Equals)?;
                    self.advance()?;
                    (key, self.parse_value()?)
                }
                Token::Ident(name) => {
                    self.advance()?;
                    self.expect(&Token::Equals)?;
                    self.advance()?;
                    (LuaKey::Text(name), self.parse_value()?)
                }
                _ => {
                    let key = LuaKey::Number(format_number(next_index));
                    next_index += 1.0;
                    (key, self.parse_value()?)
                }
            };
            set_entry(&mut entries, key, value);

            if self.current == Token::Comma || self.current == Token::Semicolon {
                self.advance()?;
            } else if self.current != Token::RBrace {
                return Err(self.error("expected a table separator"));
            }
        }
        self.advance()?;
        self.depth -= 1;
        Ok(LuaValue::Table(entries))
    }
}

/// Duplicate keys overwrite each other as in a Lua table: the last one wins, the
/// insertion position is kept.
fn set_entry(entries: &mut Vec<(LuaKey, LuaValue)>, key: LuaKey, value: LuaValue) {
    if let Some(slot) = entries.iter_mut().find(|(existing, _)| *existing == key) {
        slot.1 = value;
        return;
    }
    entries.push((key, value));
}

fn make_key(value: &LuaValue) -> Result<LuaKey, String> {
    match value {
        LuaValue::Str(text) => Ok(LuaKey::Text(text.clone())),
        LuaValue::Number(number) => {
            if !number.is_finite() {
                return Err("invalid table key: number out of range".to_string());
            }
            Ok(LuaKey::Number(format_number(*number)))
        }
        _ => Err("invalid table key: keys must be strings or numbers".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

struct Lexer<'a> {
    source: &'a [u8],
    pos: usize,
}

impl Lexer<'_> {
    fn next(&mut self) -> Result<(Token, usize), String> {
        self.skip_space_and_comments();
        if self.pos >= self.source.len() {
            return Ok((Token::Eof, self.pos));
        }
        let start = self.pos;
        let character = self.source[self.pos];
        self.pos += 1;
        let token = match character {
            b'{' => Token::LBrace,
            b'}' => Token::RBrace,
            b'[' => Token::LBracket,
            b']' => Token::RBracket,
            b'=' => Token::Equals,
            b',' => Token::Comma,
            b';' => Token::Semicolon,
            b'"' | b'\'' => Token::Str(self.read_string(character, start)?),
            _ if is_identifier_start(character) => {
                while self.pos < self.source.len() && is_identifier_part(self.source[self.pos]) {
                    self.pos += 1;
                }
                match &self.source[start..self.pos] {
                    b"true" => Token::Bool(true),
                    b"false" => Token::Bool(false),
                    b"nil" => Token::Nil,
                    name => Token::Ident(String::from_utf8_lossy(name).into_owned()),
                }
            }
            _ if character == b'-' || character.is_ascii_digit() => {
                while self.pos < self.source.len()
                    && b"0123456789.eE+-".contains(&self.source[self.pos])
                {
                    self.pos += 1;
                }
                let text = String::from_utf8_lossy(&self.source[start..self.pos]).into_owned();
                match text.parse::<f64>() {
                    Ok(number) => Token::Number(number),
                    Err(_) => {
                        return Err(format!("Lua error at byte {start}: invalid number {text:?}"))
                    }
                }
            }
            _ => {
                return Err(format!(
                    "Lua error at byte {start}: unsupported character {:?}",
                    character as char
                ))
            }
        };
        Ok((token, start))
    }

    /// Lua strings are byte sequences: `\ddd` can produce arbitrary bytes. At the end
    /// `from_utf8_lossy` replaces invalid sequences with U+FFFD — the same thing Go's
    /// `json.Marshal` does with invalid UTF-8.
    fn read_string(&mut self, quote: u8, start: usize) -> Result<String, String> {
        let mut value: Vec<u8> = Vec::new();
        while self.pos < self.source.len() {
            let character = self.source[self.pos];
            self.pos += 1;
            if character == quote {
                return Ok(String::from_utf8_lossy(&value).into_owned());
            }
            if character != b'\\' {
                value.push(character);
                continue;
            }
            if self.pos >= self.source.len() {
                break;
            }
            let escaped = self.source[self.pos];
            self.pos += 1;
            match escaped {
                b'a' => value.push(0x07),
                b'b' => value.push(0x08),
                b'f' => value.push(0x0c),
                b'n' => value.push(b'\n'),
                b'r' => value.push(b'\r'),
                b't' => value.push(b'\t'),
                b'v' => value.push(0x0b),
                b'\\' | b'"' | b'\'' => value.push(escaped),
                b'z' => {
                    while self.pos < self.source.len() && is_space(self.source[self.pos]) {
                        self.pos += 1;
                    }
                }
                b'0'..=b'9' => {
                    let mut end = self.pos;
                    while end < self.source.len()
                        && end < self.pos + 2
                        && self.source[end].is_ascii_digit()
                    {
                        end += 1;
                    }
                    let mut digits = vec![escaped];
                    digits.extend_from_slice(&self.source[self.pos..end]);
                    let text = String::from_utf8_lossy(&digits);
                    match text.parse::<u16>() {
                        Ok(number) if number <= 255 => {
                            self.pos = end;
                            value.push(number as u8);
                        }
                        _ => {
                            return Err(format!(
                                "Lua error at byte {start}: invalid string escape sequence"
                            ))
                        }
                    }
                }
                _ => {
                    return Err(format!(
                        "Lua error at byte {start}: unsupported string escape sequence"
                    ))
                }
            }
        }
        Err(format!(
            "Lua error at byte {start}: unterminated string"
        ))
    }

    fn skip_space_and_comments(&mut self) {
        if self.pos == 0 && self.source.starts_with(b"\xef\xbb\xbf") {
            self.pos = 3;
        }
        loop {
            while self.pos < self.source.len() && is_space(self.source[self.pos]) {
                self.pos += 1;
            }
            if self.pos + 1 >= self.source.len()
                || self.source[self.pos] != b'-'
                || self.source[self.pos + 1] != b'-'
            {
                return;
            }
            self.pos += 2;
            while self.pos < self.source.len() && self.source[self.pos] != b'\n' {
                self.pos += 1;
            }
        }
    }
}

fn is_space(character: u8) -> bool {
    matches!(character, b' ' | b'\t' | b'\n' | b'\r' | 0x0c | 0x0b)
}

fn is_identifier_start(character: u8) -> bool {
    character == b'_' || character.is_ascii_alphabetic()
}

fn is_identifier_part(character: u8) -> bool {
    is_identifier_start(character) || character.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(body: &str) -> String {
        format!(
            "ForeverCollectDB = {{\n[\"schemaVersion\"] = 9,\n[\"catalogs\"] = {{\n{body}\n}},\n}}"
        )
    }

    fn parse(source: &str) -> Result<Value, String> {
        parse_forever_collect(source.as_bytes())
    }

    /// Only the intermediate representation, without the snapshot validation around it.
    fn parse_raw(source: &str) -> Result<Value, String> {
        let mut parser = Parser::new(source.as_bytes())?;
        parser.advance()?;
        parser.expect(&Token::Equals)?;
        parser.advance()?;
        normalize(parser.parse_value()?)
    }

    #[test]
    fn parses_a_snapshot() {
        let value = parse(&valid("[\"2:11509:2:enUS:8\"] = { [\"projectID\"] = 2 },")).unwrap();
        assert_eq!(value["schemaVersion"], 9);
        assert_eq!(value["catalogs"]["2:11509:2:enUS:8"]["projectID"], 2);
    }

    #[test]
    fn rejects_an_unsupported_schema_version() {
        let source = "ForeverCollectDB = {\n[\"schemaVersion\"] = 8,\n[\"catalogs\"] = { [\"k\"] = {} },\n}";
        let error = parse(source).unwrap_err();
        assert!(error.contains("schemaVersion"), "{error}");
    }

    #[test]
    fn rejects_a_snapshot_without_catalogs() {
        let error = parse("ForeverCollectDB = {\n[\"schemaVersion\"] = 9,\n[\"catalogs\"] = {\n},\n}")
            .unwrap_err();
        assert!(error.contains("no catalogs"), "{error}");
    }

    #[test]
    fn rejects_a_non_table_catalogs_field() {
        let error = parse("ForeverCollectDB = {\n[\"schemaVersion\"] = 9,\n[\"catalogs\"] = \"nope\",\n}")
            .unwrap_err();
        assert!(error.contains("catalogs"), "{error}");
    }

    #[test]
    fn rejects_a_non_string_latest_catalog_key() {
        let source = "ForeverCollectDB = {\n[\"schemaVersion\"] = 9,\n[\"latestCatalogKey\"] = 7,\n[\"catalogs\"] = { [\"k\"] = {} },\n}";
        let error = parse(source).unwrap_err();
        assert!(error.contains("latestCatalogKey"), "{error}");
    }

    /// The dump is read as data, never executed: a function call simply is not a
    /// literal and therefore a parse error.
    #[test]
    fn rejects_executable_lua() {
        let error = parse("ForeverCollectDB = { [\"x\"] = os.execute(\"rm -rf /\") }").unwrap_err();
        assert!(error.contains("literal"), "{error}");
    }

    #[test]
    fn rejects_content_after_the_assignment() {
        let error = parse("ForeverCollectDB = { }\nprint(1)").unwrap_err();
        assert!(error.contains("unexpected content"), "{error}");
    }

    /// The worker unmarshals these fields into Go `int`; `143.0` would make it fail hard.
    #[test]
    fn integral_numbers_stay_integers() {
        let value = parse_raw("x = { [\"spellID\"] = 143, [\"scannedAt\"] = 1789482844 }").unwrap();
        let encoded = serde_json::to_string(&value).unwrap();
        assert_eq!(encoded, r#"{"scannedAt":1789482844,"spellID":143}"#);
    }

    #[test]
    fn fractional_numbers_stay_floats() {
        let value = parse_raw("x = { [\"ratio\"] = 0.25, [\"negative\"] = -3 }").unwrap();
        let encoded = serde_json::to_string(&value).unwrap();
        assert_eq!(encoded, r#"{"negative":-3,"ratio":0.25}"#);
    }

    #[test]
    fn rejects_numbers_outside_the_valid_range() {
        let error = parse_raw("x = { 1e999 }").unwrap_err();
        assert!(error.contains("out of range"), "{error}");
    }

    #[test]
    fn numeric_keys_become_strings() {
        let value = parse_raw("x = { [2039] = \"a\", [7] = \"b\" }").unwrap();
        assert_eq!(value["2039"], "a");
        assert_eq!(value["7"], "b");
    }

    #[test]
    fn dense_tables_become_arrays() {
        assert_eq!(parse_raw("x = { true, false }").unwrap(), serde_json::json!([true, false]));
        assert_eq!(
            parse_raw("x = { [1] = \"a\", [2] = \"b\", [3] = \"c\" }").unwrap(),
            serde_json::json!(["a", "b", "c"])
        );
    }

    #[test]
    fn sparse_tables_become_objects() {
        assert_eq!(
            parse_raw("x = { [1] = 1, [3] = 3 }").unwrap(),
            serde_json::json!({ "1": 1, "3": 3 })
        );
        assert_eq!(
            parse_raw("x = { [0] = 1, [1] = 2 }").unwrap(),
            serde_json::json!({ "0": 1, "1": 2 })
        );
        assert_eq!(parse_raw("x = { [1.5] = 1 }").unwrap(), serde_json::json!({ "1.5": 1 }));
    }

    #[test]
    fn empty_tables_become_objects() {
        assert_eq!(parse_raw("x = { }").unwrap(), serde_json::json!({}));
    }

    #[test]
    fn nil_values_become_null() {
        assert_eq!(parse_raw("x = { [\"a\"] = nil }").unwrap(), serde_json::json!({ "a": null }));
    }

    #[test]
    fn later_duplicate_keys_win() {
        assert_eq!(parse_raw("x = { [\"a\"] = 1, [\"a\"] = 2 }").unwrap(), serde_json::json!({ "a": 2 }));
    }

    #[test]
    fn reads_string_escapes() {
        let value = parse_raw(r#"x = { "\065\066", "a\tb", "c\z
            d", "\\", "'" }"#)
            .unwrap();
        assert_eq!(value, serde_json::json!(["AB", "a\tb", "cd", "\\", "'"]));
    }

    #[test]
    fn rejects_out_of_range_decimal_escapes() {
        let error = parse_raw(r#"x = { "\999" }"#).unwrap_err();
        assert!(error.contains("escape"), "{error}");
    }

    #[test]
    fn skips_a_byte_order_mark_and_comments() {
        let source = "\u{feff}-- header\nForeverCollectDB = {\n[\"schemaVersion\"] = 9, -- trailing\n[\"catalogs\"] = { [\"k\"] = {} },\n}";
        assert_eq!(parse(source).unwrap()["schemaVersion"], 9);
    }

    #[test]
    fn identifier_keys_are_supported() {
        assert_eq!(parse_raw("x = { schemaVersion = 9 }").unwrap(), serde_json::json!({ "schemaVersion": 9 }));
    }

    #[test]
    fn semicolons_separate_entries() {
        assert_eq!(parse_raw("x = { 1; 2; 3 }").unwrap(), serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn reports_an_unterminated_table() {
        let error = parse_raw("x = { [\"a\"] = 1,").unwrap_err();
        assert!(error.contains("unterminated table"), "{error}");
    }

    #[test]
    fn reports_an_unterminated_string() {
        let error = parse_raw("x = { \"abc }").unwrap_err();
        assert!(error.contains("unterminated string"), "{error}");
    }

    /// The client is built with `panic = "abort"`: without a depth limit a pathological
    /// file would kill the app through a stack overflow.
    #[test]
    fn rejects_deeply_nested_tables() {
        let source = format!("x = {}{}", "{".repeat(300), "}".repeat(300));
        let error = parse_raw(&source).unwrap_err();
        assert!(error.contains("nested too deeply"), "{error}");
    }

    #[test]
    fn accepts_nesting_within_the_limit() {
        let source = format!("x = {}{}", "{".repeat(50), "}".repeat(50));
        assert!(parse_raw(&source).is_ok());
    }

    /// Compares against the snapshot the Go ingress produced from the same Lua. Both
    /// files are the shared golden fixture; the server's copy of the JSON lives in
    /// `services/ingress/cmd/ingress/testdata/snapshot.json`.
    /// `FOREVERDB_FIXTURE_LUA` / `FOREVERDB_FIXTURE_JSON` override the paths.
    ///
    /// The comparison is on `Value`, not on bytes: Go escapes `<`, `>` and `&` as
    /// `<`/`>`/`&`, serde_json does not. The canonicalization in the ingress makes
    /// exactly this difference irrelevant.
    #[test]
    fn matches_the_go_parser_on_the_golden_fixture() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let fixture = |name: &str, variable: &str| {
            std::env::var_os(variable)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| dir.join(name))
        };
        let lua = fixture("snapshot.lua", "FOREVERDB_FIXTURE_LUA");
        let json = fixture("snapshot.json", "FOREVERDB_FIXTURE_JSON");
        if !lua.is_file() || !json.is_file() {
            eprintln!("golden fixture is not present; skipping");
            return;
        }
        let parsed = parse_forever_collect(&std::fs::read(&lua).unwrap()).unwrap();
        let expected: Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
        assert_eq!(parsed, expected);
    }
}
