//! The small part of TOML that the harness's own files use.

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Arr(Vec<Value>),
    Table(Table),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    entries: Vec<(String, Value)>,
}

impl Table {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    fn insert(&mut self, key: &str, value: Value) -> Result<(), String> {
        if self.get(key).is_some() {
            return Err(format!("`{key}` appears twice in one table"));
        }
        self.entries.push((key.to_string(), value));
        Ok(())
    }

    pub fn table(&self, key: &str) -> Option<&Table> {
        match self.get(key) {
            Some(Value::Table(table)) => Some(table),
            _ => None,
        }
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Value::Str(text)) => Some(text),
            _ => None,
        }
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        match self.get(key) {
            Some(Value::Int(number)) => Some(*number),
            _ => None,
        }
    }

    /// The integers in an array. `None` if it isn't an array of integers.
    pub fn ints(&self, key: &str) -> Option<Vec<i64>> {
        match self.get(key) {
            Some(Value::Arr(items)) => items
                .iter()
                .map(|item| match item {
                    Value::Int(number) => Some(*number),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    }
}

/// Reads a file made only of `[[name]]` tables. An error names its line.
pub fn array_of_tables(text: &str, name: &str) -> Result<Vec<Table>, String> {
    let tables = arrays_of_tables(text, &[name])?;
    Ok(tables.into_iter().map(|(_, table)| table).collect())
}

/// Reads a file made only of `[[name]]` tables, for each name in `names`. Each
/// table comes with its name, in the file's order. An error names its line.
pub fn arrays_of_tables<'a>(
    text: &str,
    names: &[&'a str],
) -> Result<Vec<(&'a str, Table)>, String> {
    let headers: Vec<String> = names.iter().map(|name| format!("`[[{name}]]`")).collect();
    let headers = headers.join(" or ");
    let mut tables: Vec<(&str, Table)> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            let name = names
                .iter()
                .find(|name| line == format!("[[{name}]]"))
                .ok_or_else(|| format!("line {number}: expected {headers}, got `{line}`"))?;
            tables.push((name, Table::default()));
            continue;
        }
        let (_, table) = tables
            .last_mut()
            .ok_or_else(|| format!("line {number}: a key must come after {headers}"))?;
        add_line(table, line).map_err(|e| format!("line {number}: {e}"))?;
    }
    Ok(tables)
}

/// Reads a file of top-level keys, then `[name]` tables. Each table becomes a
/// value under its name. An error names its line.
pub fn document(text: &str) -> Result<Table, String> {
    let mut root = Table::default();
    let mut open: Option<(String, Table)> = None;
    for (index, line) in text.lines().enumerate() {
        let at = |e: String| format!("line {}: {e}", index + 1);
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            let name = line
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
                .filter(|name| is_plain_key(name))
                .ok_or_else(|| at(format!("`{line}` isn't a `[name]` header")))?;
            if let Some((closed, table)) = open.take() {
                root.insert(&closed, Value::Table(table)).map_err(at)?;
            }
            if root.get(name).is_some() {
                return Err(at(format!("`{name}` appears twice in one table")));
            }
            open = Some((name.to_string(), Table::default()));
            continue;
        }
        let table = open.as_mut().map_or(&mut root, |(_, table)| table);
        add_line(table, line).map_err(at)?;
    }
    if let Some((closed, table)) = open {
        root.insert(&closed, Value::Table(table))?;
    }
    Ok(root)
}

/// A string as TOML writes it.
pub fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn is_plain_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn add_line(table: &mut Table, line: &str) -> Result<(), String> {
    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| format!("expected `key = value`, got `{line}`"))?;
    let key = key.trim();
    if !is_plain_key(key) {
        return Err(format!("`{key}` isn't a plain key"));
    }
    table.insert(key, parse_value(value.trim())?)
}

fn parse_value(text: &str) -> Result<Value, String> {
    let (value, rest) = read_value(text)?;
    let rest = rest.trim_start();
    if !rest.is_empty() && !rest.starts_with('#') {
        return Err(format!("unexpected text after the value: `{rest}`"));
    }
    Ok(value)
}

fn read_value(text: &str) -> Result<(Value, &str), String> {
    if let Some(rest) = text.strip_prefix('"') {
        return read_string(rest);
    }
    if let Some(rest) = text.strip_prefix('[') {
        return read_array(rest);
    }
    if let Some(rest) = text.strip_prefix('{') {
        return read_inline_table(rest);
    }
    let end = text
        .find(|c: char| matches!(c, ',' | ']' | '}' | '#') || c.is_whitespace())
        .unwrap_or(text.len());
    let (word, rest) = text.split_at(end);
    let value = match word {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => parse_number(word).ok_or_else(|| format!("`{word}` isn't a supported value"))?,
    };
    Ok((value, rest))
}

/// An integer, or digits on both sides of one `.`. It reads nothing else that
/// `f64` would, such as `inf`.
fn parse_number(word: &str) -> Option<Value> {
    if let Ok(number) = word.parse() {
        return Some(Value::Int(number));
    }
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let (whole, fraction) = word.split_once('.')?;
    if !digits(whole.strip_prefix('-').unwrap_or(whole)) || !digits(fraction) {
        return None;
    }
    let number: f64 = word.parse().ok()?;
    number.is_finite().then_some(Value::Float(number))
}

fn read_inline_table(text: &str) -> Result<(Value, &str), String> {
    let mut table = Table::default();
    let mut rest = text.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix('}') {
            return Ok((Value::Table(table), after));
        }
        let (key, after) = rest
            .split_once('=')
            .ok_or_else(|| "an inline table is never closed".to_string())?;
        let key = key.trim();
        if !is_plain_key(key) {
            return Err(format!("`{key}` isn't a plain key"));
        }
        let (value, after) = read_value(after.trim_start())?;
        table.insert(key, value)?;
        rest = after.trim_start();
        if let Some(after) = rest.strip_prefix(',') {
            rest = after.trim_start();
        } else if !rest.starts_with('}') {
            return Err("expected `,` or `}` in an inline table".to_string());
        }
    }
}

fn read_string(text: &str) -> Result<(Value, &str), String> {
    let mut value = String::new();
    let mut chars = text.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return Ok((Value::Str(value), &text[at + 1..])),
            '\\' => match chars.next() {
                Some((_, '"')) => value.push('"'),
                Some((_, '\\')) => value.push('\\'),
                Some((_, 'n')) => value.push('\n'),
                Some((_, 't')) => value.push('\t'),
                _ => return Err("unsupported escape in a string".to_string()),
            },
            c => value.push(c),
        }
    }
    Err("a string is never closed".to_string())
}

fn read_array(text: &str) -> Result<(Value, &str), String> {
    let mut items = Vec::new();
    let mut rest = text.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix(']') {
            return Ok((Value::Arr(items), after));
        }
        if rest.is_empty() {
            return Err("an array is never closed".to_string());
        }
        let (item, after) = read_value(rest)?;
        items.push(item);
        rest = after.trim_start();
        if let Some(after) = rest.strip_prefix(',') {
            rest = after.trim_start();
        } else if !rest.starts_with(']') {
            return Err("expected `,` or `]` in an array".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Value, array_of_tables, arrays_of_tables, document, quoted};

    #[test]
    fn reads_tables_of_several_names_in_file_order() {
        let text = "[[step]]\nid = 1\n[[milestone]]\nname = \"m\"\n[[step]]\nid = 2\n";
        let tables = arrays_of_tables(text, &["milestone", "step"]).unwrap();
        let names: Vec<&str> = tables.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["step", "milestone", "step"]);
        assert_eq!(tables[0].1.int("id"), Some(1));
        assert_eq!(tables[1].1.str("name"), Some("m"));
        assert_eq!(tables[2].1.int("id"), Some(2));
    }

    #[test]
    fn a_header_outside_the_names_is_an_error_that_lists_them() {
        let error = arrays_of_tables("[[stage]]\n", &["milestone", "step"]).unwrap_err();
        assert_eq!(
            error,
            "line 1: expected `[[milestone]]` or `[[step]]`, got `[[stage]]`"
        );
        let error = arrays_of_tables("[step]\n", &["step"]).unwrap_err();
        assert_eq!(error, "line 1: expected `[[step]]`, got `[step]`");
    }

    #[test]
    fn a_key_before_any_header_is_an_error_that_lists_the_names() {
        let error = arrays_of_tables("id = 1\n", &["milestone", "step"]).unwrap_err();
        assert_eq!(
            error,
            "line 1: a key must come after `[[milestone]]` or `[[step]]`"
        );
    }

    #[test]
    fn reads_the_value_types() {
        let text =
            "[[step]]\nid = 3\nname = \"a \\\"b\\\" `c`\" # note\nneeds = [1, 2]\nspend = true\n";
        let tables = array_of_tables(text, "step").unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].int("id"), Some(3));
        assert_eq!(tables[0].str("name"), Some("a \"b\" `c`"));
        assert_eq!(tables[0].ints("needs"), Some(vec![1, 2]));
        assert_eq!(tables[0].get("spend"), Some(&Value::Bool(true)));
        assert_eq!(tables[0].get("missing"), None);
    }

    #[test]
    fn reads_several_tables_with_comments_and_blanks() {
        let text = "# top\n\n[[step]]\nid = 1\n\n# between\n[[step]]\nid = 2\n";
        let tables = array_of_tables(text, "step").unwrap();
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[1].int("id"), Some(2));
    }

    #[test]
    fn an_empty_array_has_no_integers_to_get_wrong() {
        let tables = array_of_tables("[[step]]\nneeds = []\n", "step").unwrap();
        assert_eq!(tables[0].ints("needs"), Some(vec![]));
    }

    #[test]
    fn a_hash_inside_a_string_is_not_a_comment() {
        let tables = array_of_tables("[[step]]\nname = \"a # b\"\n", "step").unwrap();
        assert_eq!(tables[0].str("name"), Some("a # b"));
    }

    #[test]
    fn an_array_of_strings_is_not_integers() {
        let tables = array_of_tables("[[step]]\nneeds = [\"a\"]\n", "step").unwrap();
        assert_eq!(
            tables[0].get("needs"),
            Some(&Value::Arr(vec![Value::Str("a".to_string())]))
        );
        assert_eq!(tables[0].ints("needs"), None);
    }

    #[test]
    fn errors_name_their_line() {
        for text in [
            "[[other]]\n",
            "[[step]]\nid\n",
            "[[step]]\nid = \"open\n",
            "[[step]]\nid = [1\n",
            "[[step]]\nid = [1 2]\n",
            "[[step]]\nid = 1.5.5\n",
            "[[step]]\nid = inf\n",
            "[[step]]\nid = .5\n",
            "[[step]]\nid = 5.\n",
            "[[step]]\nid = 1 2\n",
            "[[step]]\nid = 1\nid = 2\n",
            "[[step]]\nbad key = 1\n",
            "[[step]]\nid = \"\\x\"\n",
        ] {
            let error = array_of_tables(text, "step").unwrap_err();
            assert!(error.starts_with("line "), "{text:?}: {error}");
        }
    }

    #[test]
    fn a_float_is_not_an_integer() {
        let tables = array_of_tables("[[step]]\nid = 1.5\n", "step").unwrap();
        assert_eq!(tables[0].get("id"), Some(&Value::Float(1.5)));
        assert_eq!(tables[0].int("id"), None);
    }

    #[test]
    fn reads_keys_inline_tables_and_named_tables() {
        let text =
            "a = 1 # note\nspec = { proven = 2, promises = 3.5 }\n\n[one]\nb = \"x\"\n\n[two]\n";
        let root = document(text).unwrap();
        assert_eq!(root.int("a"), Some(1));
        let spec = root.table("spec").unwrap();
        assert_eq!(spec.int("proven"), Some(2));
        assert_eq!(spec.get("promises"), Some(&Value::Float(3.5)));
        assert_eq!(root.table("one").unwrap().str("b"), Some("x"));
        assert_eq!(root.table("two").unwrap().entries().count(), 0);
    }

    #[test]
    fn document_errors_name_their_line() {
        for text in [
            "[[a]]\n",
            "[a b]\n",
            "[a\n",
            "[a]\n[a]\n",
            "a = 1\n[a]\n",
            "a = 1\na = 2\n",
            "a\n",
            "a = { b = 1\n",
            "a = { b = 1 c = 2 }\n",
            "a = { b = 1, b = 2 }\n",
            "a = { b c = 1 }\n",
        ] {
            let error = document(text).unwrap_err();
            assert!(error.starts_with("line "), "{text:?}: {error}");
        }
    }

    #[test]
    fn a_quoted_string_reads_back() {
        let text = "a \"b\" \\ c\nd\te";
        let root = document(&format!("x = {}\n", quoted(text))).unwrap();
        assert_eq!(root.str("x"), Some(text));
    }

    #[test]
    fn a_key_before_any_table_is_an_error() {
        assert!(array_of_tables("id = 1\n", "step").is_err());
    }
}
