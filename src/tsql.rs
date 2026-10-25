//! T-SQL (SQL Server) lexical forms the shared grammar does not know.
//!
//! tree-sitter-sequel has no bracketed names, `#temp` tables, `@@` globals,
//! `TOP`, `APPLY` or `GO` batches. The parser therefore reads a T-SQL
//! document through a rewrite of the same byte length, so every position it
//! reports is still a position in the user's text:
//!
//! - `[a b]` → `"a b"` (an escaped `]]` → `""`);
//! - `#t` / `##t` → `_t` / `__t`;
//! - `@@ROWCOUNT` → `__ROWCOUNT`;
//! - `TOP 10`, `TOP (@n) PERCENT WITH TIES` → spaces;
//! - `CROSS APPLY` / `OUTER APPLY` → `CROSS JOIN ` (a lateral row source);
//! - table hints, `WITH (NOLOCK, INDEX(ix))` → spaces;
//! - the `OUTPUT` clause of `INSERT` / `UPDATE` / `DELETE` / `MERGE`, a
//!   `MERGE`'s `BY TARGET` / `BY SOURCE`, `OFFSET n ROWS` / `FETCH NEXT n ROWS
//!   ONLY`'s words after the count, and `TRAN` → spaces;
//! - a type's `(max)` → `(1  )`;
//! - a line holding only `GO [count]` → `;` and spaces.
//!
//! Strings, comments and quoted names are left as they are. The text a
//! request answers from (completion labels, edits) is never rewritten.
//!
//! Code that masks or scans SQL without a parser (`SqlParser::mask_sql_noise`
//! and the placeholder scanner) learns that it reads T-SQL from [`scope`]:
//! the server analyzes a T-SQL document's request inside it, so `#name` is a
//! temp table there rather than a MySQL comment, and `GO` ends a batch.

tokio::task_local! {
    static TSQL: bool;
}

/// SQL Server's table hints, as `WITH (NOLOCK)` or the older `t (NOLOCK)`.
const TABLE_HINTS: &[&str] = &[
    "FORCESEEK",
    "FORCESCAN",
    "HOLDLOCK",
    "INDEX",
    "NOEXPAND",
    "NOLOCK",
    "NOWAIT",
    "PAGLOCK",
    "READCOMMITTED",
    "READCOMMITTEDLOCK",
    "READPAST",
    "READUNCOMMITTED",
    "REPEATABLEREAD",
    "ROWLOCK",
    "SERIALIZABLE",
    "SNAPSHOT",
    "SPATIAL_WINDOW_MAX_CELLS",
    "TABLOCK",
    "TABLOCKX",
    "UPDLOCK",
    "XLOCK",
];

/// Whether a name is one of SQL Server's table hints.
pub fn is_table_hint(name: &str) -> bool {
    TABLE_HINTS
        .iter()
        .any(|hint| name.eq_ignore_ascii_case(hint))
}

/// Whether a dialect name, a registered one or its alias, is T-SQL.
pub fn is_tsql_dialect(dialect: &str) -> bool {
    dialect.eq_ignore_ascii_case("mssql") || dialect.eq_ignore_ascii_case("tsql")
}

/// Whether the current request analyzes a T-SQL document.
pub fn is_active() -> bool {
    TSQL.try_with(|active| *active).unwrap_or(false)
}

/// Runs a request's future with T-SQL lexing on when `active`.
pub async fn scope<F: std::future::Future>(active: bool, future: F) -> F::Output {
    TSQL.scope(active, future).await
}

/// The synchronous form of [`scope`].
pub fn sync_scope<R>(active: bool, f: impl FnOnce() -> R) -> R {
    TSQL.sync_scope(active, f)
}

/// The same-length rewrite the parser reads a T-SQL document through.
pub fn rewrite_for_parser(source: &str) -> String {
    let mut bytes = source.as_bytes().to_vec();
    let mut index = 0;
    // The DML verb of the statement at each parenthesis depth.
    let mut verbs: Vec<Option<&'static str>> = vec![None];
    let mut previous_word: Option<(usize, usize)> = None;
    while index < bytes.len() {
        match bytes[index] {
            b'(' => {
                rewrite_max(&mut bytes, index);
                verbs.push(None);
                index += 1;
            }
            b')' => {
                if verbs.len() > 1 {
                    verbs.pop();
                }
                index += 1;
            }
            b';' => {
                *verbs.last_mut().expect("a statement level") = None;
                index += 1;
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => index = line_end(&bytes, index),
            b'/' if bytes.get(index + 1) == Some(&b'*') => index = block_comment_end(&bytes, index),
            b'\'' | b'"' => index = quoted_end(&bytes, index, bytes[index]),
            b'[' => index = rewrite_bracketed(&mut bytes, index),
            b'#' if !follows_name(&bytes, index) => {
                let mut end = index;
                while bytes.get(end) == Some(&b'#') {
                    end += 1;
                }
                if bytes.get(end).is_some_and(|byte| is_name_byte(*byte)) {
                    bytes[index..end].fill(b'_');
                }
                index = end;
            }
            b'@' if bytes.get(index + 1) == Some(&b'@') && !follows_name(&bytes, index) => {
                bytes[index] = b'_';
                bytes[index + 1] = b'_';
                index += 2;
            }
            byte if is_name_byte(byte) && !follows_name(&bytes, index) => {
                let end = word_end(&bytes, index);
                let word = &source[index..end];
                let previous = previous_word.map(|(start, end)| &source[start..end]);
                previous_word = Some((index, end));
                let verb = verbs.last_mut().expect("a statement level");
                if let Some(dml) = ["INSERT", "UPDATE", "DELETE", "MERGE"]
                    .into_iter()
                    .find(|dml| word.eq_ignore_ascii_case(dml))
                {
                    if verb.is_none() || dml == "MERGE" {
                        *verb = Some(dml);
                    }
                } else if word.eq_ignore_ascii_case("GO") {
                    *verb = None;
                } else if word.eq_ignore_ascii_case("OUTPUT") && verb.is_some() {
                    let clause_end = output_clause_end(&bytes, end);
                    blank(&mut bytes, index, clause_end);
                    index = clause_end;
                    continue;
                } else if word.eq_ignore_ascii_case("BY")
                    && previous.is_some_and(|previous| previous.eq_ignore_ascii_case("MATCHED"))
                {
                    let next = skip_spaces(&bytes, end);
                    let next_end = word_end(&bytes, next);
                    let side = &source[next..next_end];
                    if side.eq_ignore_ascii_case("TARGET") || side.eq_ignore_ascii_case("SOURCE") {
                        blank(&mut bytes, index, next_end);
                        index = next_end;
                        continue;
                    }
                } else if word.eq_ignore_ascii_case("OFFSET") {
                    if let Some(rows_end) = offset_rows_end(&bytes, end) {
                        blank(&mut bytes, index, rows_end);
                        index = rows_end;
                        continue;
                    }
                } else if word.eq_ignore_ascii_case("FETCH") {
                    let next = skip_spaces(&bytes, end);
                    let next_end = word_end(&bytes, next);
                    let direction = &source[next..next_end];
                    if direction.eq_ignore_ascii_case("NEXT")
                        || direction.eq_ignore_ascii_case("FIRST")
                    {
                        let only = find_word(source, next_end, "ONLY").unwrap_or(next_end);
                        blank(&mut bytes, index, only);
                        index = only;
                        continue;
                    }
                } else if (word.eq_ignore_ascii_case("TRAN")
                    || word.eq_ignore_ascii_case("TRANSACTION"))
                    && previous.is_some_and(|previous| {
                        ["BEGIN", "COMMIT", "ROLLBACK", "SAVE"]
                            .iter()
                            .any(|keyword| previous.eq_ignore_ascii_case(keyword))
                    })
                {
                    blank(&mut bytes, index, end);
                    index = end;
                    continue;
                }
                if word.eq_ignore_ascii_case("TOP") {
                    if let Some(clause_end) = top_clause_end(&bytes, end) {
                        blank(&mut bytes, index, clause_end);
                        index = clause_end;
                        continue;
                    }
                } else if word.eq_ignore_ascii_case("WITH") {
                    if let Some(hints_end) = table_hints_end(&bytes, end) {
                        blank(&mut bytes, index, hints_end);
                        index = hints_end;
                        continue;
                    }
                } else if word.eq_ignore_ascii_case("CROSS") || word.eq_ignore_ascii_case("OUTER") {
                    let next = skip_spaces(&bytes, end);
                    let next_end = word_end(&bytes, next);
                    if next_end > next && source[next..next_end].eq_ignore_ascii_case("APPLY") {
                        bytes[index..end].copy_from_slice(b"CROSS");
                        bytes[next..next_end].copy_from_slice(b"JOIN ");
                        index = next_end;
                        continue;
                    }
                }
                index = end;
            }
            _ => index += 1,
        }
    }
    mark_batch_separators(&mut bytes);
    // Every rewrite replaces ASCII with ASCII, so the text stays valid UTF-8.
    String::from_utf8(bytes).expect("the T-SQL rewrite preserves UTF-8")
}

/// Turns each line that holds only `GO` (optionally a count) into `;` and
/// spaces, so a batch ends where SQL Server's tools end it.
pub fn mark_batch_separators(bytes: &mut [u8]) {
    let mut start = 0;
    while start < bytes.len() {
        let end = bytes[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |offset| start + offset);
        let first = skip_spaces(bytes, start);
        if first + 1 < end
            && bytes[first].eq_ignore_ascii_case(&b'g')
            && bytes[first + 1].eq_ignore_ascii_case(&b'o')
        {
            let mut rest = skip_spaces(bytes, first + 2);
            while rest < end && bytes[rest].is_ascii_digit() {
                rest += 1;
            }
            let rest = skip_spaces(bytes, rest);
            let comment = rest + 1 < end && bytes[rest] == b'-' && bytes[rest + 1] == b'-';
            if rest >= end || comment || bytes[rest] == b'\r' {
                let separator_end = if comment { rest } else { end };
                blank(bytes, first, separator_end);
                bytes[first] = b';';
            }
        }
        start = end + 1;
    }
}

/// Masks a T-SQL line's `GO` in text that is already masked for scanning.
pub(crate) fn mark_batch_separators_in(text: &mut String) {
    let mut bytes = std::mem::take(text).into_bytes();
    mark_batch_separators(&mut bytes);
    *text = String::from_utf8(bytes).expect("masking preserves UTF-8");
}

fn rewrite_bracketed(bytes: &mut [u8], start: usize) -> usize {
    let mut index = start + 1;
    let mut close = None;
    while index < bytes.len() {
        match bytes[index] {
            b']' if bytes.get(index + 1) == Some(&b']') => index += 2,
            b']' => {
                close = Some(index);
                break;
            }
            // A name never spans lines.
            b'\n' => return index,
            _ => index += 1,
        }
    }
    let Some(close) = close else {
        return bytes.len();
    };
    bytes[start] = b'"';
    bytes[close] = b'"';
    let mut inner = start + 1;
    while inner < close {
        // A `"` in the name would need doubling, which changes the length:
        // the parser reads the name with `_` there, the text keeps it.
        if bytes[inner] == b'"' {
            bytes[inner] = b'_';
            inner += 1;
        } else if bytes[inner] == b']' {
            bytes[inner] = b'"';
            bytes[inner + 1] = b'"';
            inner += 2;
        } else {
            inner += 1;
        }
    }
    close + 1
}

/// The end of `TOP n` / `TOP (expr)` with its `PERCENT` and `WITH TIES`, or
/// `None` when `TOP` is not followed by a count (a column named `top`).
fn top_clause_end(bytes: &[u8], after_top: usize) -> Option<usize> {
    let start = skip_spaces(bytes, after_top);
    let mut end = match bytes.get(start)? {
        b'(' => {
            let mut depth = 0usize;
            let mut index = start;
            loop {
                match bytes.get(index)? {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break index + 1;
                        }
                    }
                    _ => {}
                }
                index += 1;
            }
        }
        byte if byte.is_ascii_digit() => {
            let mut index = start;
            while bytes
                .get(index)
                .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'.')
            {
                index += 1;
            }
            index
        }
        _ => return None,
    };
    for keywords in [&["PERCENT"][..], &["WITH", "TIES"][..]] {
        let mut cursor = end;
        let mut matched = true;
        for keyword in keywords {
            let word_start = skip_spaces(bytes, cursor);
            let word_end = word_end(bytes, word_start);
            if !bytes[word_start..word_end].eq_ignore_ascii_case(keyword.as_bytes()) {
                matched = false;
                break;
            }
            cursor = word_end;
        }
        if matched {
            end = cursor;
        }
    }
    Some(end)
}

/// The end of `WITH (hint, ...)` when every item of the list starts with a
/// table hint; a CTE's `WITH name AS (` is never followed by `(`.
fn table_hints_end(bytes: &[u8], after_with: usize) -> Option<usize> {
    let open = skip_spaces(bytes, after_with);
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut index = open;
    let close = loop {
        match bytes.get(index)? {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    break index;
                }
            }
            b'\n' | b';' => return None,
            _ => {}
        }
        index += 1;
    };
    let list = std::str::from_utf8(&bytes[open + 1..close]).ok()?;
    let mut depth = 0usize;
    let mut items = vec![String::new()];
    for character in list.chars() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => items.push(String::new()),
            _ if depth == 0 => items.last_mut()?.push(character),
            _ => {}
        }
    }
    items
        .iter()
        .all(|item| {
            let word = item
                .trim()
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .next();
            word.is_some_and(is_table_hint)
        })
        .then_some(close + 1)
}

/// The end of an `OUTPUT` clause: the next `FROM`, `WHERE`, `VALUES`,
/// `SELECT`, `DEFAULT`, `OPTION` or `WHEN` at its level, a `;`, or a `GO`.
fn output_clause_end(bytes: &[u8], start: usize) -> usize {
    const ENDS: &[&str] = &[
        "FROM", "WHERE", "VALUES", "SELECT", "DEFAULT", "OPTION", "WHEN", "GO",
    ];
    let mut depth = 0usize;
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'-' if bytes.get(index + 1) == Some(&b'-') => index = line_end(bytes, index),
            b'/' if bytes.get(index + 1) == Some(&b'*') => index = block_comment_end(bytes, index),
            b'\'' | b'"' => index = quoted_end(bytes, index, bytes[index]),
            b'[' => index = quoted_end(bytes, index, b']'),
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' if depth == 0 => return index,
            b')' => {
                depth -= 1;
                index += 1;
            }
            b';' if depth == 0 => return index,
            byte if is_name_byte(byte) && !follows_name(bytes, index) => {
                let end = word_end(bytes, index);
                if depth == 0
                    && ENDS
                        .iter()
                        .any(|word| bytes[index..end].eq_ignore_ascii_case(word.as_bytes()))
                {
                    return index;
                }
                index = end;
            }
            _ => index += 1,
        }
    }
    bytes.len()
}

/// The end of `OFFSET <count> ROWS` (the grammar knows `OFFSET` only after a
/// `LIMIT`), or `None` when no `ROWS` follows the count.
fn offset_rows_end(bytes: &[u8], after_offset: usize) -> Option<usize> {
    let start = skip_spaces(bytes, after_offset);
    let count_end = if bytes.get(start) == Some(&b'(') {
        let mut depth = 0usize;
        let mut index = start;
        loop {
            match bytes.get(index)? {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break index + 1;
                    }
                }
                _ => {}
            }
            index += 1;
        }
    } else {
        let mut index = start;
        while bytes
            .get(index)
            .is_some_and(|byte| is_name_byte(*byte) || *byte == b'@')
        {
            index += 1;
        }
        index
    };
    if count_end == start {
        return None;
    }
    let rows = skip_spaces(bytes, count_end);
    let rows_end = word_end(bytes, rows);
    let word = &bytes[rows..rows_end];
    (word.eq_ignore_ascii_case(b"ROWS") || word.eq_ignore_ascii_case(b"ROW")).then_some(rows_end)
}

/// The end of the next whole word `word` at or after `start`.
fn find_word(source: &str, start: usize, word: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = start;
    while index < bytes.len() {
        if is_name_byte(bytes[index]) && !follows_name(bytes, index) {
            let end = word_end(bytes, index);
            if source[index..end].eq_ignore_ascii_case(word) {
                return Some(end);
            }
            index = end;
        } else if bytes[index] == b';' {
            return None;
        } else {
            index += 1;
        }
    }
    None
}

/// A type's `(max)` as `(1  )`, a length the grammar reads.
fn rewrite_max(bytes: &mut [u8], open: usize) {
    let start = skip_spaces(bytes, open + 1);
    let end = start + 3;
    if bytes.len() >= end
        && bytes[start..end].eq_ignore_ascii_case(b"MAX")
        && bytes.get(skip_spaces(bytes, end)) == Some(&b')')
    {
        bytes[start..end].copy_from_slice(b"1  ");
    }
}

fn blank(bytes: &mut [u8], start: usize, end: usize) {
    for byte in &mut bytes[start..end] {
        if !matches!(*byte, b'\n' | b'\r') {
            *byte = b' ';
        }
    }
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

fn follows_name(bytes: &[u8], index: usize) -> bool {
    index > 0 && (is_name_byte(bytes[index - 1]) || matches!(bytes[index - 1], b'@' | b'#' | b'$'))
}

fn word_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while bytes.get(end).is_some_and(|byte| is_name_byte(*byte)) {
        end += 1;
    }
    end
}

fn skip_spaces(bytes: &[u8], mut index: usize) -> usize {
    while bytes
        .get(index)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        index += 1;
    }
    index
}

fn line_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |offset| start + offset)
}

fn block_comment_end(bytes: &[u8], start: usize) -> usize {
    bytes[start + 2..]
        .windows(2)
        .position(|pair| pair == b"*/")
        .map_or(bytes.len(), |offset| start + 2 + offset + 2)
}

fn quoted_end(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == quote {
            if bytes.get(index + 1) == Some(&quote) {
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

    fn rewritten(source: &str) -> String {
        let result = rewrite_for_parser(source);
        assert_eq!(result.len(), source.len(), "the rewrite keeps every offset");
        result
    }

    #[test]
    fn bracketed_names_become_quoted_names() {
        assert_eq!(
            rewritten("SELECT [Tenant ID], [a]]b] FROM [dbo].[orders]"),
            r#"SELECT "Tenant ID", "a""b" FROM "dbo"."orders""#
        );
    }

    #[test]
    fn temp_tables_and_globals_become_names() {
        assert_eq!(
            rewritten("SELECT @@ROWCOUNT FROM #t JOIN ##g ON 1 = 1"),
            "SELECT __ROWCOUNT FROM _t JOIN __g ON 1 = 1"
        );
    }

    #[test]
    fn top_is_blanked_with_its_count() {
        assert_eq!(
            rewritten("SELECT TOP (10) PERCENT WITH TIES name FROM t"),
            format!(
                "SELECT {} name FROM t",
                " ".repeat("TOP (10) PERCENT WITH TIES".len())
            )
        );
        assert_eq!(
            rewritten("select top 5 name from t"),
            "select       name from t"
        );
        assert_eq!(
            rewritten("SELECT top FROM t"),
            "SELECT top FROM t",
            "a column named top"
        );
    }

    #[test]
    fn table_hints_are_blanked() {
        assert_eq!(
            rewritten("FROM t WITH (NOLOCK, INDEX(ix_t)) JOIN u WITH (UPDLOCK) ON 1 = 1"),
            "FROM t WITH (NOLOCK, INDEX(ix_t)) JOIN u WITH (UPDLOCK) ON 1 = 1"
                .replace("WITH (NOLOCK, INDEX(ix_t))", &" ".repeat(26))
                .replace("WITH (UPDLOCK)", &" ".repeat(14))
        );
        let cte = "WITH totals (id) AS (SELECT 1) SELECT * FROM totals";
        assert_eq!(rewritten(cte), cte, "a CTE's column list is not a hint");
    }

    #[test]
    fn output_clauses_are_blanked_in_dml_only() {
        let source =
            "UPDATE t SET a = 1 OUTPUT inserted.a, deleted.a INTO @log (a, b) WHERE id = 5";
        let expected = source.replace(
            "OUTPUT inserted.a, deleted.a INTO @log (a, b) ",
            &" ".repeat(46),
        );
        assert_eq!(rewritten(source), expected);
        let merge =
            "MERGE t USING s ON t.id = s.id WHEN NOT MATCHED BY TARGET THEN DELETE OUTPUT $action;";
        let expected = merge
            .replace("BY TARGET", &" ".repeat(9))
            .replace("OUTPUT $action", &" ".repeat(14));
        assert_eq!(rewritten(merge), expected);
        let select = "SELECT output FROM t";
        assert_eq!(rewritten(select), select, "a column named output");
    }

    #[test]
    fn offset_fetch_max_and_tran_are_read_as_the_grammar_knows_them() {
        let source = "SELECT a FROM t ORDER BY a OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY";
        let expected = source.replace("OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY", &" ".repeat(37));
        assert_eq!(rewritten(source), expected);
        assert_eq!(
            rewritten("CREATE TABLE t (a nvarchar(max))"),
            "CREATE TABLE t (a nvarchar(1  ))"
        );
        assert_eq!(
            rewritten("BEGIN TRAN; COMMIT TRANSACTION;"),
            "BEGIN     ; COMMIT            ;"
        );
    }

    #[test]
    fn a_quote_in_a_bracketed_name_is_read_as_an_underscore() {
        assert_eq!(
            rewritten(r#"SELECT [a"b] FROM t"#),
            r#"SELECT "a_b" FROM t"#
        );
    }

    #[test]
    fn apply_becomes_a_join() {
        assert_eq!(
            rewritten("FROM a OUTER APPLY f(a.id) x CROSS  APPLY g(x.id) y"),
            "FROM a CROSS JOIN  f(a.id) x CROSS  JOIN  g(x.id) y"
        );
    }

    #[test]
    fn go_lines_end_batches() {
        assert_eq!(
            rewritten("SELECT 1\nGO\nselect 2\n  go 3 -- twice\nSELECT 3\ngone"),
            "SELECT 1\n; \nselect 2\n  ;    -- twice\nSELECT 3\ngone"
        );
        assert_eq!(rewritten("SELECT 1\r\nGO\r\n"), "SELECT 1\r\n; \r\n");
    }

    #[test]
    fn strings_comments_and_quoted_names_are_left_alone() {
        let source = "SELECT '#t [x] TOP 1' -- #t [x]\n/* @@x */ \"[y]\"";
        assert_eq!(rewritten(source), source);
    }

    #[test]
    fn the_scope_tells_whether_tsql_is_read() {
        assert!(!is_active());
        assert!(sync_scope(true, is_active));
        assert!(!sync_scope(false, is_active));
    }
}
