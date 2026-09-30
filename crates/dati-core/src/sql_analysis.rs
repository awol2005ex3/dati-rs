//! A lexical analysis of rendered SQL: the operation types a statement carries and the tables it
//! touches.
//!
//! The Java module of the same name ran JSqlParser over the SQL. Porting that grammar is out of
//! proportion to what the two callers need — a [`SqlPolicy`](crate::model::mcp_tool::SqlPolicy)
//! verdict and the set of tables to check against the service scope — so this is a lexer instead.
//! It tokenizes with the same string, comment and dollar-quote rules pgjdbc uses (and that
//! [`crate::template`] already applies when it counts placeholders), then reads the leading keyword
//! and the operands of the keywords that introduce a table.
//!
//! Three behaviours are worth stating, because a real parser gets them for free:
//! - A statement that does not begin with a known keyword, or whose `FROM`/`JOIN`/`INTO`/`TABLE` is
//!   not followed by a name, is treated as one a parser would have rejected. Like the Java
//!   fallback, that answers `OTHER` with an **empty** statement-type list, which lets a policy pass
//!   the SQL and leave the verdict to the database.
//! - An index target (`CREATE INDEX i ON t(c)`) is not reported as a table reference.
//! - A `USING` partner of a `DELETE` is not reported either.

use serde::Serialize;

/// What one statement does. The wire value is the Java enum name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SqlOperationType {
    #[serde(rename = "SELECT")]
    Select,
    #[serde(rename = "INSERT")]
    Insert,
    #[serde(rename = "UPDATE")]
    Update,
    #[serde(rename = "DELETE")]
    Delete,
    #[serde(rename = "MERGE")]
    Merge,
    #[serde(rename = "DDL")]
    Ddl,
    #[serde(rename = "METADATA")]
    Metadata,
    #[serde(rename = "TRANSACTION")]
    Transaction,
    #[serde(rename = "SET")]
    Set,
    #[serde(rename = "MULTI")]
    Multi,
    #[serde(rename = "OTHER")]
    Other,
}

impl SqlOperationType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Merge => "MERGE",
            Self::Ddl => "DDL",
            Self::Metadata => "METADATA",
            Self::Transaction => "TRANSACTION",
            Self::Set => "SET",
            Self::Multi => "MULTI",
            Self::Other => "OTHER",
        }
    }
}

/// A table as the SQL named it: the qualifier is kept, so a scope check can match either form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    pub fn unqualified(name: impl Into<String>) -> Self {
        Self {
            schema: None,
            name: name.into(),
        }
    }

    pub fn qualified(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    /// `schema.name`, or just `name` when the SQL left it unqualified.
    pub fn qualified_name(&self) -> String {
        match &self.schema {
            Some(schema) => format!("{schema}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlAnalysisResult {
    pub op_type: SqlOperationType,
    pub statement_types: Vec<SqlOperationType>,
    pub tables: Vec<TableRef>,
}

impl SqlAnalysisResult {
    fn unreadable() -> Self {
        Self {
            op_type: SqlOperationType::Other,
            statement_types: Vec::new(),
            tables: Vec::new(),
        }
    }

    /// `MULTI` is a marker: a policy checks it on the result, then walks the per-statement types.
    pub fn is_multi(&self) -> bool {
        self.op_type == SqlOperationType::Multi
    }
}

/// Analyzes a rendered SQL string, `?` placeholders included.
pub fn analyze(sql: &str) -> SqlAnalysisResult {
    let Some(tokens) = tokenize(sql) else {
        return SqlAnalysisResult::unreadable();
    };
    let segments = split_statements(&tokens);
    let segments: Vec<(usize, usize)> = segments
        .into_iter()
        // A trailing `;`, or a bare `;` between two statements, contributes nothing — as it did in
        // Java, where the empty statement never reached the type list.
        .filter(|(from, to)| tokens.get(*from..*to).is_some_and(|part| !part.is_empty()))
        .collect();
    if segments.is_empty() {
        return SqlAnalysisResult::unreadable();
    }

    let mut types = Vec::with_capacity(segments.len());
    let mut tables: Vec<TableRef> = Vec::new();
    for (from, to) in segments {
        let Some((op_type, refs)) = analyze_statement(&tokens[from..to]) else {
            return SqlAnalysisResult::unreadable();
        };
        types.push(op_type);
        for table in refs {
            if !tables.contains(&table) {
                tables.push(table);
            }
        }
    }

    let op_type = if types.len() > 1 {
        SqlOperationType::Multi
    } else {
        types[0]
    };
    SqlAnalysisResult {
        op_type,
        statement_types: types,
        tables,
    }
}

// ---- tokens -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A bare word: a keyword, a name or an alias.
    Word,
    /// A quoted identifier, already stripped of its quotes.
    Quoted,
    /// A `'…'` or `$tag$…$tag$` literal.
    String,
    Number,
    Punct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    kind: Kind,
    text: String,
}

impl Token {
    /// Upper-cased keyword text; a quoted name is never a keyword.
    fn keyword(&self) -> Option<String> {
        (self.kind == Kind::Word).then(|| self.text.to_ascii_uppercase())
    }

    /// Anything a table name can be built from.
    fn is_name(&self) -> bool {
        matches!(self.kind, Kind::Word | Kind::Quoted)
    }

    fn is_punct(&self, ch: char) -> bool {
        self.kind == Kind::Punct && self.text.starts_with(ch)
    }

    fn is_reserved(&self) -> bool {
        self.keyword()
            .is_some_and(|word| RESERVED.iter().any(|reserved| *reserved == word))
    }
}

const PUNCTS: &[char] = &[
    '(', ')', ',', ';', '*', '=', '<', '>', '+', '-', '/', '%', '|', '&', '^', '~', '!', '?', ':',
    '.', '@',
];

fn token(kind: Kind, text: String) -> Token {
    Token { kind, text }
}

/// Tokenizes SQL with comments dropped. `None` means the text cannot be read at all — an
/// unterminated quote or block comment, which is the lexer's share of what a parser rejects.
fn tokenize(sql: &str) -> Option<Vec<Token>> {
    let chars: Vec<char> = sql.chars().collect();
    let len = chars.len();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0usize;

    while i < len {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if ch == '-' && chars.get(i + 1) == Some(&'-') {
            while i < len && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == '/' && chars.get(i + 1) == Some(&'*') {
            i = find_block_comment_end(&chars, i + 2)?;
            continue;
        }
        if ch == '\'' {
            let (body, next) = read_single_quoted(&chars, i, is_escape_string_prefix(&chars, i))?;
            tokens.push(token(Kind::String, body));
            i = next;
            continue;
        }
        if ch == '$'
            && let Some((text, next)) = read_dollar_quoted(&chars, i)
        {
            tokens.push(token(Kind::String, text));
            i = next;
            continue;
        }
        if matches!(ch, '"' | '`' | '[') {
            let (text, next) = read_delimited(&chars, i)?;
            tokens.push(token(Kind::Quoted, text));
            i = next;
            continue;
        }
        if ch.is_ascii_digit() {
            let start = i;
            while i < len && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            tokens.push(token(Kind::Number, chars[start..i].iter().collect()));
            continue;
        }
        if ch.is_alphabetic() || ch == '_' || ch == '@' || ch == '#' {
            let start = i;
            while i < len
                && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '@' | '#' | '$'))
            {
                i += 1;
            }
            tokens.push(token(Kind::Word, chars[start..i].iter().collect()));
            continue;
        }
        if PUNCTS.contains(&ch) {
            tokens.push(token(Kind::Punct, ch.to_string()));
            i += 1;
            continue;
        }
        // A character nothing can be built from — a currency sign, an emoji — is skipped.
        i += 1;
    }
    Some(tokens)
}

/// Splits on `;` at paren depth zero, so a semicolon inside a string or a bracketed list stays put.
fn split_statements(tokens: &[Token]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (index, tok) in tokens.iter().enumerate() {
        if tok.is_punct('(') {
            depth += 1;
        } else if tok.is_punct(')') {
            depth = depth.saturating_sub(1);
        } else if tok.is_punct(';') && depth == 0 {
            ranges.push((start, index));
            start = index + 1;
        }
    }
    ranges.push((start, tokens.len()));
    ranges
}

fn find_block_comment_end(chars: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

/// Reads a `'…'` literal from its opening quote, returning its text and the index after the closing
/// quote. `''` always escapes a quote; inside `E'…'` a backslash escapes the next character too.
fn read_single_quoted(chars: &[char], start: usize, escape_mode: bool) -> Option<(String, usize)> {
    let mut body = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        let ch = chars[i];
        if escape_mode && ch == '\\' {
            let next = *chars.get(i + 1)?;
            body.push('\\');
            body.push(next);
            i += 2;
            continue;
        }
        if ch == '\'' {
            if chars.get(i + 1) == Some(&'\'') {
                body.push('\'');
                i += 2;
                continue;
            }
            return Some((body, i + 1));
        }
        body.push(ch);
        i += 1;
    }
    None
}

/// True when the quote at `quote_pos` opens an `E'…'` literal, i.e. a standalone `e`/`E` precedes
/// it — the same rule [`crate::template`] applies when it counts placeholders.
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

fn is_ident_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Reads a whole `$tag$…$tag$` run from `pos`, returning the raw text and the index after it.
fn read_dollar_quoted(chars: &[char], pos: usize) -> Option<(String, usize)> {
    let mut k = pos + 1;
    if chars.get(k) != Some(&'$') {
        if !matches!(chars.get(k), Some(ch) if ch.is_alphabetic() || *ch == '_') {
            return None;
        }
        while k < chars.len() && (chars[k].is_alphanumeric() || chars[k] == '_') {
            k += 1;
        }
        if chars.get(k) != Some(&'$') {
            return None;
        }
    }
    let tag: Vec<char> = chars[pos..=k].to_vec();
    let body_start = k + 1;
    let close = (body_start..chars.len())
        .find(|&at| at + tag.len() <= chars.len() && chars[at..at + tag.len()] == tag[..])?;
    Some((
        chars[pos..close + tag.len()].iter().collect(),
        close + tag.len(),
    ))
}

/// Reads a `"…"` (doubled quotes escape), `` `…` `` or `[…]` identifier.
fn read_delimited(chars: &[char], start: usize) -> Option<(String, usize)> {
    let open = chars[start];
    let close = match open {
        '"' => '"',
        '`' => '`',
        _ => ']',
    };
    let mut body = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == close {
            if open == '"' && chars.get(i + 1) == Some(&'"') {
                body.push('"');
                i += 2;
                continue;
            }
            return Some((body, i + 1));
        }
        body.push(chars[i]);
        i += 1;
    }
    None
}

// ---- per-statement analysis ----------------------------------------------------------------

/// Words a table name can never be, so a `FROM WHERE` reads as the broken statement it is rather
/// than as a table called `WHERE`.
const RESERVED: &[&str] = &[
    "ALL",
    "AND",
    "AS",
    "ASC",
    "BY",
    "CASE",
    "CAST",
    "CROSS",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "DUMPFILE",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FOR",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IN",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "JOIN",
    "LEFT",
    "LIMIT",
    "NOT",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OUTFILE",
    "PARTITION",
    "RIGHT",
    "SELECT",
    "SET",
    "TRUNCATE",
    "UNION",
    "UPDATE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WITH",
];

/// Words that may sit between a DDL `TABLE` and the name it targets.
const DDL_SKIPS: &[&str] = &[
    "IF",
    "NOT",
    "EXISTS",
    "ONLY",
    "TEMP",
    "TEMPORARY",
    "UNLOGGED",
];

/// `(type, tables)` of one statement; `None` when the lexer decides a parser would have failed.
fn analyze_statement(tokens: &[Token]) -> Option<(SqlOperationType, Vec<TableRef>)> {
    let op_type = detect_type(tokens)?;
    let mut finder = TableFinder::new(tokens, op_type);
    let tables = finder.run()?;
    Some((op_type, tables))
}

fn detect_type(tokens: &[Token]) -> Option<SqlOperationType> {
    // A parenthesized statement still declares itself by its first keyword.
    let base = usize::from(tokens.first().is_some_and(|tok| tok.is_punct('(')));
    let first = tokens.get(base)?;
    match first.keyword()?.as_str() {
        "SELECT" | "WITH" => Some(SqlOperationType::Select),
        "INSERT" | "REPLACE" => Some(SqlOperationType::Insert),
        "UPDATE" => Some(SqlOperationType::Update),
        "DELETE" => Some(SqlOperationType::Delete),
        "MERGE" => Some(SqlOperationType::Merge),
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" => Some(SqlOperationType::Ddl),
        "SHOW" | "DESC" | "DESCRIBE" | "EXPLAIN" => Some(SqlOperationType::Metadata),
        "BEGIN" | "COMMIT" | "ROLLBACK" => Some(SqlOperationType::Transaction),
        "START" => match tokens.get(base + 1).and_then(Token::keyword) {
            Some(next) if next == "TRANSACTION" => Some(SqlOperationType::Transaction),
            _ => None,
        },
        // `CALL` reads, which is why it answers OTHER with a type rather than as a failure.
        "CALL" => Some(SqlOperationType::Other),
        "SET" => Some(SqlOperationType::Set),
        _ => None,
    }
}

struct TableFinder<'a> {
    tokens: &'a [Token],
    op_type: SqlOperationType,
    index: usize,
    /// Inside a `FROM a, b` list, where a comma introduces another table.
    list: bool,
    cte_names: Vec<String>,
    found: Vec<TableRef>,
}

impl<'a> TableFinder<'a> {
    fn new(tokens: &'a [Token], op_type: SqlOperationType) -> Self {
        Self {
            tokens,
            op_type,
            index: 0,
            list: false,
            cte_names: Vec::new(),
            found: Vec::new(),
        }
    }

    fn run(&mut self) -> Option<Vec<TableRef>> {
        self.collect_cte_names();
        while self.index < self.tokens.len() {
            let tok = &self.tokens[self.index];
            if tok.is_punct('(') || tok.is_punct(')') {
                self.list = false;
                self.index += 1;
                continue;
            }
            if tok.is_punct(',') {
                self.index += 1;
                if self.list {
                    self.list = self.capture()?;
                }
                continue;
            }
            let Some(keyword) = tok.keyword() else {
                self.index += 1;
                continue;
            };
            if is_table_introducer(&keyword, self.index, self.op_type) {
                self.index += 1;
                if keyword == "TABLE" {
                    self.skip_ddl_words();
                }
                self.list = self.capture()?;
                continue;
            }
            if tok.is_reserved() {
                self.list = false;
            }
            self.index += 1;
        }
        Some(std::mem::take(&mut self.found))
    }

    /// The name at the cursor, qualifier included. `Some(true)` took one, `Some(false)` found a
    /// non-table operand (a subquery, a function call, an alias list), and `None` means a keyword
    /// that had to be followed by a name was not — SQL a parser would have rejected.
    fn capture(&mut self) -> Option<bool> {
        let tokens: &[Token] = self.tokens;
        let tok = tokens.get(self.index)?;
        if tok.is_punct('(') {
            return Some(false);
        }
        if !tok.is_name() || tok.is_reserved() {
            return None;
        }
        let mut value = TableRef::unqualified(tok.text.clone());
        self.index += 1;
        if tokens.get(self.index).is_some_and(|dot| dot.is_punct('.')) {
            let schema = tokens.get(self.index + 1).filter(|tok| tok.is_name())?;
            value = TableRef::qualified(std::mem::take(&mut value.name), schema.text.clone());
            self.index += 2;
        }
        if !value.name.is_empty() && !self.is_cte(&value.name) && !self.found.contains(&value) {
            self.found.push(value);
        }
        Some(true)
    }

    fn is_cte(&self, name: &str) -> bool {
        self.cte_names
            .iter()
            .any(|cte| cte.eq_ignore_ascii_case(name))
    }

    fn skip_ddl_words(&mut self) {
        while let Some(word) = self.tokens.get(self.index).and_then(Token::keyword) {
            if !DDL_SKIPS.contains(&word.as_str()) {
                break;
            }
            self.index += 1;
        }
    }

    /// `WITH a AS (…), b AS (…)` names every common table expression, so none of them is mistaken
    /// for a stored table later in the statement.
    fn collect_cte_names(&mut self) {
        if self.op_type != SqlOperationType::Select {
            return;
        }
        let Some(start) = self
            .tokens
            .iter()
            .position(|tok| tok.keyword().as_deref() == Some("WITH"))
        else {
            return;
        };
        let mut i = start + 1;
        if self.tokens.get(i).and_then(Token::keyword).as_deref() == Some("RECURSIVE") {
            i += 1;
        }
        while let Some(name) = self.tokens.get(i).filter(|tok| tok.is_name()) {
            self.cte_names.push(name.text.clone());
            i += 1;
            // An optional column list, then `AS`, then the body in parentheses.
            if self.tokens.get(i).is_some_and(|tok| tok.is_punct('(')) {
                i += 1;
                while self.tokens.get(i).is_some_and(|tok| !tok.is_punct(')')) {
                    i += 1;
                }
                i += 1;
            }
            if self.tokens.get(i).and_then(Token::keyword).as_deref() != Some("AS") {
                break;
            }
            i += 1;
            if !self.tokens.get(i).is_some_and(|tok| tok.is_punct('(')) {
                break;
            }
            i += 1;
            let mut depth = 1usize;
            while i < self.tokens.len() && depth > 0 {
                if self.tokens[i].is_punct('(') {
                    depth += 1;
                } else if self.tokens[i].is_punct(')') {
                    depth -= 1;
                }
                i += 1;
            }
            if self.tokens.get(i).is_some_and(|tok| tok.is_punct(',')) {
                i += 1;
                continue;
            }
            break;
        }
    }
}

/// Only these positions name a table: the operand of `FROM`, `JOIN` or `INTO`, the target of a DDL
/// `TABLE`, and the `UPDATE` that opens an update — `FOR UPDATE` and a `SET … UPDATE` branch say
/// nothing about which tables are read.
fn is_table_introducer(keyword: &str, index: usize, op_type: SqlOperationType) -> bool {
    match keyword {
        "FROM" | "JOIN" | "INTO" | "TABLE" => true,
        "UPDATE" => index == 0 && op_type == SqlOperationType::Update,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_of(sql: &str) -> Option<SqlOperationType> {
        let result = analyze(sql);
        (!result.statement_types.is_empty()).then_some(result.op_type)
    }

    fn tables_of(sql: &str) -> Vec<TableRef> {
        analyze(sql).tables
    }

    /// Every case `SqlAnalyzerTest` asserted a type for, in the same order.
    #[test]
    fn a_statement_declares_its_own_type() {
        let cases = [
            ("SELECT * FROM orders", SqlOperationType::Select),
            (
                "SELECT a.* FROM orders a JOIN users b ON a.user_id=b.id",
                SqlOperationType::Select,
            ),
            (
                "SELECT * FROM orders WHERE user_id IN (SELECT id FROM users)",
                SqlOperationType::Select,
            ),
            (
                "SELECT name FROM users UNION SELECT name FROM admins",
                SqlOperationType::Select,
            ),
            (
                "WITH active AS (SELECT * FROM orders WHERE status=?) SELECT * FROM active",
                SqlOperationType::Select,
            ),
            (
                "SELECT * FROM orders WHERE id=? FOR UPDATE",
                SqlOperationType::Select,
            ),
            (
                "INSERT INTO orders (name,price) VALUES (?,?)",
                SqlOperationType::Insert,
            ),
            (
                "INSERT INTO orders SELECT * FROM tmp_orders",
                SqlOperationType::Insert,
            ),
            (
                "UPDATE orders SET status=? WHERE id=?",
                SqlOperationType::Update,
            ),
            (
                "UPDATE orders a JOIN users b SET a.status=? WHERE b.name=?",
                SqlOperationType::Update,
            ),
            ("DELETE FROM orders WHERE id=?", SqlOperationType::Delete),
            (
                "SELECT * FROM orders /* INSERT */ WHERE id=?",
                SqlOperationType::Select,
            ),
            ("CREATE TABLE tmp (id INT)", SqlOperationType::Ddl),
            (
                "ALTER TABLE orders ADD COLUMN note TEXT",
                SqlOperationType::Ddl,
            ),
            ("DROP TABLE tmp_orders", SqlOperationType::Ddl),
            ("TRUNCATE TABLE orders", SqlOperationType::Ddl),
            (
                "CREATE INDEX idx_name ON orders(name)",
                SqlOperationType::Ddl,
            ),
            ("DROP INDEX idx_name", SqlOperationType::Ddl),
            ("SHOW TABLES", SqlOperationType::Metadata),
            ("DESCRIBE orders", SqlOperationType::Metadata),
            ("EXPLAIN SELECT * FROM orders", SqlOperationType::Metadata),
            ("COMMIT", SqlOperationType::Transaction),
            ("ROLLBACK", SqlOperationType::Transaction),
            ("SET @var = 1", SqlOperationType::Set),
            ("CALL my_proc()", SqlOperationType::Other),
            (
                "MERGE INTO orders t USING s ON t.id=s.id WHEN MATCHED THEN UPDATE SET t.x=s.x \
                 WHEN NOT MATCHED THEN INSERT VALUES (s.id)",
                SqlOperationType::Merge,
            ),
            ("BEGIN", SqlOperationType::Transaction),
            ("START TRANSACTION", SqlOperationType::Transaction),
        ];
        for (sql, expected) in cases {
            let result = analyze(sql);
            assert!(!result.is_multi(), "{sql}");
            assert_eq!(result.op_type, expected, "{sql}");
            assert_eq!(result.statement_types, vec![expected], "{sql}");
        }
    }

    /// The basic table cases of the Java test, one expected set per statement.
    #[test]
    fn a_table_list_is_collected_with_its_qualifiers() {
        let cases: [(&str, Vec<TableRef>); 13] = [
            (
                "SELECT * FROM orders",
                vec![TableRef::unqualified("orders")],
            ),
            (
                "SELECT * FROM public.orders",
                vec![TableRef::qualified("public", "orders")],
            ),
            (
                "SELECT * FROM orders JOIN users ON orders.uid=users.id",
                vec![
                    TableRef::unqualified("orders"),
                    TableRef::unqualified("users"),
                ],
            ),
            (
                "SELECT * FROM orders, users",
                vec![
                    TableRef::unqualified("orders"),
                    TableRef::unqualified("users"),
                ],
            ),
            (
                "SELECT * FROM orders o, users u",
                vec![
                    TableRef::unqualified("orders"),
                    TableRef::unqualified("users"),
                ],
            ),
            (
                "INSERT INTO orders VALUES (?)",
                vec![TableRef::unqualified("orders")],
            ),
            (
                "UPDATE orders SET x=?",
                vec![TableRef::unqualified("orders")],
            ),
            (
                "DELETE FROM orders WHERE id=?",
                vec![TableRef::unqualified("orders")],
            ),
            (
                "SELECT * FROM orders WHERE uid IN (SELECT id FROM users)",
                vec![
                    TableRef::unqualified("orders"),
                    TableRef::unqualified("users"),
                ],
            ),
            (
                "SELECT * FROM (SELECT * FROM orders) t",
                vec![TableRef::unqualified("orders")],
            ),
            (
                "WITH recent AS (SELECT * FROM orders WHERE date>?) SELECT * FROM recent",
                vec![TableRef::unqualified("orders")],
            ),
            (
                "INSERT INTO archive SELECT * FROM orders",
                vec![
                    TableRef::unqualified("archive"),
                    TableRef::unqualified("orders"),
                ],
            ),
            ("DROP TABLE orders", vec![TableRef::unqualified("orders")]),
        ];
        for (sql, expected) in cases {
            assert_eq!(tables_of(sql), expected, "{sql}");
        }
    }

    #[test]
    fn quoted_and_dotted_names_keep_their_shape() {
        assert_eq!(
            tables_of("SELECT * FROM \"Orders\""),
            vec![TableRef::unqualified("Orders")]
        );
        assert_eq!(
            tables_of("SELECT * FROM `order-items`"),
            vec![TableRef::unqualified("order-items")]
        );
        assert_eq!(
            tables_of("SELECT COUNT(*) FROM orders"),
            vec![TableRef::unqualified("orders")]
        );
        assert!(tables_of("SELECT 1+1").is_empty());
        assert_eq!(
            analyze("SELECT * FROM a WHERE x IN (SELECT y FROM b WHERE z IN (SELECT w FROM c))")
                .tables,
            vec![
                TableRef::unqualified("a"),
                TableRef::unqualified("b"),
                TableRef::unqualified("c"),
            ]
        );
        assert_eq!(
            TableRef::qualified("public", "orders").qualified_name(),
            "public.orders"
        );
        assert_eq!(TableRef::unqualified("orders").qualified_name(), "orders");
    }

    #[test]
    fn a_semicolon_ends_one_statement_and_a_trailing_one_is_ignored() {
        let result = analyze("SELECT * FROM orders; SELECT * FROM users");
        assert!(result.is_multi());
        assert_eq!(
            result.statement_types,
            vec![SqlOperationType::Select, SqlOperationType::Select]
        );

        let result = analyze("DROP TABLE tmp; SELECT * FROM orders");
        assert_eq!(result.op_type, SqlOperationType::Multi);
        assert_eq!(
            result.statement_types,
            vec![SqlOperationType::Ddl, SqlOperationType::Select]
        );

        for sql in ["SELECT * FROM orders;", "SELECT * FROM orders; ;"] {
            let result = analyze(sql);
            assert!(!result.is_multi(), "{sql}");
            assert_eq!(
                result.statement_types,
                vec![SqlOperationType::Select],
                "{sql}"
            );
        }

        let result = analyze("BEGIN; UPDATE accounts SET balance=100; COMMIT");
        assert_eq!(
            result.statement_types,
            vec![
                SqlOperationType::Transaction,
                SqlOperationType::Update,
                SqlOperationType::Transaction
            ]
        );
        assert_eq!(result.tables, vec![TableRef::unqualified("accounts")]);

        let result = analyze("UPDATE accounts SET balance=100; COMMIT");
        assert_eq!(
            result.statement_types,
            vec![SqlOperationType::Update, SqlOperationType::Transaction]
        );
    }

    #[test]
    fn a_statement_the_parser_would_reject_carries_no_types_at_all() {
        for sql in [
            "",
            "   ",
            "NOT A SQL STATEMENT !!!",
            "SELECT * FROM WHERE id=?",
            "GARBLED",
            "SELECT * FROM orders WHERE x = 'unterminated",
            "SELECT * FROM orders /* never closed",
            "SELECT * FROM",
        ] {
            let result = analyze(sql);
            assert_eq!(result.op_type, SqlOperationType::Other, "{sql}");
            assert!(result.statement_types.is_empty(), "{sql}");
            assert!(result.tables.is_empty(), "{sql}");
            assert!(!result.is_multi(), "{sql}");
        }
    }

    #[test]
    fn a_comment_never_introduces_a_table() {
        let result = analyze("SELECT * FROM orders -- DROP TABLE users");
        assert_eq!(result.op_type, SqlOperationType::Select);
        assert_eq!(result.tables, vec![TableRef::unqualified("orders")]);

        let result = analyze("SELECT * FROM orders /* INSERT INTO users */ WHERE id=?");
        assert_eq!(result.tables, vec![TableRef::unqualified("orders")]);

        let result = analyze("-- SELECT * FROM orders\nDROP TABLE users");
        assert_eq!(result.op_type, SqlOperationType::Ddl);
        assert_eq!(result.tables, vec![TableRef::unqualified("users")]);
    }

    #[test]
    fn a_dollar_quoted_body_and_a_string_cannot_smuggle_a_statement() {
        let result = analyze("SELECT $$ DELETE FROM t; $$ AS doc, orders FROM listing");
        // The dollar-quoted run is one literal, so the `;` inside it splits nothing.
        assert_eq!(result.statement_types, vec![SqlOperationType::Select]);
        let result = analyze("SELECT 'a;b' FROM orders; SELECT 1");
        assert_eq!(result.statement_types.len(), 2);
    }

    #[test]
    fn keywords_are_matched_case_insensitively() {
        assert_eq!(
            type_of("select * from Orders"),
            Some(SqlOperationType::Select)
        );
        assert_eq!(type_of("begin"), Some(SqlOperationType::Transaction));
        assert_eq!(
            type_of("start transaction"),
            Some(SqlOperationType::Transaction)
        );
        assert_eq!(
            type_of("START WITH 1"),
            None,
            "START alone is not a statement"
        );
        assert_eq!(
            tables_of("select * from Orders")[0],
            TableRef::unqualified("Orders")
        );
    }

    #[test]
    fn the_empty_cte_list_of_a_with_clause_does_not_confuse_the_finder() {
        let result =
            analyze("WITH a AS (SELECT 1), b AS (SELECT * FROM t2) SELECT * FROM a JOIN b ON 1=1");
        assert_eq!(result.tables, vec![TableRef::unqualified("t2")]);
    }
}
