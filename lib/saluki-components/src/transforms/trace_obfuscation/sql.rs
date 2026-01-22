//! SQL query obfuscation.

use super::obfuscator::SqlObfuscationConfig;
use super::sql_filters::{DiscardFilter, GroupingFilter, MetadataFinderFilter, ReplaceFilter, TokenFilter};
use super::sql_tokenizer::{SQLTokenizer, TokenKind};

/// Result of SQL obfuscation.
#[derive(Debug, Clone, PartialEq)]
pub struct ObfuscatedSQL {
    /// The obfuscated SQL query
    pub query: String,
    /// Comma-separated list of table names (if enabled)
    pub table_names: String,
}

/// Obfuscates a SQL query by replacing literals with "?" and optionally extracting metadata.
pub fn obfuscate_sql_string(query: &str, config: &SqlObfuscationConfig) -> Result<ObfuscatedSQL, String> {
    let mut tokenizer = SQLTokenizer::new(query, true, config);

    let mut metadata_filter = MetadataFinderFilter::new(config);
    let mut discard_filter = DiscardFilter::new(config.keep_sql_alias());
    let mut replace_filter = ReplaceFilter::new(config);
    let mut grouping_filter = GroupingFilter::new();

    let mut output = Vec::new();
    let mut last_token = TokenKind::LexError;
    let mut last_buffer: Vec<u8> = Vec::new();
    let mut quoted_before_dot = false;

    loop {
        let (mut token, buffer) = tokenizer.scan();

        if token == TokenKind::LexError {
            if let Some(err) = tokenizer.err() {
                return Err(format!("Tokenization error: {}", err));
            }
            break;
        }

        if token == TokenKind::EndChar {
            break;
        }

        let mut buffer_vec = buffer.to_vec();

        match metadata_filter.filter(token, last_token, &buffer_vec, &last_buffer) {
            Ok((new_token, new_buffer)) => {
                token = new_token;
                if let Some(buf) = new_buffer {
                    buffer_vec = buf;
                }
            }
            Err(e) => return Err(e),
        }

        match discard_filter.filter(token, last_token, &buffer_vec, &last_buffer) {
            Ok((new_token, new_buffer)) => {
                token = new_token;
                if let Some(buf) = new_buffer {
                    buffer_vec = buf;
                } else {
                    last_token = token;
                    continue;
                }
            }
            Err(e) => return Err(e),
        }

        match replace_filter.filter(token, last_token, &buffer_vec, &last_buffer) {
            Ok((new_token, new_buffer)) => {
                token = new_token;
                if let Some(buf) = new_buffer {
                    buffer_vec = buf;
                }
            }
            Err(e) => return Err(e),
        }

        match grouping_filter.filter(token, last_token, &buffer_vec, &last_buffer) {
            Ok((new_token, new_buffer)) => {
                token = new_token;
                if let Some(buf) = new_buffer {
                    buffer_vec = buf;
                } else {
                    last_token = token;
                    continue;
                }
            }
            Err(e) => return Err(e),
        }

        if last_token == TokenKind::QuotedID && buffer_vec == b"." {
            quoted_before_dot = true;
        } else if buffer_vec != b"." && last_buffer != b"." {
            quoted_before_dot = false;
        }

        if !output.is_empty() && needs_space_before(token, &buffer_vec, last_token, &last_buffer, quoted_before_dot) {
            output.push(b' ');
        }

        output.extend_from_slice(&buffer_vec);

        last_token = token;
        last_buffer = buffer_vec;
    }

    let metadata = metadata_filter.results();

    Ok(ObfuscatedSQL {
        query: String::from_utf8_lossy(&output).to_string(),
        table_names: metadata.tables_csv,
    })
}

fn needs_space_before(
    token: TokenKind, buffer: &[u8], last_token: TokenKind, last_buffer: &[u8], quoted_before_dot: bool,
) -> bool {
    if buffer.is_empty() {
        return false;
    }

    if buffer == b"," {
        return false;
    }

    if buffer == b"." {
        return last_token == TokenKind::QuotedID;
    }

    if last_buffer == b"." {
        return token == TokenKind::QuotedID || quoted_before_dot;
    }

    if buffer == b"=" && last_buffer == b":" {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> SqlObfuscationConfig {
        SqlObfuscationConfig::default()
    }

    #[test]
    fn test_sql_quantizer() {
        let cases = vec![
            // Basic cases
            (
                "select * from users where id = 42",
                "select * from users where id = ?",
            ),
            (
                "select * from users where float = .43422",
                "select * from users where float = ?",
            ),
            (
                "SELECT host, status FROM ec2_status WHERE org_id = 42",
                "SELECT host, status FROM ec2_status WHERE org_id = ?",
            ),
            (
                "SELECT host, status FROM ec2_status WHERE org_id=42",
                "SELECT host, status FROM ec2_status WHERE org_id = ?",
            ),
            // Comments should be removed
            (
                "-- get user \n--\n select * \n   from users \n    where\n       id = 214325346",
                "select * from users where id = ?",
            ),
            (
                "select * from users where id = 214325346     # This comment continues to the end of line",
                "select * from users where id = ?",
            ),
            (
                "select * from users where id = 214325346     -- This comment continues to the end of line",
                "select * from users where id = ?",
            ),
            (
                "SELECT * FROM /* this is an in-line comment */ users;",
                "SELECT * FROM users",
            ),
            // IN clause grouping - multiple values should collapse to single ?
            (
                "SELECT * FROM `host` WHERE `id` IN (42, 43) /*comment with parameters,host:localhost,url:controller#home,id:FF005:00CAA*/",
                "SELECT * FROM host WHERE id IN ( ? )",
            ),
            (
                "SELECT articles.* FROM articles WHERE articles.id IN (1, 3, 5)",
                "SELECT articles.* FROM articles WHERE articles.id IN ( ? )",
            ),
            // Quoted identifiers should be unquoted
            (
                "SELECT `host`.`address` FROM `host` WHERE org_id=42",
                "SELECT host . address FROM host WHERE org_id = ?",
            ),
            (
                r#"SELECT "host"."address" FROM "host" WHERE org_id=42"#,
                "SELECT host . address FROM host WHERE org_id = ?",
            ),
            // Named parameters - should be obfuscated
            (
                "UPDATE user_dash_pref SET json_prefs = %(json_prefs)s, modified = '2015-08-27 22:10:32.492912' WHERE user_id = %(user_id)s AND url = %(url)s",
                "UPDATE user_dash_pref SET json_prefs = ? modified = ? WHERE user_id = ? AND url = ?",
            ),
            // Multiple values in IN clause should group
            (
                "SELECT DISTINCT host.id AS host_id FROM host JOIN host_alias ON host_alias.host_id = host.id WHERE host.org_id = %(org_id_1)s AND host.name NOT IN (%(name_1)s) AND host.name IN (%(name_2)s, %(name_3)s, %(name_4)s, %(name_5)s)",
                "SELECT DISTINCT host.id FROM host JOIN host_alias ON host_alias.host_id = host.id WHERE host.org_id = ? AND host.name NOT IN ( ? ) AND host.name IN ( ? )",
            ),
            // Array literals should group
            (
                "SELECT org_id, metric_key FROM metrics_metadata WHERE org_id = %(org_id)s AND metric_key = ANY(array[75])",
                "SELECT org_id, metric_key FROM metrics_metadata WHERE org_id = ? AND metric_key = ANY ( array [ ? ] )",
            ),
            (
                "SELECT org_id, metric_key   FROM metrics_metadata   WHERE org_id = %(org_id)s AND metric_key = ANY(array[21, 25, 32])",
                "SELECT org_id, metric_key FROM metrics_metadata WHERE org_id = ? AND metric_key = ANY ( array [ ? ] )",
            ),
            // LIMIT should be obfuscated
            (
                "SELECT articles.* FROM articles WHERE articles.id = 1 LIMIT 1",
                "SELECT articles.* FROM articles WHERE articles.id = ? LIMIT ?",
            ),
            (
                "SELECT articles.* FROM articles WHERE articles.id = 1 LIMIT 1, 20",
                "SELECT articles.* FROM articles WHERE articles.id = ? LIMIT ?",
            ),
            // BETWEEN
            (
                "SELECT articles.* FROM articles WHERE (articles.created_at BETWEEN '2016-10-31 23:00:00.000000' AND '2016-11-01 23:00:00.000000')",
                "SELECT articles.* FROM articles WHERE ( articles.created_at BETWEEN ? AND ? )",
            ),
            // Positional parameters should be converted to ?
            (
                "SELECT articles.* FROM articles WHERE (articles.created_at BETWEEN $1 AND $2)",
                "SELECT articles.* FROM articles WHERE ( articles.created_at BETWEEN ? AND ? )",
            ),
            // Boolean literals
            (
                "SELECT articles.* FROM articles WHERE (articles.published != true)",
                "SELECT articles.* FROM articles WHERE ( articles.published != ? )",
            ),
            // Already obfuscated queries should remain unchanged
            (
                "SELECT articles.* FROM articles WHERE ( title = ? ) AND ( author = ? )",
                "SELECT articles.* FROM articles WHERE ( title = ? ) AND ( author = ? )",
            ),
            // Named placeholders should NOT be obfuscated
            (
                "SELECT articles.* FROM articles WHERE ( title = :title )",
                "SELECT articles.* FROM articles WHERE ( title = :title )",
            ),
            (
                "SELECT articles.* FROM articles WHERE ( title = @title )",
                "SELECT articles.* FROM articles WHERE ( title = @title )",
            ),
            // AS alias should be removed by default
            (
                "SELECT date(created_at) as ordered_date, sum(price) as total_price FROM orders GROUP BY date(created_at) HAVING sum(price) > 100",
                "SELECT date ( created_at ), sum ( price ) FROM orders GROUP BY date ( created_at ) HAVING sum ( price ) > ?",
            ),
            (
                "SELECT * FROM articles WHERE id > 10 ORDER BY id asc LIMIT 20",
                "SELECT * FROM articles WHERE id > ? ORDER BY id asc LIMIT ?",
            ),
            // String literals in joins
            (
                "SELECT clients.* FROM clients INNER JOIN posts ON posts.author_id = author.id AND posts.published = 't'",
                "SELECT clients.* FROM clients INNER JOIN posts ON posts.author_id = author.id AND posts.published = ?",
            ),
            // Multiple statements - BEGIN/COMMIT
            (
                "SELECT * FROM clients WHERE (clients.first_name = 'Andy') LIMIT 1 BEGIN INSERT INTO clients (created_at, first_name, locked, orders_count, updated_at) VALUES ('2011-08-30 05:22:57', 'Andy', 1, NULL, '2011-08-30 05:22:57') COMMIT",
                "SELECT * FROM clients WHERE ( clients.first_name = ? ) LIMIT ? BEGIN INSERT INTO clients ( created_at, first_name, locked, orders_count, updated_at ) VALUES ( ? ) COMMIT",
            ),
            // SAVEPOINT
            (
                r#"SAVEPOINT "s139956586256192_x1""#,
                "SAVEPOINT ?",
            ),
            // Multiple VALUES should collapse to single group
            // TODO: Fix multi-VALUES grouping
            // (
            //     "INSERT INTO user (id, username) VALUES ('Fred','Smith'), ('John','Smith'), ('Michael','Smith'), ('Robert','Smith');",
            //     "INSERT INTO user ( id, username ) VALUES ( ? )",
            // ),
            // JSON/dict literals
            (
                "CREATE KEYSPACE Excelsior WITH replication = {'class': 'SimpleStrategy', 'replication_factor' : 3};",
                "CREATE KEYSPACE Excelsior WITH replication = ?",
            ),
            // %s placeholders
            (
                r#"SELECT "webcore_page"."id" FROM "webcore_page" WHERE "webcore_page"."slug" = %s ORDER BY "webcore_page"."path" ASC LIMIT 1"#,
                "SELECT webcore_page . id FROM webcore_page WHERE webcore_page . slug = ? ORDER BY webcore_page . path ASC LIMIT ?",
            ),
            // AS alias removal
            (
                "SELECT server_table.host AS host_id FROM table#.host_tags as server_table WHERE server_table.host_id = 50",
                "SELECT server_table.host FROM table#.host_tags WHERE server_table.host_id = ?",
            ),
            // Function calls with multiple args should group
            (
                "SELECT name, pretty_print(address) FROM people;",
                "SELECT name, pretty_print ( address ) FROM people",
            ),
            (
                "* SELECT * FROM fake_data(1, 2, 3);",
                "* SELECT * FROM fake_data ( ? )",
            ),
            // NULL values in INSERT should group
            (
                "INSERT INTO `qual-aa`.issues (alert0 , alert1) VALUES (NULL, NULL)",
                "INSERT INTO qual-aa . issues ( alert0, alert1 ) VALUES ( ? )",
            ),
            (
                "INSERT INTO user (id, email, name) VALUES (null, ?, ?)",
                "INSERT INTO user ( id, email, name ) VALUES ( ? )",
            ),
            // Variables (@var) should NOT be obfuscated
            (
                "SET @g = 'POLYGON((0 0,10 0,10 10,0 10,0 0),(5 5,7 5,7 7,5 7, 5 5))';",
                "SET @g = ?",
            ),
            // Empty strings
            (
                "SELECT * FROM users WHERE firstname=''",
                "SELECT * FROM users WHERE firstname = ?",
            ),
            (
                "SELECT * FROM users WHERE firstname=' '",
                "SELECT * FROM users WHERE firstname = ?",
            ),
            (
                r#"SELECT * FROM users WHERE firstname="""#,
                "SELECT * FROM users WHERE firstname = ?",
            ),
            // Escaped quotes
            (
                r#"SELECT * FROM foo LEFT JOIN bar ON 'backslash\' = foo.b WHERE foo.name = 'String'"#,
                "SELECT * FROM foo LEFT JOIN bar ON ? = foo.b WHERE foo.name = ?",
            ),
            (
                r#"SELECT * FROM foo LEFT JOIN bar ON 'embedded ''quote'' in string' = foo.b WHERE foo.name = 'String'"#,
                "SELECT * FROM foo LEFT JOIN bar ON ? = foo.b WHERE foo.name = ?",
            ),
            // ARRAY with multiple values should group
            (
                "SELECT org_id,metric_key,metric_type,interval FROM metrics_metadata WHERE org_id = ? AND metric_key = ANY(ARRAY[?,?,?,?,?])",
                "SELECT org_id, metric_key, metric_type, interval FROM metrics_metadata WHERE org_id = ? AND metric_key = ANY ( ARRAY [ ? ] )",
            ),
            // Type casts
            (
                "SELECT a :: VARCHAR(255) FROM foo WHERE foo.name = 'String'",
                "SELECT a :: VARCHAR ( ? ) FROM foo WHERE foo.name = ?",
            ),
        ];

        let config = default_config();
        for (i, (input, expected)) in cases.iter().enumerate() {
            let result = obfuscate_sql_string(input, &config);
            match result {
                Ok(obfuscated) => {
                    assert_eq!(
                        &obfuscated.query, expected,
                        "Test case {} failed:\nInput: {}\nExpected: {}\nGot: {}",
                        i, input, expected, obfuscated.query
                    );
                }
                Err(e) => {
                    panic!("Test case {} failed to parse:\nInput: {}\nError: {}", i, input, e);
                }
            }
        }
    }

    #[test]
    fn test_keep_sql_alias() {
        let query = "SELECT username AS person FROM users WHERE id=4";

        // Test with keep_sql_alias = false (default)
        let config_off = SqlObfuscationConfig {
            keep_sql_alias: false,
            ..default_config()
        };
        let result = obfuscate_sql_string(query, &config_off).unwrap();
        assert_eq!(result.query, "SELECT username FROM users WHERE id = ?");

        // Test with keep_sql_alias = true
        let config_on = SqlObfuscationConfig {
            keep_sql_alias: true,
            ..default_config()
        };
        let result = obfuscate_sql_string(query, &config_on).unwrap();
        assert_eq!(result.query, "SELECT username AS person FROM users WHERE id = ?");
    }

    #[test]
    fn test_can_obfuscate_autovacuum() {
        let config = default_config();
        let cases = vec![
            (
                "autovacuum: VACUUM ANALYZE fake.table",
                "autovacuum : VACUUM ANALYZE fake.table",
            ),
            (
                "autovacuum: VACUUM ANALYZE fake.table_downtime",
                "autovacuum : VACUUM ANALYZE fake.table_downtime",
            ),
            (
                "autovacuum: VACUUM fake.big_table (to prevent wraparound)",
                "autovacuum : VACUUM fake.big_table ( to prevent wraparound )",
            ),
        ];

        for (input, expected) in cases {
            let result = obfuscate_sql_string(input, &config).unwrap();
            assert_eq!(result.query, expected);
        }
    }

    #[test]
    fn test_dollar_quoted_func() {
        let query = "SELECT $func$INSERT INTO table VALUES ('a', 1, 2)$func$ FROM users";

        // Test with dollar_quoted_func = false (default)
        let config_off = SqlObfuscationConfig {
            dollar_quoted_func: false,
            ..default_config()
        };
        let result = obfuscate_sql_string(query, &config_off).unwrap();
        assert_eq!(result.query, "SELECT ? FROM users");

        // Test with dollar_quoted_func = true
        let config_on = SqlObfuscationConfig {
            dollar_quoted_func: true,
            ..default_config()
        };
        let result = obfuscate_sql_string(query, &config_on).unwrap();
        assert_eq!(
            result.query,
            "SELECT $func$INSERT INTO table VALUES ( ? )$func$ FROM users"
        );
    }

    #[test]
    fn test_sql_replace_digits() {
        let config = SqlObfuscationConfig {
            replace_digits: true,
            ..default_config()
        };

        let cases = vec![
            (
                "REPLACE INTO sales_2019_07_01 (`itemID`, `date`, `qty`, `price`) VALUES ((SELECT itemID FROM item1001 WHERE `sku` = [sku]), CURDATE(), [qty], 0.00)",
                "REPLACE INTO sales_?_?_? ( itemID, date, qty, price ) VALUES ( ( SELECT itemID FROM item? WHERE sku = [ sku ] ), CURDATE ( ), [ qty ], ? )",
            ),
            (
                "SELECT ddh19.name, ddt.tags FROM dd91219.host ddh19, dd21916.host_tags ddt WHERE ddh19.id = ddt.host_id AND ddh19.org_id = 2 AND ddh19.name = 'datadog'",
                "SELECT ddh?.name, ddt.tags FROM dd?.host ddh?, dd?.host_tags ddt WHERE ddh?.id = ddt.host_id AND ddh?.org_id = ? AND ddh?.name = ?",
            ),
            (
                "SELECT ddu2.name, ddo.id10, ddk.app_key52 FROM dd3120.user ddu2, dd1931.orgs55 ddo, dd53819.keys ddk",
                "SELECT ddu?.name, ddo.id?, ddk.app_key? FROM dd?.user ddu?, dd?.orgs? ddo, dd?.keys ddk",
            ),
        ];

        for (query, expected) in cases {
            let result = obfuscate_sql_string(query, &config).unwrap();
            assert_eq!(result.query, expected, "Failed for query: {}", query);
        }
    }

    #[test]
    #[ignore] // TODO: $action is obfuscated to ? - tokenizer needs to recognize $identifier as SQL Server variable
    fn test_single_dollar_identifier() {
        let query = r#"
	MERGE INTO Employees AS target
	USING EmployeeUpdates AS source
	ON (target.EmployeeID = source.EmployeeID)
	WHEN MATCHED THEN
		UPDATE SET
			target.Name = source.Name
	WHEN NOT MATCHED BY TARGET THEN
		INSERT (EmployeeID, Name)
		VALUES (source.EmployeeID, source.Name)
	WHEN NOT MATCHED BY SOURCE THEN
		DELETE
	OUTPUT $action, inserted.*, deleted.*;
	"#;

        let config = SqlObfuscationConfig {
            dbms: "sqlserver".to_string(),
            ..default_config()
        };

        let result = obfuscate_sql_string(query, &config).unwrap();
        assert_eq!(
            result.query,
            "MERGE INTO Employees USING EmployeeUpdates ON ( target.EmployeeID = source.EmployeeID ) WHEN MATCHED THEN UPDATE SET target.Name = source.Name WHEN NOT MATCHED BY TARGET THEN INSERT ( EmployeeID, Name ) VALUES ( source.EmployeeID, source.Name ) WHEN NOT MATCHED BY SOURCE THEN DELETE OUTPUT $action, inserted.*, deleted.*"
        );
    }

    #[test]
    fn test_pg_json_operators() {
        let config = SqlObfuscationConfig {
            dbms: "postgres".to_string(),
            ..default_config()
        };

        let cases = vec![
            (
                "select users.custom #> '{a,b}' from users",
                "select users.custom #> ? from users",
            ),
            (
                "select users.custom #>> '{a,b}' from users",
                "select users.custom #>> ? from users",
            ),
            (
                "select users.custom #- '{a,b}' from users",
                "select users.custom #- ? from users",
            ),
            (
                "select users.custom -> 'foo' from users",
                "select users.custom -> ? from users",
            ),
            (
                "select users.custom ->> 'foo' from users",
                "select users.custom ->> ? from users",
            ),
            (
                "select * from users where user.custom @> '{a,b}'",
                "select * from users where user.custom @> ?",
            ),
            (
                "SELECT a FROM foo WHERE value<@name",
                "SELECT a FROM foo WHERE value <@ name",
            ),
        ];

        for (input, expected) in cases {
            let result = obfuscate_sql_string(input, &config).unwrap();
            assert_eq!(result.query, expected, "Failed for query: {}", input);
        }
    }

    #[test]
    fn test_multiple_process() {
        let config = default_config();

        let cases = vec![
            (
                "SELECT clients.* FROM clients INNER JOIN posts ON posts.author_id = author.id AND posts.published = 't'",
                "SELECT clients.* FROM clients INNER JOIN posts ON posts.author_id = author.id AND posts.published = ?",
            ),
            (
                "SELECT articles.* FROM articles WHERE articles.id IN (1, 3, 5)",
                "SELECT articles.* FROM articles WHERE articles.id IN ( ? )",
            ),
            (
                r#"SELECT id FROM jq_jobs
WHERE
schedulable_at <= 1555367948 AND
queue_name = 'order_jobs' AND
status = 1 AND
id % 8 = 3
ORDER BY
schedulable_at
LIMIT 1000"#,
                "SELECT id FROM jq_jobs WHERE schedulable_at <= ? AND queue_name = ? AND status = ? AND id % ? = ? ORDER BY schedulable_at LIMIT ?",
            ),
        ];

        for (query, expected) in cases {
            let result = obfuscate_sql_string(query, &config).unwrap();
            assert_eq!(result.query, expected, "Failed for query: {}", query);
        }
    }
}
