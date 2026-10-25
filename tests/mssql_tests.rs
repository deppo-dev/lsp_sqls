//! The `mssql` (T-SQL) dialect over the language server's own protocol:
//! bracketed names, `#temp` tables, `TOP`, `APPLY` and `GO` batches.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const DDL_URI: &str = "file:///workspace/ddl/AppDb.dbo.sql";
const SCHEMA_ID: &str = "8f6d3c7e-0c55-4f0e-9a43-4b8d6a7c2e11";

struct LspProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl LspProcess {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sql-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sql-lsp test binary");
        let stdin = child.stdin.take().expect("sql-lsp stdin");
        let stdout = BufReader::new(child.stdout.take().expect("sql-lsp stdout"));
        let mut lsp = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        lsp.request(
            "initialize",
            json!({ "processId": null, "rootUri": null, "capabilities": {} }),
        );
        lsp.notify("initialized", json!({}));
        lsp
    }

    /// Pushes the `AppDb.dbo` schema for `uri`, opens it, and returns the
    /// diagnostics published for it.
    fn open_tsql(&mut self, uri: &str, language_id: &str, text: &str) -> Vec<Value> {
        self.notify(
            "workspace/didChangeConfiguration",
            json!({
                "settings": {
                    "schemas": [schema()],
                    "fileSchemas": { (uri): SCHEMA_ID },
                }
            }),
        );
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": { "uri": uri, "languageId": language_id, "version": 1, "text": text }
            }),
        );
        loop {
            let message = self.read_message();
            if message.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
                && message.pointer("/params/uri").and_then(Value::as_str) == Some(uri)
            {
                return message
                    .pointer("/params/diagnostics")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
            }
        }
    }

    fn completion_labels(&mut self, uri: &str, text: &str, marker: &str) -> Vec<String> {
        let result = self.request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": uri },
                "position": position_after(text, marker),
            }),
        );
        let items = result
            .get("items")
            .or(Some(&result))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        items
            .iter()
            .filter_map(|item| item.get("label").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        );
        loop {
            let message = self.read_message();
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                assert!(
                    message.get("error").is_none(),
                    "LSP request {method} failed: {message}"
                );
                return message.get("result").cloned().unwrap_or(Value::Null);
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write_message(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn write_message(&mut self, message: &Value) {
        let body = serde_json::to_vec(message).expect("serialize LSP message");
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write LSP header");
        self.stdin.write_all(&body).expect("write LSP body");
        self.stdin.flush().expect("flush LSP body");
    }

    fn read_message(&mut self) -> Value {
        let mut content_length = None;
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).expect("read LSP header") > 0,
                "sql-lsp stdout closed"
            );
            let header = line.trim_end_matches(['\r', '\n']);
            if header.is_empty() {
                break;
            }
            if let Some(value) = header.strip_prefix("Content-Length:") {
                content_length = Some(value.trim().parse::<usize>().expect("content length"));
            }
        }
        let mut body = vec![0; content_length.expect("Content-Length header")];
        self.stdout.read_exact(&mut body).expect("read LSP body");
        serde_json::from_slice(&body).expect("parse LSP body")
    }
}

impl Drop for LspProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn column(name: &str, data_type: &str, line: u32) -> Value {
    json!({
        "name": name, "data_type": data_type, "nullable": false, "primary_key": name == "id",
        "unique": name == "id", "indexed": name == "id", "comment": null,
        "source_location": [DDL_URI, line]
    })
}

fn schema() -> Value {
    json!({
        "id": SCHEMA_ID,
        "catalog": "AppDb",
        "database": "dbo",
        "server_version": "16.0",
        "tables": [
            {
                "name": "customer",
                "columns": [column("id", "int", 2), column("name", "nvarchar(200)", 3)],
                "indexes": [], "constraints": [], "comment": null,
                "source_location": [DDL_URI, 1]
            },
            {
                "name": "orders",
                "columns": [
                    column("id", "int", 6),
                    column("customer_id", "int", 7),
                    column("Order Total", "money", 8)
                ],
                "indexes": [], "constraints": [], "comment": null,
                "source_location": [DDL_URI, 5]
            }
        ],
        "functions": [],
        "source_uri": null
    })
}

fn position_after(text: &str, marker: &str) -> Value {
    let offset = text.find(marker).expect("marker in text") + marker.len();
    let before = &text[..offset];
    let line = before.matches('\n').count();
    let character = before[before.rfind('\n').map_or(0, |index| index + 1)..]
        .encode_utf16()
        .count();
    json!({ "line": line, "character": character })
}

fn syntax_errors(diagnostics: &[Value]) -> Vec<String> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.get("code").and_then(Value::as_str) == Some("SYNTAX_ERROR"))
        .map(|diagnostic| diagnostic.to_string())
        .collect()
}

fn errors(diagnostics: &[Value]) -> Vec<String> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.get("severity").and_then(Value::as_u64) == Some(1))
        .map(|diagnostic| diagnostic.to_string())
        .collect()
}

#[test]
fn columns_complete_after_a_bracketed_alias_with_top() {
    let mut lsp = LspProcess::spawn();
    let uri = "file:///workspace/report.sql";
    let text = "SELECT TOP (5) c. FROM [dbo].[customer] AS c";
    lsp.open_tsql(uri, "mssql", text);

    let labels = lsp.completion_labels(uri, text, "c.");
    assert!(
        labels.contains(&"id".to_string()) && labels.contains(&"name".to_string()),
        "{labels:?}"
    );
}

#[test]
fn a_temp_table_does_not_hide_the_rest_of_its_line() {
    let mut lsp = LspProcess::spawn();
    let uri = "file:///workspace/temp.sql";
    let text = "SELECT o. FROM #seen s JOIN dbo.orders o ON o.id = s.id";
    lsp.open_tsql(uri, "mssql", text);

    let labels = lsp.completion_labels(uri, text, "o.");
    assert!(labels.contains(&"customer_id".to_string()), "{labels:?}");
}

#[test]
fn cross_apply_keeps_the_row_sources_before_it() {
    let mut lsp = LspProcess::spawn();
    let uri = "file:///workspace/apply.sql";
    let text = "SELECT c. FROM dbo.customer c CROSS APPLY (SELECT TOP 1 o.id FROM dbo.orders o WHERE o.customer_id = c.id) last_order";
    lsp.open_tsql(uri, "mssql", text);

    let labels = lsp.completion_labels(uri, text, "SELECT c.");
    assert!(labels.contains(&"name".to_string()), "{labels:?}");
}

#[test]
fn a_tsql_script_with_batches_has_no_syntax_errors() {
    let mut lsp = LspProcess::spawn();
    let uri = "file:///workspace/script.sql";
    let text = "SELECT TOP 10 [Order Total] FROM [dbo].[orders] WITH (NOLOCK) ORDER BY id\nGO\nSELECT c.name, o.[Order Total]\nFROM dbo.customer c\nOUTER APPLY (SELECT TOP 1 * FROM dbo.orders x WHERE x.customer_id = c.id) o\nGO\nSELECT COUNT(*) FROM #seen\n";
    let diagnostics = lsp.open_tsql(uri, "mssql", text);
    assert!(errors(&diagnostics).is_empty(), "{diagnostics:?}");
}

#[test]
fn a_bracketed_table_goes_to_its_ddl_line_and_its_usages() {
    let mut lsp = LspProcess::spawn();
    let uri = "file:///workspace/usages.sql";
    let text = "SELECT * FROM [dbo].[orders]\nGO\nSELECT id FROM dbo.orders";
    lsp.open_tsql(uri, "mssql", text);

    let definition = lsp.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": position_after(text, "[dbo].[ord") }),
    );
    let location = definition
        .as_array()
        .and_then(|locations| locations.first())
        .unwrap_or(&definition);
    assert_eq!(
        location.get("uri").and_then(Value::as_str),
        Some(DDL_URI),
        "{definition}"
    );
    assert_eq!(
        location
            .pointer("/range/start/line")
            .and_then(Value::as_u64),
        Some(4)
    );

    let references = lsp.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri },
            "position": position_after(text, "FROM dbo.ord"),
            "context": { "includeDeclaration": true }
        }),
    );
    let lines = references
        .as_array()
        .expect("references")
        .iter()
        .filter_map(|location| {
            location
                .pointer("/range/start/line")
                .and_then(Value::as_u64)
        })
        .collect::<Vec<_>>();
    assert!(lines.contains(&0) && lines.contains(&2), "{references}");
}

#[test]
fn format_keeps_brackets_variables_and_batches() {
    let mut lsp = LspProcess::spawn();
    let uri = "file:///workspace/format.sql";
    let text = "select [Order Total] from [dbo].[orders] where id = @id\ngo\nselect 1";
    lsp.open_tsql(uri, "mssql", text);

    let edits = lsp.request(
        "textDocument/formatting",
        json!({ "textDocument": { "uri": uri }, "options": { "tabSize": 4, "insertSpaces": true } }),
    );
    let formatted = edits[0]["newText"].as_str().expect("formatted text");
    assert!(formatted.contains("[Order Total]"), "{formatted}");
    assert!(formatted.contains("[dbo].[orders]"), "{formatted}");
    assert!(formatted.contains("@id"), "{formatted}");
    assert!(formatted.to_ascii_uppercase().contains("GO"), "{formatted}");
}

#[test]
fn a_tsql_language_id_reads_the_file_as_mssql() {
    let mut lsp = LspProcess::spawn();
    let uri = "file:///workspace/plain.sql";
    let text = "SELECT o. FROM #recent r JOIN [dbo].[orders] o ON o.id = r.id";
    lsp.open_tsql(uri, "tsql", text);

    let labels = lsp.completion_labels(uri, text, "SELECT o.");
    assert!(labels.contains(&"Order Total".to_string()), "{labels:?}");
}

/// Completion labels at the end of `text`, opened as a T-SQL file.
fn labels_at_end(text: &str) -> Vec<String> {
    let mut lsp = LspProcess::spawn();
    let uri = "file:///workspace/statement.sql";
    lsp.open_tsql(uri, "mssql", text);
    lsp.completion_labels(uri, text, text)
}

fn has_all(labels: &[String], expected: &[&str]) -> bool {
    expected
        .iter()
        .all(|label| labels.iter().any(|candidate| candidate == label))
}

const MERGE: &str = "MERGE INTO dbo.orders AS t USING dbo.customer AS s ON t.customer_id = s.id ";

#[test]
fn merge_completes_its_target_source_and_their_aliases() {
    for (text, expected) in [
        ("MERGE INTO ", &["dbo.customer", "dbo.orders"][..]),
        ("MERGE INTO dbo.orders AS t USING ", &["dbo.customer", "dbo.orders"]),
        ("MERGE INTO dbo.orders AS t USING dbo.customer AS s ON s.", &["id", "name"]),
        ("MERGE dbo.orders WITH (HOLDLOCK) AS t USING dbo.customer s ON t.", &["customer_id", "Order Total"]),
        (
            "MERGE [dbo].[orders] t USING (SELECT id, name FROM dbo.customer) AS s ON t.customer_id = s.",
            &["id", "name"],
        ),
    ] {
        let labels = labels_at_end(text);
        assert!(has_all(&labels, expected), "{text:?}: {labels:?}");
    }
}

#[test]
fn merge_offers_each_clause_in_turn() {
    for (tail, expected) in [
        (
            "",
            &[
                "WHEN MATCHED THEN",
                "WHEN NOT MATCHED THEN",
                "WHEN NOT MATCHED BY SOURCE THEN",
                "s.id",
                "t.customer_id",
            ][..],
        ),
        (
            "WHEN ",
            &["MATCHED", "NOT MATCHED", "NOT MATCHED BY SOURCE"],
        ),
        ("WHEN MATCHED ", &["THEN", "AND"]),
        ("WHEN MATCHED THEN ", &["UPDATE SET", "DELETE"]),
        (
            "WHEN MATCHED THEN UPDATE SET ",
            &["id", "customer_id", "Order Total"],
        ),
        (
            "WHEN MATCHED THEN UPDATE SET customer_id = ",
            &["s.id", "s.name"],
        ),
        ("WHEN NOT MATCHED THEN ", &["INSERT"]),
        (
            "WHEN NOT MATCHED THEN INSERT (",
            &["customer_id", "Order Total"],
        ),
        (
            "WHEN NOT MATCHED THEN INSERT (customer_id) VALUES (",
            &["s.id", "s.name", "NULL"],
        ),
        (
            "WHEN NOT MATCHED THEN INSERT (customer_id) VALUES (s.id) ",
            &["WHEN MATCHED THEN", "OUTPUT"],
        ),
        (
            "WHEN MATCHED THEN DELETE ",
            &["WHEN NOT MATCHED THEN", "OUTPUT"],
        ),
    ] {
        let text = format!("{MERGE}{tail}");
        let labels = labels_at_end(&text);
        assert!(has_all(&labels, expected), "{tail:?}: {labels:?}");
    }
}

#[test]
fn output_rows_are_the_statements_target() {
    for text in [
        "UPDATE dbo.orders SET customer_id = 1 OUTPUT inserted.",
        "UPDATE [dbo].[orders] SET customer_id = 1 OUTPUT deleted.",
        "DELETE FROM dbo.orders OUTPUT deleted.",
        "INSERT INTO dbo.orders (customer_id) OUTPUT inserted.",
        "DECLARE @n int
SET @n = 1
UPDATE dbo.orders SET customer_id = @n OUTPUT inserted.",
        "MERGE INTO dbo.orders AS t USING dbo.customer AS s ON t.customer_id = s.id WHEN MATCHED THEN DELETE OUTPUT deleted.",
    ] {
        let labels = labels_at_end(text);
        assert!(has_all(&labels, &["id", "customer_id", "Order Total"]), "{text:?}: {labels:?}");
        assert!(!labels.iter().any(|label| label == "name"), "{text:?}: {labels:?}");
    }

    let labels = labels_at_end("UPDATE dbo.orders SET customer_id = 1 OUTPUT ");
    assert!(has_all(&labels, &["inserted", "deleted"]), "{labels:?}");
    let labels = labels_at_end(&format!("{MERGE}WHEN MATCHED THEN DELETE OUTPUT "));
    assert!(labels.iter().any(|label| label == "$action"), "{labels:?}");
}

#[test]
fn statements_complete_without_semicolons_and_with_the_old_table_hint() {
    for (text, marker, expected) in [
        (
            "DECLARE @id int = 1
SELECT c. FROM dbo.customer c",
            "SELECT c.",
            &["id", "name"][..],
        ),
        (
            "SET NOCOUNT ON
SELECT o. FROM dbo.orders o",
            "SELECT o.",
            &["customer_id"],
        ),
        (
            "IF @x = 1
  SELECT o. FROM dbo.orders o",
            "SELECT o.",
            &["customer_id"],
        ),
        (
            "SELECT o. FROM dbo.orders o (NOLOCK)",
            "SELECT o.",
            &["customer_id"],
        ),
        (
            "SELECT x. FROM dbo.orders x WHERE x.[a\"b] = 1",
            "SELECT x.",
            &["customer_id"],
        ),
    ] {
        let mut lsp = LspProcess::spawn();
        let uri = "file:///workspace/statement.sql";
        lsp.open_tsql(uri, "mssql", text);
        let labels = lsp.completion_labels(uri, text, marker);
        assert!(has_all(&labels, expected), "{text:?}: {labels:?}");
    }
}

#[test]
fn valid_tsql_reports_no_syntax_errors_and_a_mistake_still_does() {
    for script in [
        "MERGE INTO dbo.orders AS t
USING dbo.customer AS s ON t.customer_id = s.id
WHEN MATCHED THEN UPDATE SET t.customer_id = s.id
WHEN NOT MATCHED BY TARGET THEN INSERT (customer_id) VALUES (s.id)
WHEN NOT MATCHED BY SOURCE THEN DELETE
OUTPUT $action, inserted.id, deleted.id;",
        "UPDATE dbo.orders SET customer_id = 1 OUTPUT inserted.id, deleted.customer_id INTO @log WHERE id = 5;",
        "INSERT INTO dbo.orders (customer_id) OUTPUT inserted.id VALUES (1);",
        "DECLARE @id int = 1;
SET @id = 2;
IF @id > 1
BEGIN
  PRINT 'big';
END
ELSE
  SELECT 1;",
        "EXEC dbo.do_it @id = 1;",
        "BEGIN TRAN;
DELETE FROM dbo.orders WHERE id = 1;
COMMIT TRANSACTION;",
        "SELECT TOP 5 * INTO #t FROM dbo.orders;
DROP TABLE #t;",
        "SELECT id FROM dbo.orders ORDER BY id OFFSET 0 ROWS FETCH NEXT 10 ROWS ONLY;",
        "CREATE TABLE #t (id int NOT NULL PRIMARY KEY, name nvarchar(max) NULL);",
        "SELECT * FROM dbo.orders o (NOLOCK);",
    ] {
        let mut lsp = LspProcess::spawn();
        let diagnostics = lsp.open_tsql("file:///workspace/script.sql", "mssql", script);
        assert!(syntax_errors(&diagnostics).is_empty(), "{script:?}: {diagnostics:?}");
    }

    let mut lsp = LspProcess::spawn();
    let diagnostics = lsp.open_tsql(
        "file:///workspace/mistake.sql",
        "mssql",
        "SELECT id FROM dbo.orders WHERE (id = 1;",
    );
    assert!(!syntax_errors(&diagnostics).is_empty(), "{diagnostics:?}");
}
