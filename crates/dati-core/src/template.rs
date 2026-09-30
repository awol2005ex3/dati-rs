//! The Handlebars-style template engine shared by custom SQL tools and prompts.
//!
//! This is the Java `com.dati.common.template` package: a parser that turns `{{var}}`,
//! [`{{{raw}}}`], `{{#if}}` and `{{#where}}` into a node list, plus two renderers — one that inlines
//! values into text, one that emits `?` placeholders with their bindings for prepared-statement
//! execution.
//! The parser is deliberately context-free: it does not know about SQL quotes or comments, which is
//! why [`find_quoted_variables`] exists as a separate check.

use std::collections::HashMap;

use serde_json::{Map, Value};

/// One node of a parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Text(String),
    Var {
        name: String,
        default: Option<String>,
        raw: bool,
    },
    If {
        condition: String,
        body: Vec<Node>,
    },
    Where {
        body: Vec<Node>,
    },
}

/// A parsed template: the node list, plus the variable names in first-seen order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompiledTemplate {
    nodes: Vec<Node>,
}

impl CompiledTemplate {
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Every variable the template reads, including `{{#if}}` conditions, deduplicated and in the
    /// order they appear — the iteration order Java's `LinkedHashSet` gave.
    pub fn variables(&self) -> Vec<String> {
        let mut vars = Vec::new();
        collect_variables(&self.nodes, &mut vars);
        vars
    }
}

fn collect_variables(nodes: &[Node], vars: &mut Vec<String>) {
    for node in nodes {
        match node {
            Node::Var { name, .. } => push_unique(vars, name),
            Node::If { condition, body } => {
                push_unique(vars, condition);
                collect_variables(body, vars);
            }
            Node::Where { body } => collect_variables(body, vars),
            Node::Text(_) => {}
        }
    }
}

fn push_unique(vars: &mut Vec<String>, name: &str) {
    if !vars.iter().any(|seen| seen == name) {
        vars.push(name.to_string());
    }
}

/// A rendered `?` placeholder and the value bound to it.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamBinding {
    pub name: String,
    pub value: Value,
}

/// SQL text with `?` placeholders plus the bindings that match them one to one.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PreparedSql {
    pub sql: String,
    pub bindings: Vec<ParamBinding>,
}

impl PreparedSql {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            bindings: Vec::new(),
        }
    }
}

/// Template values by name. A JSON `null` reads as absent, the way Java's `null` did: the default
/// wins, and an `{{#if}}` on it goes falsy.
pub type Values<'a> = HashMap<&'a str, &'a Value>;

/// Indexes a JSON argument object for the renderers.
pub fn values_of(args: &'_ Map<String, Value>) -> Values<'_> {
    args.iter()
        .map(|(name, value)| (name.as_str(), value))
        .collect()
}

/// Parses a template; the `Err` carries the message `TemplateParseException` would have had.
pub fn parse(template: &str) -> Result<CompiledTemplate, String> {
    let chars: Vec<char> = template.chars().collect();
    Ok(CompiledTemplate {
        nodes: parse_nodes(&chars, 0, chars.len())?,
    })
}

/// Parses the `[from, to)` slice. Java recursed by re-parsing the body substring; the same slice is
/// rescanned here so a block body goes through the identical algorithm.
fn parse_nodes(chars: &[char], from: usize, to: usize) -> Result<Vec<Node>, String> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut text: Vec<char> = Vec::new();
    let mut i = from;

    while i < to {
        // An escape: \{{{ is the literal {{{, and \{{ is the literal {{.
        if chars[i] == '\\'
            && i + 3 < to
            && chars[i + 1] == '{'
            && chars[i + 2] == '{'
            && chars[i + 3] == '{'
        {
            text.extend(['{', '{', '{']);
            i += 4;
            continue;
        }
        if chars[i] == '\\' && i + 2 < to && chars[i + 1] == '{' && chars[i + 2] == '{' {
            text.extend(['{', '{']);
            i += 3;
            continue;
        }

        if chars[i] == '{' && i + 2 < to && chars[i + 1] == '{' && chars[i + 2] == '{' {
            flush_text(&mut text, &mut nodes);
            let opened = i;
            i += 3;
            let Some(close) = index_of(chars, i, to, &['}', '}', '}']) else {
                return Err(format!("Unclosed '{{{{{{' at position {opened}"));
            };
            let content = trim(&chars[i..close]);
            i = close + 3;
            nodes.push(parse_var(content, true)?);
            continue;
        }

        if chars[i] == '{' && i + 1 < to && chars[i + 1] == '{' {
            flush_text(&mut text, &mut nodes);
            let opened = i;
            i += 2;
            let Some(close) = index_of(chars, i, to, &['}', '}']) else {
                return Err(format!("Unclosed '{{{{' at position {opened}"));
            };
            let content = trim(&chars[i..close]);
            i = close + 2;

            if content.starts_with(&['#']) {
                i = parse_block(chars, content, i, to, &mut nodes)?;
            } else if content.starts_with(&['/']) {
                let tag: String = content.iter().collect();
                return Err(format!("Unexpected closing tag '{{{{{tag}}}}}'"));
            } else {
                nodes.push(parse_var(content, false)?);
            }
            continue;
        }

        text.push(chars[i]);
        i += 1;
    }

    flush_text(&mut text, &mut nodes);
    Ok(nodes)
}

fn parse_block(
    chars: &[char],
    content: &[char],
    pos: usize,
    to: usize,
    nodes: &mut Vec<Node>,
) -> Result<usize, String> {
    let directive = trim(&content[1..]);
    if directive.starts_with(&['i', 'f', ' ']) {
        let condition = trim(&directive[3..]).iter().collect::<String>();
        validate_var_name(&condition, "condition")?;
        return parse_generic_block(
            chars,
            &format!("{{{{#if {condition}}}}}"),
            "{{/if}}",
            "{{#if ",
            pos,
            to,
            Some(condition),
            nodes,
        );
    }
    if directive == "where".chars().collect::<Vec<char>>().as_slice() {
        return parse_generic_block(
            chars,
            "{{#where}}",
            "{{/where}}",
            "{{#where}}",
            pos,
            to,
            None,
            nodes,
        );
    }
    Err(format!(
        "Unknown block directive: '{{{{#{}}}}}'",
        directive.iter().collect::<String>()
    ))
}

// The recursive-descent block parser threads the cursor, the tag text and the node sink through
// every branch, so the wide signature is deliberate rather than a struct waiting to be extracted.
#[allow(clippy::too_many_arguments)]
fn parse_generic_block(
    chars: &[char],
    open_tag: &str,
    end_tag: &str,
    open_pattern: &str,
    pos: usize,
    to: usize,
    condition: Option<String>,
    nodes: &mut Vec<Node>,
) -> Result<usize, String> {
    let end = find_matching_end_tag(chars, pos, to, end_tag, open_pattern)
        .ok_or_else(|| format!("Unclosed '{open_tag}' — missing '{end_tag}'"))?;
    // A block tag on its own line would otherwise leave a blank line in the output.
    let (body_from, body_to) = trim_newlines(&chars[pos..end], pos);
    let body = parse_nodes(chars, body_from, body_to)?;
    nodes.push(match condition {
        Some(condition) => Node::If { condition, body },
        None => Node::Where { body },
    });
    Ok(end + end_tag.chars().count())
}

fn parse_var(content: &[char], raw: bool) -> Result<Node, String> {
    let text: String = content.iter().collect();
    let (name, default) = match text.find(':') {
        Some(colon) => (&text[..colon], Some(text[colon + 1..].to_string())),
        None => (text.as_str(), None),
    };
    validate_var_name(name, "variable")?;
    Ok(Node::Var {
        name: name.to_string(),
        default,
        raw,
    })
}

fn validate_var_name(name: &str, label: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.');
    if valid {
        return Ok(());
    }
    Err(format!(
        "Invalid {label} name: '{name}'. Must match [A-Za-z0-9_.]+"
    ))
}

fn find_matching_end_tag(
    chars: &[char],
    from: usize,
    to: usize,
    end_tag: &str,
    open_pattern: &str,
) -> Option<usize> {
    let end: Vec<char> = end_tag.chars().collect();
    let open: Vec<char> = open_pattern.chars().collect();
    let mut depth = 1usize;
    let mut i = from;
    while i < to {
        // A `\{{` pair is a literal `{{`: it can neither open nor close a block.
        if chars[i] == '\\' && i + 2 < to && chars[i + 1] == '{' && chars[i + 2] == '{' {
            i += 3;
            continue;
        }
        if starts_with(chars, i, to, &open) {
            depth += 1;
            i += 1;
            continue;
        }
        if starts_with(chars, i, to, &end) {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
            i += end.len();
            continue;
        }
        i += 1;
    }
    None
}

fn starts_with(chars: &[char], from: usize, to: usize, needle: &[char]) -> bool {
    from + needle.len() <= to && chars[from..from + needle.len()] == *needle
}

fn index_of(chars: &[char], from: usize, to: usize, needle: &[char]) -> Option<usize> {
    if needle.len() > to - from {
        return None;
    }
    (from..=to - needle.len()).find(|&at| starts_with(chars, at, to, needle))
}

fn flush_text(text: &mut Vec<char>, nodes: &mut Vec<Node>) {
    if !text.is_empty() {
        nodes.push(Node::Text(text.iter().collect()));
        text.clear();
    }
}

/// Java's `String.trim()`: drop the leading and trailing characters that are `<= ' '`.
fn trim(chars: &[char]) -> &[char] {
    let start = chars.iter().position(|ch| *ch > ' ').unwrap_or(chars.len());
    let end = chars
        .iter()
        .rposition(|ch| *ch > ' ')
        .map_or(start, |at| at + 1);
    &chars[start..end]
}

/// Strips only `\n` from both ends of a slice, addressing into `chars` by absolute index.
fn trim_newlines(chars: &[char], base: usize) -> (usize, usize) {
    let mut start = 0usize;
    while start < chars.len() && chars[start] == '\n' {
        start += 1;
    }
    let mut end = chars.len();
    while end > start && chars[end - 1] == '\n' {
        end -= 1;
    }
    (base + start, base + end)
}

/// `TemplateUtils.isTruthy`: only null, the empty string and an empty array are falsy — `0` and
/// `false` stay truthy because they carry meaning in SQL.
fn is_truthy(value: Option<&&Value>) -> bool {
    match value.copied() {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(_) => true,
    }
}

/// The value a variable renders: the supplied one, or its default as text.
fn resolved(values: &Values<'_>, node: &Node, fallback: &Option<String>) -> Option<Value> {
    let name = match node {
        Node::Var { name, .. } => name,
        _ => return None,
    };
    match values.get(name.as_str()).copied() {
        Some(value) if !value.is_null() => Some(value.clone()),
        _ => fallback.as_ref().cloned().map(Value::String),
    }
}

/// Renders text, inlining values the way Java's `StringBuilder.append(value)` did.
pub fn render_text(compiled: &CompiledTemplate, values: &Values<'_>) -> String {
    let mut out = String::new();
    render_text_nodes(&compiled.nodes, values, &mut out);
    out
}

fn render_text_nodes(nodes: &[Node], values: &Values<'_>, out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Var { name, default, .. } => match values.get(name.as_str()).copied() {
                Some(value) if !value.is_null() => out.push_str(&display(value)),
                _ => {
                    if let Some(default) = default {
                        out.push_str(default);
                    }
                }
            },
            Node::If { condition, body } => {
                if is_truthy(values.get(condition.as_str())) {
                    render_text_nodes(body, values, out);
                }
            }
            Node::Where { body } => {
                let mut inner = String::new();
                render_text_nodes(body, values, &mut inner);
                let trimmed = collapse_blank_lines(inner.trim());
                if !trimmed.is_empty() {
                    out.push_str(&trimmed);
                }
            }
        }
    }
}

/// Renders SQL, turning every non-raw variable into a `?` placeholder plus one binding.
pub fn render_sql(compiled: &CompiledTemplate, values: &Values<'_>) -> Result<PreparedSql, String> {
    let mut prepared = PreparedSql::default();
    render_sql_nodes(&compiled.nodes, values, &mut prepared)?;
    Ok(prepared)
}

fn render_sql_nodes(
    nodes: &[Node],
    values: &Values<'_>,
    out: &mut PreparedSql,
) -> Result<(), String> {
    for node in nodes {
        match node {
            Node::Text(text) => out.sql.push_str(text),
            Node::Var { name, default, raw } => {
                let value = resolved(values, node, default);
                if *raw {
                    match value {
                        Some(Value::Array(_)) => {
                            // Six literal braces each side plus the pair around `name`.
                            return Err(format!(
                                "Raw variable '{{{{{{{name}}}}}}}' cannot be an array"
                            ));
                        }
                        Some(value) => out.sql.push_str(&display(&value)),
                        None => {}
                    }
                    continue;
                }
                match value {
                    Some(Value::Array(items)) => {
                        for (index, item) in items.iter().enumerate() {
                            if index > 0 {
                                out.sql.push_str(", ");
                            }
                            out.sql.push('?');
                            out.bindings.push(ParamBinding {
                                name: name.clone(),
                                value: item.clone(),
                            });
                        }
                    }
                    value => {
                        out.sql.push('?');
                        out.bindings.push(ParamBinding {
                            name: name.clone(),
                            value: value.unwrap_or(Value::Null),
                        });
                    }
                }
            }
            Node::If { condition, body } => {
                if is_truthy(values.get(condition.as_str())) {
                    render_sql_nodes(body, values, out)?;
                }
            }
            Node::Where { body } => {
                let mut inner = PreparedSql::default();
                render_sql_nodes(body, values, &mut inner)?;
                let trimmed = collapse_blank_lines(inner.sql.trim());
                if trimmed.is_empty() {
                    continue;
                }
                out.sql.push_str("WHERE ");
                out.sql.push_str(&strip_leading_and_or(&trimmed));
                out.bindings.extend(inner.bindings);
            }
        }
    }
    Ok(())
}

/// Java's `StringBuilder.append(value)` for the types a parameter can hold.
fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Folds a blank line into a single newline — the cleanup that keeps a skipped `{{#if}}` branch
/// from leaving an empty line. Equivalent to Java's `replaceAll("\n\\s*\n", "\n")`, including its
/// left-to-right, non-overlapping pass.
fn collapse_blank_lines(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\n' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            // `\s*` is greedy, so the match ends at the last newline inside that run.
            if let Some(close) = (i + 1..j).rev().find(|&k| chars[k] == '\n') {
                out.push('\n');
                i = close + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// A leading `AND`/`OR` in a `{{#where}}` body is the artifact of a dropped first condition.
fn strip_leading_and_or(text: &str) -> String {
    let upper = text.to_uppercase();
    if upper.starts_with("AND ") {
        text["AND ".len()..].to_string()
    } else if upper.starts_with("OR ") {
        text["OR ".len()..].to_string()
    } else if upper == "AND" || upper == "OR" {
        String::new()
    } else {
        text.to_string()
    }
}

/// The rendered SQL with its bindings inlined, for display only — this text is never executed.
pub fn to_display_sql(prepared: &PreparedSql) -> String {
    let mut bindings = prepared.bindings.iter();
    let mut out = String::with_capacity(prepared.sql.len());
    for ch in prepared.sql.chars() {
        if ch == '?'
            && let Some(binding) = bindings.next()
        {
            out.push_str(&format_value(&binding.value));
            continue;
        }
        out.push(ch);
    }
    out
}

fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::String(text) if !is_number_text(text) => format!("'{text}'"),
        other => display(other),
    }
}

/// Java's `-?\d+(\\.\\d+)?`: unlike a float parse this rejects `1.`, `.5` and exponents.
fn is_number_text(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let all_digits = |value: &str| !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit());
    match digits.split_once('.') {
        None => all_digits(digits),
        Some((integer, fraction)) => all_digits(integer) && all_digits(fraction),
    }
}

/// The names of variables whose `?` would land inside a string literal, a comment or a
/// dollar-quoted block — where the driver does not count it as a parameter, so the binding count
/// could never match at execution time.
pub fn find_quoted_variables(compiled: &CompiledTemplate) -> Vec<String> {
    let mut sql = String::new();
    let mut placeholders: Vec<(usize, String)> = Vec::new();
    linearize(&compiled.nodes, &mut sql, &mut placeholders);

    let chars: Vec<char> = sql.chars().collect();
    let mut violations: Vec<String> = Vec::new();
    let mut next = 0usize;
    let mut state = LexState::Normal;
    let mut escape_mode = false;
    let mut dollar_tag: Vec<char> = Vec::new();
    let mut i = 0usize;

    while i < chars.len() {
        if next < placeholders.len() && i == placeholders[next].0 {
            if state != LexState::Normal
                && !violations.iter().any(|seen| *seen == placeholders[next].1)
            {
                violations.push(placeholders[next].1.clone());
            }
            next += 1;
            i += 1;
            continue;
        }

        let ch = chars[i];
        let after = chars.get(i + 1).copied().unwrap_or('\0');
        match state {
            LexState::Normal => {
                if ch == '\'' {
                    state = LexState::SingleQuote;
                    escape_mode = is_escape_string_prefix(&chars, i);
                    i += 1;
                } else if ch == '-' && after == '-' {
                    state = LexState::LineComment;
                    i += 2;
                } else if ch == '/' && after == '*' {
                    state = LexState::BlockComment;
                    i += 2;
                } else if ch == '$' {
                    match match_dollar_tag(&chars, i) {
                        Some(tag) => {
                            i += tag.len();
                            dollar_tag = tag;
                            state = LexState::DollarQuote;
                        }
                        None => i += 1,
                    }
                } else {
                    i += 1;
                }
            }
            LexState::SingleQuote => {
                if escape_mode && ch == '\\' && after != '\0' {
                    i += 2;
                } else if ch == '\'' {
                    if after == '\'' {
                        i += 2;
                    } else {
                        state = LexState::Normal;
                        escape_mode = false;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            LexState::LineComment => {
                if ch == '\n' {
                    state = LexState::Normal;
                }
                i += 1;
            }
            LexState::BlockComment => {
                if ch == '*' && after == '/' {
                    state = LexState::Normal;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            LexState::DollarQuote => {
                if !dollar_tag.is_empty() && starts_with(&chars, i, chars.len(), &dollar_tag) {
                    let len = dollar_tag.len();
                    dollar_tag.clear();
                    state = LexState::Normal;
                    i += len;
                } else {
                    i += 1;
                }
            }
        }
    }
    violations
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LexState {
    Normal,
    SingleQuote,
    LineComment,
    BlockComment,
    DollarQuote,
}

fn linearize(nodes: &[Node], sql: &mut String, placeholders: &mut Vec<(usize, String)>) {
    for node in nodes {
        match node {
            Node::Text(text) => sql.push_str(text),
            Node::Var {
                name,
                default: _,
                raw,
            } => {
                // A raw variable inlines its value: no placeholder, no binding, no mismatch.
                if *raw {
                    continue;
                }
                placeholders.push((sql.chars().count(), name.clone()));
                sql.push('?');
            }
            Node::If { condition: _, body } | Node::Where { body } => {
                linearize(body, sql, placeholders);
            }
        }
    }
}

/// True when the quote at `quote_pos` opens an `E'...'` literal, i.e. a standalone `e`/`E` precedes it.
fn is_escape_string_prefix(chars: &[char], quote_pos: usize) -> bool {
    if quote_pos == 0 {
        return false;
    }
    let prev = chars[quote_pos - 1];
    if prev != 'E' && prev != 'e' {
        return false;
    }
    quote_pos < 2 || !is_ident_char(chars[quote_pos - 2])
}

/// Matches a dollar-quote opening tag (`$$` or `$tag$`) at `pos`.
fn match_dollar_tag(chars: &[char], pos: usize) -> Option<Vec<char>> {
    let j = pos + 1;
    if j >= chars.len() {
        return None;
    }
    if chars[j] == '$' {
        return Some(vec!['$', '$']);
    }
    if !(chars[j].is_alphabetic() || chars[j] == '_') {
        return None;
    }
    let mut k = j;
    while k < chars.len() && (chars[k].is_alphanumeric() || chars[k] == '_') {
        k += 1;
    }
    if k < chars.len() && chars[k] == '$' {
        return Some(chars[pos..=k].to_vec());
    }
    None
}

fn is_ident_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(args: Value) -> Map<String, Value> {
        args.as_object().expect("object of arguments").clone()
    }

    #[test]
    fn a_variable_becomes_a_placeholder_and_its_default_is_bound() {
        let compiled = parse("SELECT * FROM t WHERE a = {{a}} AND b = {{b:0}}").unwrap();
        let args = fixture(json!({ "a": "x" }));
        let prepared = render_sql(&compiled, &values_of(&args)).unwrap();
        assert_eq!(prepared.sql, "SELECT * FROM t WHERE a = ? AND b = ?");
        assert_eq!(prepared.bindings[0].value, json!("x"));
        // The absent `b` still holds its placeholder — bound to the default text.
        assert_eq!(prepared.bindings[1].value, json!("0"));
        assert_eq!(compiled.variables(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn an_array_expands_into_one_placeholder_per_item() {
        let compiled = parse("WHERE id IN ({{ids}})").unwrap();
        let args = fixture(json!({ "ids": [1, 2, 3] }));
        let prepared = render_sql(&compiled, &values_of(&args)).unwrap();
        assert_eq!(prepared.sql, "WHERE id IN (?, ?, ?)");
        assert_eq!(prepared.bindings.len(), 3);
        assert_eq!(prepared.bindings[2].value, json!(3));
    }

    #[test]
    fn a_raw_variable_inlines_its_value_and_rejects_arrays() {
        let compiled = parse("SELECT * FROM {{{table}}}").unwrap();
        let args = fixture(json!({ "table": "orders" }));
        let prepared = render_sql(&compiled, &values_of(&args)).unwrap();
        assert_eq!(prepared.sql, "SELECT * FROM orders");
        assert!(prepared.bindings.is_empty());

        let arrays = fixture(json!({ "table": ["a"] }));
        assert_eq!(
            render_sql(&compiled, &values_of(&arrays)).unwrap_err(),
            "Raw variable '{{{table}}}' cannot be an array"
        );
    }

    #[test]
    fn zero_and_false_are_truthy_but_null_empty_and_blank_are_not() {
        let compiled = parse("{{#if flag}}yes{{/if}}").unwrap();
        for truthy in [json!(0), json!(false), json!("0")] {
            let args = fixture(json!({ "flag": truthy.clone() }));
            assert_eq!(
                render_text(&compiled, &values_of(&args)),
                "yes",
                "{truthy} keeps the branch"
            );
        }
        for falsy in [json!(null), json!(""), json!([])] {
            let args = fixture(json!({ "flag": falsy.clone() }));
            assert_eq!(
                render_text(&compiled, &values_of(&args)),
                "",
                "{falsy} drops the branch"
            );
        }
        assert_eq!(compiled.variables(), vec!["flag".to_string()]);
    }

    #[test]
    fn a_where_block_prepends_where_and_drops_a_leading_and() {
        let compiled =
            parse("SELECT 1 {{#where}}\nAND a = {{a}}\nOR b = {{b}}\n{{/where}}").unwrap();
        let args = fixture(json!({ "a": "x", "b": "y" }));
        let prepared = render_sql(&compiled, &values_of(&args)).unwrap();
        assert_eq!(prepared.sql, "SELECT 1 WHERE a = ?\nOR b = ?");
        assert_eq!(prepared.bindings.len(), 2);

        // Text mode never writes the keyword and keeps the leftover `AND`: it only inlines what
        // is there.
        assert_eq!(
            render_text(&compiled, &values_of(&args)),
            "SELECT 1 AND a = x\nOR b = y"
        );
    }

    #[test]
    fn a_where_block_with_nothing_left_in_writes_no_keyword() {
        let compiled = parse("SELECT 1 {{#where}}{{#if a}}AND x = {{a}}{{/if}}{{/where}}").unwrap();
        let empty = fixture(json!({}));
        assert_eq!(
            render_sql(&compiled, &values_of(&empty)).unwrap().sql,
            "SELECT 1 "
        );
        let filled = fixture(json!({ "a": 7 }));
        let prepared = render_sql(&compiled, &values_of(&filled)).unwrap();
        assert_eq!(prepared.sql, "SELECT 1 WHERE x = ?");
        assert_eq!(prepared.bindings[0].value, json!(7));
    }

    #[test]
    fn escapes_and_bad_tags_keep_their_java_messages() {
        let compiled = parse(r"\{{literal}} and \{{{raw}}}").unwrap();
        let empty = fixture(json!({}));
        assert_eq!(
            render_text(&compiled, &values_of(&empty)),
            "{{literal}} and {{{raw}}}"
        );

        assert_eq!(
            parse("{{#each x}}a{{/each}}").unwrap_err(),
            "Unknown block directive: '{{#each x}}'"
        );
        assert_eq!(
            parse("{{#if}}x{{/if}}").unwrap_err(),
            "Unknown block directive: '{{#if}}'"
        );
        assert_eq!(
            parse("{{/if}}").unwrap_err(),
            "Unexpected closing tag '{{/if}}'"
        );
        assert_eq!(
            parse("{{#if a > b}}x{{/if}}").unwrap_err(),
            "Invalid condition name: 'a > b'. Must match [A-Za-z0-9_.]+"
        );
        assert_eq!(
            parse("{{bad-name}}").unwrap_err(),
            "Invalid variable name: 'bad-name'. Must match [A-Za-z0-9_.]+"
        );
        assert_eq!(
            parse("SELECT {{a").unwrap_err(),
            "Unclosed '{{' at position 7"
        );
        assert_eq!(
            parse("SELECT {{{a").unwrap_err(),
            "Unclosed '{{{' at position 7"
        );
        assert_eq!(
            parse("{{#if a}}x").unwrap_err(),
            "Unclosed '{{#if a}}' — missing '{{/if}}'"
        );
    }

    #[test]
    fn if_blocks_nest_and_their_end_tag_is_matched_by_depth() {
        let compiled = parse("{{#if a}}1{{#if b}}2{{/if}}3{{/if}}").unwrap();
        let outer = fixture(json!({ "a": 1 }));
        assert_eq!(render_text(&compiled, &values_of(&outer)), "13");
        let both = fixture(json!({ "a": 1, "b": true }));
        assert_eq!(render_text(&compiled, &values_of(&both)), "123");
        assert_eq!(compiled.variables(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn quoted_variables_are_detected_but_plain_placeholders_are_not() {
        let compiled = parse("SELECT '{{a}}' , b = {{b}} -- {{c}}\n, $$x{{d}}y$$").unwrap();
        assert_eq!(
            find_quoted_variables(&compiled),
            vec!["a".to_string(), "c".to_string(), "d".to_string()]
        );

        let clean = parse("SELECT * FROM t WHERE x = {{x}}").unwrap();
        assert!(find_quoted_variables(&clean).is_empty());

        // Inside `E'...'` a quote only closes the literal when no backslash owns it.
        let escape = parse(r"SELECT E'a\' {{x}}'").unwrap();
        assert_eq!(find_quoted_variables(&escape), vec!["x".to_string()]);

        let raw = parse("SELECT $$ {{{raw}}} $$").unwrap();
        assert!(
            find_quoted_variables(&raw).is_empty(),
            "a raw variable never becomes a counted placeholder"
        );
    }

    #[test]
    fn display_sql_inlines_bindings_for_reading_only() {
        let compiled =
            parse("SELECT * FROM t WHERE a = {{a}} AND n = {{n}} AND z = {{z}}").unwrap();
        let args = fixture(json!({ "a": "O'Hara", "n": 12 }));
        let prepared = render_sql(&compiled, &values_of(&args)).unwrap();
        assert_eq!(
            to_display_sql(&prepared),
            "SELECT * FROM t WHERE a = 'O'Hara' AND n = 12 AND z = NULL"
        );
        assert_eq!(format_value(&json!("1.5")), "1.5");
        assert_eq!(format_value(&json!("1e5")), "'1e5'");
        assert_eq!(format_value(&json!("-0.25")), "-0.25");
        assert_eq!(format_value(&json!("")), "''");
    }

    #[test]
    fn blank_line_folding_matches_the_java_regex() {
        assert_eq!(collapse_blank_lines("a\n\nb"), "a\nb");
        assert_eq!(collapse_blank_lines("a\n   \nb"), "a\nb");
        assert_eq!(collapse_blank_lines("a\n\n\nb"), "a\nb");
        assert_eq!(collapse_blank_lines("a\r\nb"), "a\r\nb");
        assert_eq!(collapse_blank_lines("a\n \n x"), "a\n x");
    }

    #[test]
    fn strip_leading_and_or_follows_java_exactly() {
        assert_eq!(strip_leading_and_or("AND x"), "x");
        assert_eq!(strip_leading_and_or("or y = 1"), "y = 1");
        assert_eq!(strip_leading_and_or("AND"), "");
        assert_eq!(strip_leading_and_or("or"), "");
        assert_eq!(strip_leading_and_or("ANDNOT z"), "ANDNOT z");
    }

    #[test]
    fn java_trim_and_the_block_body_newline_strip_differ_on_purpose() {
        assert_eq!(trim(&"  x  ".chars().collect::<Vec<char>>()), ['x']);
        assert!(trim(&"   ".chars().collect::<Vec<char>>()).is_empty());
        // A block body keeps inner spaces but loses the newlines that bracket it.
        assert_eq!(
            trim_newlines(&"\n body \n".chars().collect::<Vec<char>>(), 0),
            (1, 7)
        );
    }
}
