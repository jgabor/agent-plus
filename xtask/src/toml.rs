//! The small part of TOML that the harness's own files use.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    Arr(Vec<Value>),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    entries: Vec<(String, Value)>,
}

impl Table {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
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
    let header = format!("[[{name}]]");
    let mut tables: Vec<Table> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if line != header {
                return Err(format!("line {number}: expected `{header}`, got `{line}`"));
            }
            tables.push(Table::default());
            continue;
        }
        let table = tables
            .last_mut()
            .ok_or_else(|| format!("line {number}: a key must come after `{header}`"))?;
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {number}: expected `key = value`, got `{line}`"))?;
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("line {number}: `{key}` isn't a plain key"));
        }
        if table.get(key).is_some() {
            return Err(format!("line {number}: `{key}` appears twice in one table"));
        }
        let value = parse_value(value.trim()).map_err(|e| format!("line {number}: {e}"))?;
        table.entries.push((key.to_string(), value));
    }
    Ok(tables)
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
    let end = text
        .find(|c: char| c == ',' || c == ']' || c == '#' || c.is_whitespace())
        .unwrap_or(text.len());
    let (word, rest) = text.split_at(end);
    let value = match word {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => Value::Int(
            word.parse()
                .map_err(|_| format!("`{word}` isn't a supported value"))?,
        ),
    };
    Ok((value, rest))
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
    use super::{Value, array_of_tables};

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
            "[[step]]\nid = 1.5\n",
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
    fn a_key_before_any_table_is_an_error() {
        assert!(array_of_tables("id = 1\n", "step").is_err());
    }
}
