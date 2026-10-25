//! Completion inside T-SQL statements the shared contexts do not model: a
//! `MERGE` (its target and source, `ON`, and each `WHEN … THEN` action) and
//! the `OUTPUT` clause of `INSERT`, `UPDATE`, `DELETE` and `MERGE`, whose
//! `inserted` and `deleted` rows are the statement's target table.

use super::MssqlDialect;
use crate::dialects::common;
use crate::parser::SqlParser;
use crate::schema::{Column, Schema, Table};
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, Position};

const WHEN_KEYWORDS: &[&str] = &[
    "WHEN MATCHED THEN",
    "WHEN NOT MATCHED THEN",
    "WHEN NOT MATCHED BY SOURCE THEN",
];

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    /// A word as written (keywords, plain names, numbers, `@vars`).
    Word(String),
    /// A `[bracketed]` or `"quoted"` name, unquoted.
    Name(String),
    Dot,
    Comma,
    Open,
    Close,
    Operator,
    Semicolon,
    Literal,
}

#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    /// The parenthesis depth the token is at, within the statement.
    depth: usize,
}

impl Token {
    fn is_word(&self, word: &str) -> bool {
        matches!(&self.kind, Kind::Word(text) if text.eq_ignore_ascii_case(word))
    }

    fn name(&self) -> Option<&str> {
        match &self.kind {
            Kind::Word(text) | Kind::Name(text) => Some(text),
            _ => None,
        }
    }
}

/// A table the statement names, with the alias it is known by.
struct Relation {
    reference: String,
    alias: Option<String>,
    /// The opening parenthesis of a derived table, `(SELECT …) AS s`.
    derived: Option<usize>,
}

impl Relation {
    fn label(&self) -> &str {
        self.alias
            .as_deref()
            .unwrap_or_else(|| self.reference.rsplit('.').next().unwrap_or(&self.reference))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum When {
    Matched,
    NotMatched,
    NotMatchedBySource,
}

/// Completion for the T-SQL statement at the cursor, when it is one this
/// module models; `None` leaves it to the shared contexts.
pub(super) fn complete(
    dialect: &MssqlDialect,
    sql: &str,
    position: Position,
    schema: Option<&Schema>,
) -> Option<Vec<CompletionItem>> {
    let before = common::text_before_position(sql, position);
    let mut tokens = statement_tokens(before);
    let partial = partial_word(before);
    if !partial.is_empty() {
        tokens.pop();
    }
    let qualifier = if matches!(tokens.last().map(|token| &token.kind), Some(Kind::Dot)) {
        tokens.pop();
        let qualifier = tokens.pop()?.name()?.to_string();
        Some(qualifier)
    } else {
        None
    };

    let statement = dml_statement(&tokens)?;
    let context = Context {
        dialect,
        schema,
        partial: partial.to_ascii_lowercase(),
    };

    if let Some(qualifier) = &qualifier {
        if qualifier.eq_ignore_ascii_case("inserted") || qualifier.eq_ignore_ascii_case("deleted") {
            let target = target_of(&tokens[statement.start..], statement.keyword)?;
            let table = context.resolve(sql, position, &target.reference);
            return Some(context.columns(table, None));
        }
    }

    let top = |token: &&Token| token.depth == 0;
    let statement_tokens = &tokens[statement.start..];
    if let Some(output) = statement_tokens
        .iter()
        .rposition(|token| token.depth == 0 && token.is_word("OUTPUT"))
    {
        let after = &statement_tokens[output + 1..];
        if qualifier.is_none() && !after.iter().filter(top).any(|token| token.is_word("INTO")) {
            let rows: &[&str] = match statement.keyword {
                "INSERT" => &["inserted"],
                "DELETE" => &["deleted"],
                _ => &["inserted", "deleted"],
            };
            // Names, not keywords: inserted as written whatever the keyword case.
            let mut items = context.keywords(rows, "0");
            for item in &mut items {
                item.kind = Some(CompletionItemKind::STRUCT);
                item.detail = Some(format!("The {} rows of the statement's target", item.label));
            }
            if statement.keyword == "MERGE" {
                let mut action = context.keywords(&["$action"], "0");
                for item in &mut action {
                    item.kind = Some(CompletionItemKind::VARIABLE);
                    item.detail =
                        Some("INSERT, UPDATE or DELETE: what MERGE did to the row".to_string());
                }
                items.extend(action);
            }
            if after
                .last()
                .is_some_and(|token| !matches!(token.kind, Kind::Comma))
            {
                items.extend(context.keywords(&["INTO"], "1"));
            }
            return Some(items);
        }
        // Past `OUTPUT … INTO`: the shared contexts complete the rest.
        qualifier.as_ref()?;
    }

    if statement.keyword == "MERGE" {
        return merge(
            &context,
            sql,
            position,
            statement_tokens,
            qualifier.as_deref(),
        );
    }
    None
}

struct Context<'a> {
    dialect: &'a MssqlDialect,
    schema: Option<&'a Schema>,
    partial: String,
}

impl Context<'_> {
    fn keywords(&self, keywords: &[&str], sort: &str) -> Vec<CompletionItem> {
        keywords
            .iter()
            .filter(|keyword| {
                self.partial.is_empty() || keyword.to_ascii_lowercase().starts_with(&self.partial)
            })
            .map(|keyword| {
                let mut item = self.dialect.create_keyword_item(keyword);
                common::set_completion_sort_text(&mut item, sort, keyword);
                item
            })
            .collect()
    }

    fn table(&self, reference: &str) -> Option<&Table> {
        self.schema
            .and_then(|schema| common::find_table_by_reference(schema, reference))
    }

    /// A table by its name or, failing that, by an alias of the statement.
    fn resolve(&self, sql: &str, position: Position, reference: &str) -> Option<&Table> {
        self.table(reference).or_else(|| {
            let aliases = SqlParser::relation_aliases_at_position(sql, position);
            let alias = SqlParser::resolve_relation_alias(&aliases, reference)?;
            self.table(&alias.relation)
        })
    }

    /// A table's columns, named with `qualifier` when one is given.
    fn columns(&self, table: Option<&Table>, qualifier: Option<&str>) -> Vec<CompletionItem> {
        let Some(table) = table else {
            return Vec::new();
        };
        table
            .columns
            .iter()
            .map(|column| {
                let mut item = self.dialect.create_column_item(column, qualifier);
                if qualifier.is_some() {
                    item.filter_text = Some(item.label.clone());
                }
                item
            })
            .collect()
    }

    fn tables(&self) -> Vec<CompletionItem> {
        let mut items = Vec::new();
        if let Some(schema) = self.schema {
            common::add_schema_tables(&mut items, schema, &self.partial, true);
        }
        items
    }
}

fn merge(
    context: &Context,
    sql: &str,
    position: Position,
    tokens: &[Token],
    qualifier: Option<&str>,
) -> Option<Vec<CompletionItem>> {
    // tokens[0] is MERGE.
    let mut index = 1;
    if tokens.get(index).is_some_and(|token| token.is_word("TOP")) {
        index += 1;
        while tokens
            .get(index)
            .is_some_and(|token| token.depth > 0 || matches!(token.kind, Kind::Open | Kind::Close))
        {
            index += 1;
        }
        if tokens
            .get(index)
            .is_some_and(|token| token.is_word("PERCENT"))
        {
            index += 1;
        }
    }
    let has_into = tokens.get(index).is_some_and(|token| token.is_word("INTO"));
    if has_into {
        index += 1;
    }

    let Some((target, after_target)) = relation_at(tokens, index, &["USING"]) else {
        let mut items = context.tables();
        if !has_into {
            items.extend(context.keywords(&["INTO"], "1"));
        }
        return Some(items);
    };

    let using = position_of(tokens, after_target, "USING");
    let Some(using) = using else {
        if inside_parentheses(tokens) {
            return Some(
                context.keywords(&["HOLDLOCK", "SERIALIZABLE", "ROWLOCK", "UPDLOCK"], "0"),
            );
        }
        let mut keywords = vec!["USING"];
        if target.alias.is_none() {
            keywords.insert(0, "AS");
        }
        keywords.push("WITH (HOLDLOCK)");
        return Some(context.keywords(&keywords, "0"));
    };

    if tokens
        .get(using + 1)
        .is_some_and(|token| matches!(token.kind, Kind::Open))
        && inside_parentheses(tokens)
        && position_of(tokens, using + 1, "ON").is_none()
    {
        // In the source's subquery: the shared contexts complete it.
        return None;
    }
    let Some((source, after_source)) = relation_at(tokens, using + 1, &["ON"]) else {
        return Some(context.tables());
    };
    let target_table = context.resolve(sql, position, &target.reference);
    let derived = source
        .derived
        .map(|open| derived_table(context, tokens, open, source.label()));
    let source_table = derived
        .as_ref()
        .or_else(|| context.table(&source.reference))
        .or_else(|| {
            source
                .alias
                .as_deref()
                .and_then(|alias| context.table(alias))
        });

    if let Some(qualifier) = qualifier {
        let table = if names(qualifier, &target) {
            target_table
        } else if names(qualifier, &source) {
            source_table
        } else {
            context.resolve(sql, position, qualifier)
        };
        return Some(context.columns(table, None));
    }

    let both = || {
        let mut items = context.columns(source_table, Some(source.label()));
        items.extend(context.columns(target_table, Some(target.label())));
        items
    };

    let Some(on) = position_of(tokens, after_source, "ON") else {
        let mut keywords = vec!["ON"];
        if source.alias.is_none() {
            keywords.insert(0, "AS");
        }
        return Some(context.keywords(&keywords, "0"));
    };

    let whens = (on + 1..tokens.len())
        .filter(|index| tokens[*index].depth == 0 && tokens[*index].is_word("WHEN"))
        .collect::<Vec<_>>();
    let Some(&when) = whens.last() else {
        let last = tokens.last();
        let expects_operand = last.is_some_and(|token| {
            token.is_word("ON")
                || token.is_word("AND")
                || token.is_word("OR")
                || matches!(token.kind, Kind::Operator | Kind::Open)
        });
        let (columns_sort, keywords_sort) = if expects_operand {
            ("0", "1")
        } else {
            ("1", "0")
        };
        let mut items = sorted(both(), columns_sort);
        if !expects_operand {
            let mut keywords = WHEN_KEYWORDS.to_vec();
            keywords.extend(["AND", "OR"]);
            items.extend(context.keywords(&keywords, keywords_sort));
        }
        return Some(items);
    };

    let clause = &tokens[when + 1..];
    let top = clause
        .iter()
        .filter(|token| token.depth == 0)
        .collect::<Vec<_>>();
    let kind = if top.first().is_some_and(|token| token.is_word("MATCHED")) {
        Some(When::Matched)
    } else if top.first().is_some_and(|token| token.is_word("NOT"))
        && top.get(1).is_some_and(|token| token.is_word("MATCHED"))
    {
        if top.get(2).is_some_and(|token| token.is_word("BY"))
            && top.get(3).is_some_and(|token| token.is_word("SOURCE"))
        {
            Some(When::NotMatchedBySource)
        } else {
            Some(When::NotMatched)
        }
    } else {
        None
    };

    let then = top.iter().position(|token| token.is_word("THEN"));
    let Some(then) = then else {
        return Some(match (kind, top.last()) {
            (None, None) => {
                context.keywords(&["MATCHED", "NOT MATCHED", "NOT MATCHED BY SOURCE"], "0")
            }
            (None, Some(_)) => {
                context.keywords(&["MATCHED", "MATCHED BY SOURCE", "MATCHED BY TARGET"], "0")
            }
            (Some(_), Some(last)) if last.is_word("AND") || matches!(last.kind, Kind::Operator) => {
                both()
            }
            (Some(_), Some(last)) if last.is_word("BY") => {
                context.keywords(&["TARGET", "SOURCE"], "0")
            }
            (Some(When::NotMatched), Some(last)) if last.is_word("MATCHED") => {
                context.keywords(&["THEN", "AND", "BY TARGET", "BY SOURCE"], "0")
            }
            (Some(_), _) => {
                let mut items = context.keywords(&["THEN", "AND"], "0");
                items.extend(sorted(both(), "1"));
                items
            }
        });
    };

    let action = &top[then + 1..];
    let inserting = kind == Some(When::NotMatched);
    let Some(verb) = action.first() else {
        let actions: &[&str] = if inserting {
            &["INSERT", "INSERT DEFAULT VALUES"]
        } else {
            &["UPDATE SET", "DELETE"]
        };
        return Some(context.keywords(actions, "0"));
    };
    let next_clauses = || {
        let mut keywords = WHEN_KEYWORDS.to_vec();
        keywords.push("OUTPUT");
        context.keywords(&keywords, "0")
    };

    if verb.is_word("DELETE") {
        return Some(next_clauses());
    }
    if verb.is_word("UPDATE") {
        let Some(set) = action.iter().position(|token| token.is_word("SET")) else {
            return Some(context.keywords(&["SET"], "0"));
        };
        let last = action.last().filter(|_| action.len() > set + 1);
        return Some(match last.map(|token| &token.kind) {
            None | Some(Kind::Comma) => context.columns(target_table, None),
            Some(Kind::Operator) => both(),
            _ => {
                let mut items = context.keywords(&[","], "0");
                items.extend(next_clauses());
                items
            }
        });
    }
    if verb.is_word("INSERT") {
        let action_start = clause
            .iter()
            .position(|token| token.depth == 0 && token.is_word("INSERT"))
            .map_or(0, |index| index + 1);
        let in_action = &clause[action_start..];
        let values = in_action
            .iter()
            .position(|token| token.depth == 0 && token.is_word("VALUES"));
        let open = inside_parentheses(clause);
        return Some(match (values, open) {
            (None, true) => context.columns(target_table, None),
            (Some(_), true) => {
                let mut items = both();
                items.extend(context.keywords(&["NULL", "DEFAULT"], "1"));
                items
            }
            (None, false) if in_action.is_empty() => {
                context.keywords(&["(", "VALUES", "DEFAULT VALUES"], "0")
            }
            (None, false) => context.keywords(&["VALUES"], "0"),
            (Some(_), false) => next_clauses(),
        });
    }
    None
}

/// Whether the cursor, after the last token, is inside parentheses.
fn inside_parentheses(tokens: &[Token]) -> bool {
    tokens
        .last()
        .is_some_and(|token| token.depth > 0 || matches!(token.kind, Kind::Open))
}

fn sorted(mut items: Vec<CompletionItem>, sort: &str) -> Vec<CompletionItem> {
    for item in &mut items {
        let label = item.label.clone();
        common::set_completion_sort_text(item, sort, &label);
    }
    items
}

fn names(qualifier: &str, relation: &Relation) -> bool {
    let qualifier = SqlParser::normalize_identifier(qualifier);
    relation
        .alias
        .as_deref()
        .is_some_and(|alias| alias.eq_ignore_ascii_case(&qualifier))
        || relation
            .reference
            .rsplit('.')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(&qualifier))
}

/// The table named at `start` (a dotted name, optionally `WITH (hints)` and an
/// alias), and the index after it; `None` when no name is complete there.
fn relation_at(tokens: &[Token], start: usize, stops: &[&str]) -> Option<(Relation, usize)> {
    let mut index = start;
    let first = tokens.get(index)?;
    if matches!(first.kind, Kind::Open) {
        // A derived source: `(SELECT …) AS s`.
        let depth = first.depth;
        index += 1;
        while tokens.get(index).is_some_and(|token| token.depth > depth) {
            index += 1;
        }
        if !tokens
            .get(index)
            .is_some_and(|token| matches!(token.kind, Kind::Close))
        {
            return None;
        }
        index += 1;
        let (alias, after) = alias_at(tokens, index, stops);
        let alias = alias?;
        return Some((
            Relation {
                reference: alias.clone(),
                alias: Some(alias),
                derived: Some(start),
            },
            after,
        ));
    }

    let mut parts = vec![first.name()?.to_string()];
    index += 1;
    while tokens
        .get(index)
        .is_some_and(|token| matches!(token.kind, Kind::Dot))
    {
        parts.push(tokens.get(index + 1)?.name()?.to_string());
        index += 2;
    }
    if index >= tokens.len() {
        // Still being typed.
        return None;
    }
    if tokens.get(index).is_some_and(|token| token.is_word("WITH"))
        && tokens
            .get(index + 1)
            .is_some_and(|token| matches!(token.kind, Kind::Open))
    {
        index += 2;
        while tokens.get(index).is_some_and(|token| token.depth > 0) {
            index += 1;
        }
        if tokens
            .get(index)
            .is_some_and(|token| matches!(token.kind, Kind::Close))
        {
            index += 1;
        }
    }
    let (alias, after) = alias_at(tokens, index, stops);
    Some((
        Relation {
            reference: parts.join("."),
            alias,
            derived: None,
        },
        after,
    ))
}

fn alias_at(tokens: &[Token], mut index: usize, stops: &[&str]) -> (Option<String>, usize) {
    if tokens.get(index).is_some_and(|token| token.is_word("AS")) {
        index += 1;
    }
    match tokens.get(index) {
        Some(token)
            if token.depth == 0
                && !stops.iter().any(|stop| token.is_word(stop))
                && token.name().is_some() =>
        {
            (token.name().map(str::to_string), index + 1)
        }
        _ => (None, index),
    }
}

fn position_of(tokens: &[Token], from: usize, word: &str) -> Option<usize> {
    (from..tokens.len()).find(|index| tokens[*index].depth == 0 && tokens[*index].is_word(word))
}

struct Statement {
    start: usize,
    keyword: &'static str,
}

/// The DML statement the tokens end in: its first token and its verb. A
/// leading `WITH` common table expression is passed over.
fn dml_statement(tokens: &[Token]) -> Option<Statement> {
    let start = tokens
        .iter()
        .rposition(|token| token.depth == 0 && matches!(token.kind, Kind::Semicolon))
        .map_or(0, |index| index + 1);
    tokens[start..]
        .iter()
        .enumerate()
        .filter(|(_, token)| token.depth == 0)
        .find_map(|(offset, token)| {
            ["MERGE", "INSERT", "UPDATE", "DELETE", "SELECT"]
                .into_iter()
                .find(|verb| token.is_word(verb))
                .map(|keyword| (start + offset, keyword))
        })
        .filter(|(_, keyword)| *keyword != "SELECT")
        .map(|(start, keyword)| Statement { start, keyword })
}

/// The table a DML statement writes: `INSERT [INTO] t`, `UPDATE t`,
/// `DELETE [FROM] t`, `MERGE [INTO] t`.
fn target_of(tokens: &[Token], keyword: &str) -> Option<Relation> {
    let mut index = 1;
    if tokens.get(index).is_some_and(|token| token.is_word("TOP")) {
        index += 1;
        while tokens
            .get(index)
            .is_some_and(|token| token.depth > 0 || matches!(token.kind, Kind::Open | Kind::Close))
        {
            index += 1;
        }
    }
    let skip = match keyword {
        "INSERT" | "MERGE" => "INTO",
        "DELETE" => "FROM",
        _ => "",
    };
    if !skip.is_empty() && tokens.get(index).is_some_and(|token| token.is_word(skip)) {
        index += 1;
    }
    let mut parts = vec![tokens.get(index)?.name()?.to_string()];
    index += 1;
    while tokens
        .get(index)
        .is_some_and(|token| matches!(token.kind, Kind::Dot))
    {
        parts.push(tokens.get(index + 1)?.name()?.to_string());
        index += 2;
    }
    Some(Relation {
        reference: parts.join("."),
        alias: None,
        derived: None,
    })
}

/// A derived table's columns, read from its select list: each item's alias or
/// last name, a `*` the columns of the table it selects from.
fn derived_table(context: &Context, tokens: &[Token], open: usize, name: &str) -> Table {
    let depth = tokens[open].depth + 1;
    let inner = tokens[open + 1..]
        .iter()
        .take_while(|token| token.depth >= depth)
        .collect::<Vec<_>>();
    let select = inner
        .iter()
        .position(|token| token.depth == depth && token.is_word("SELECT"));
    let from = inner
        .iter()
        .position(|token| token.depth == depth && token.is_word("FROM"));
    let base = from.and_then(|from| {
        let mut parts = vec![inner.get(from + 1)?.name()?.to_string()];
        let mut index = from + 2;
        while inner
            .get(index)
            .is_some_and(|token| matches!(token.kind, Kind::Dot))
        {
            parts.push(inner.get(index + 1)?.name()?.to_string());
            index += 2;
        }
        context.table(&parts.join("."))
    });

    let mut columns = Vec::new();
    let Some(select) = select else {
        return Table {
            name: name.to_string(),
            columns,
            ..Default::default()
        };
    };
    let list = &inner[select + 1..from.unwrap_or(inner.len())];
    // Past `DISTINCT` / `ALL` and `TOP n` / `TOP (n) [PERCENT]`.
    let mut start = 0;
    if list
        .first()
        .is_some_and(|token| token.is_word("DISTINCT") || token.is_word("ALL"))
    {
        start += 1;
    }
    if list.get(start).is_some_and(|token| token.is_word("TOP")) {
        start += 1;
        if list
            .get(start)
            .is_some_and(|token| matches!(token.kind, Kind::Open))
        {
            start += 1;
            while list.get(start).is_some_and(|token| token.depth > depth) {
                start += 1;
            }
        }
        start += 1;
        if list
            .get(start)
            .is_some_and(|token| token.is_word("PERCENT"))
        {
            start += 1;
        }
    }
    let start = start.min(list.len());
    for item in
        list[start..].split(|token| token.depth == depth && matches!(token.kind, Kind::Comma))
    {
        let top = item
            .iter()
            .filter(|token| token.depth == depth)
            .collect::<Vec<_>>();
        if top
            .last()
            .is_some_and(|token| matches!(token.kind, Kind::Operator))
        {
            if let Some(base) = base {
                columns.extend(base.columns.iter().cloned());
            }
            continue;
        }
        let named = match top.iter().position(|token| token.is_word("AS")) {
            Some(alias) => top.get(alias + 1).and_then(|token| token.name()),
            None => top.last().and_then(|token| token.name()),
        };
        if let Some(column) = named {
            let known = base.and_then(|base| {
                base.columns
                    .iter()
                    .find(|candidate| candidate.name.eq_ignore_ascii_case(column))
            });
            columns.push(known.cloned().unwrap_or_else(|| Column {
                name: column.to_string(),
                ..Default::default()
            }));
        }
    }
    Table {
        name: name.to_string(),
        columns,
        ..Default::default()
    }
}

/// The word being typed at the end of the text (empty after a space or a dot).
fn partial_word(text: &str) -> &str {
    let start = text
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_name_char(*character))
        .last()
        .map_or(text.len(), |(index, _)| index);
    &text[start..]
}

fn is_name_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '@' | '#' | '$')
}

/// The tokens of the statement the text ends in.
fn statement_tokens(text: &str) -> Vec<Token> {
    let mut tokens = tokenize(text);
    if let Some(end) = tokens
        .iter()
        .rposition(|token| token.depth == 0 && matches!(token.kind, Kind::Semicolon))
    {
        tokens.drain(..=end);
    }
    tokens
}

fn tokenize(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut depth = 0usize;
    let mut index = 0;
    let mut line_start = true;
    while index < bytes.len() {
        let byte = bytes[index];
        let start = index;
        let kind = match byte {
            b'\n' => {
                line_start = true;
                index += 1;
                continue;
            }
            b' ' | b'\t' | b'\r' => {
                index += 1;
                continue;
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = text[index + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| index + 2 + end + 2);
                continue;
            }
            b'\'' => {
                index = quoted_end(bytes, index, b'\'');
                Kind::Literal
            }
            b'"' | b'[' => {
                let close = if byte == b'"' { b'"' } else { b']' };
                index = quoted_end(bytes, index, close);
                let inner = &text[start + 1..index.saturating_sub(1).max(start + 1)];
                let doubled = if close == b'"' { "\"\"" } else { "]]" };
                Kind::Name(inner.replace(doubled, &doubled[..1]))
            }
            b'.' => {
                index += 1;
                Kind::Dot
            }
            b',' => {
                index += 1;
                Kind::Comma
            }
            b'(' => {
                index += 1;
                tokens.push(Token {
                    kind: Kind::Open,
                    depth,
                });
                depth += 1;
                line_start = false;
                continue;
            }
            b')' => {
                index += 1;
                depth = depth.saturating_sub(1);
                Kind::Close
            }
            b';' => {
                index += 1;
                Kind::Semicolon
            }
            b'=' | b'<' | b'>' | b'!' | b'+' | b'-' | b'*' | b'/' | b'%' => {
                index += 1;
                Kind::Operator
            }
            _ if is_name_char(text[index..].chars().next().unwrap_or(' ')) => {
                while let Some(character) = text[index..].chars().next() {
                    if !is_name_char(character) {
                        break;
                    }
                    index += character.len_utf8();
                }
                let word = &text[start..index];
                let rest = text[index..].split('\n').next().unwrap_or("");
                if line_start
                    && word.eq_ignore_ascii_case("GO")
                    && rest
                        .trim()
                        .chars()
                        .all(|character| character.is_ascii_digit())
                {
                    Kind::Semicolon
                } else {
                    Kind::Word(word.to_string())
                }
            }
            _ => {
                index += text[index..].chars().next().map_or(1, char::len_utf8);
                line_start = false;
                continue;
            }
        };
        line_start = false;
        tokens.push(Token { kind, depth });
    }
    tokens
}

fn quoted_end(bytes: &[u8], start: usize, close: u8) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == close {
            if bytes.get(index + 1) == Some(&close) {
                index += 2;
                continue;
            }
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<Kind> {
        tokenize(text).into_iter().map(|token| token.kind).collect()
    }

    #[test]
    fn tokens_read_brackets_go_and_depth() {
        assert_eq!(
            kinds("[dbo].[a]]b] x\nGO\n(\"q\")"),
            vec![
                Kind::Name("dbo".into()),
                Kind::Dot,
                Kind::Name("a]b".into()),
                Kind::Word("x".into()),
                Kind::Semicolon,
                Kind::Open,
                Kind::Name("q".into()),
                Kind::Close,
            ]
        );
        let tokens = tokenize("a (b (c))");
        assert_eq!(
            tokens.iter().map(|token| token.depth).collect::<Vec<_>>(),
            [0, 0, 1, 1, 2, 1, 0]
        );
    }

    #[test]
    fn the_target_of_each_dml_statement() {
        for (sql, keyword) in [
            ("INSERT INTO [dbo].[orders] (id)", "INSERT"),
            ("UPDATE TOP (5) dbo.orders SET", "UPDATE"),
            ("DELETE FROM dbo.orders WHERE", "DELETE"),
            ("MERGE INTO dbo.orders AS t", "MERGE"),
        ] {
            let tokens = tokenize(sql);
            assert_eq!(
                target_of(&tokens, keyword).unwrap().reference,
                "dbo.orders",
                "{sql}"
            );
        }
    }
}
