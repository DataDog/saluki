//! Redis command obfuscation.

use super::obfuscator::{RedisObfuscationConfig, ValkeyObfuscationConfig};

const REDIS_COMPOUND_COMMANDS: &[&str] = &["CLIENT", "CLUSTER", "COMMAND", "CONFIG", "DEBUG", "SCRIPT"];
const REDIS_TRUNCATION_MARK: &str = "...";
const MAX_REDIS_NB_COMMANDS: usize = 3;

/// Quantizes a Redis command string by extracting just the command names.
pub fn quantize_redis_string(query: &str) -> String {
    let query = compact_whitespaces(query);
    let mut resource = String::new();
    let mut truncated = false;
    let mut nb_cmds = 0;

    let mut remaining = query.as_str();
    while !remaining.is_empty() && nb_cmds < MAX_REDIS_NB_COMMANDS {
        let (raw_line, rest) = match remaining.split_once('\n') {
            Some((line, rest)) => (line, rest),
            None => (remaining, ""),
        };
        remaining = rest;

        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(3, ' ').collect();

        if parts[0].ends_with(REDIS_TRUNCATION_MARK) {
            truncated = true;
            continue;
        }

        let mut command = parts[0].to_uppercase();

        if REDIS_COMPOUND_COMMANDS.contains(&command.as_str()) && parts.len() > 1 {
            if parts[1].ends_with(REDIS_TRUNCATION_MARK) {
                truncated = true;
                continue;
            }
            command.push(' ');
            command.push_str(&parts[1].to_uppercase());
        }

        if !resource.is_empty() {
            resource.push(' ');
        }
        resource.push_str(&command);

        nb_cmds += 1;
        truncated = false;
    }

    if (nb_cmds == MAX_REDIS_NB_COMMANDS && !remaining.is_empty()) || truncated {
        if !resource.is_empty() {
            resource.push(' ');
        }
        resource.push_str("...");
    }

    resource
}

/// Obfuscates a Redis command string by removing sensitive arguments.
pub fn obfuscate_redis_string(rediscmd: &str, config: &RedisObfuscationConfig) -> String {
    if config.remove_all_args() {
        return rediscmd
            .lines()
            .map(remove_all_redis_args)
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut tokenizer = RedisTokenizer::new(rediscmd.as_bytes());
    let mut result = String::new();
    let mut cmd = String::new();
    let mut args = Vec::new();

    loop {
        let (tok, typ, done) = tokenizer.scan();

        match typ {
            TokenType::Command => {
                if !cmd.is_empty() {
                    obfuscate_redis_cmd(&mut result, &cmd, &args);
                    let trimmed = result.trim_end().to_string();
                    result.clear();
                    result.push_str(&trimmed);
                    result.push('\n');
                }
                cmd = tok;
                args.clear();
            }
            TokenType::Argument => {
                if !tok.is_empty() {
                    args.push(tok);
                }
            }
        }

        if done {
            obfuscate_redis_cmd(&mut result, &cmd, &args);
            break;
        }
    }

    result.trim_end().to_string()
}

/// Obfuscates a Valkey command string by removing sensitive arguments.
pub fn obfuscate_valkey_string(valkeycmd: &str, config: &ValkeyObfuscationConfig) -> String {
    if config.remove_all_args() {
        return valkeycmd
            .lines()
            .map(remove_all_redis_args)
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut tokenizer = RedisTokenizer::new(valkeycmd.as_bytes());
    let mut result = String::new();
    let mut cmd = String::new();
    let mut args = Vec::new();

    loop {
        let (tok, typ, done) = tokenizer.scan();

        match typ {
            TokenType::Command => {
                if !cmd.is_empty() {
                    obfuscate_redis_cmd(&mut result, &cmd, &args);
                    let trimmed = result.trim_end().to_string();
                    result.clear();
                    result.push_str(&trimmed);
                    result.push('\n');
                }
                cmd = tok;
                args.clear();
            }
            TokenType::Argument => {
                if !tok.is_empty() {
                    args.push(tok);
                }
            }
        }

        if done {
            obfuscate_redis_cmd(&mut result, &cmd, &args);
            break;
        }
    }

    result.trim_end().to_string()
}

/// Removes all arguments from a Redis command.
pub fn remove_all_redis_args(rediscmd: &str) -> String {
    let parts: Vec<&str> = rediscmd.split_whitespace().collect();
    if parts.is_empty() {
        return String::new();
    }

    let cmd = parts[0];
    let args = &parts[1..];

    let mut result = String::from(cmd);
    if args.is_empty() {
        return result;
    }

    result.push(' ');

    match cmd.to_uppercase().as_str() {
        "BITFIELD" => {
            result.push('?');
            for arg in args {
                let arg_upper = arg.to_uppercase();
                if arg_upper == "SET" || arg_upper == "GET" || arg_upper == "INCRBY" {
                    result.push_str(&format!(" {} ?", arg));
                }
            }
        }
        "CONFIG" => {
            let arg_upper = args[0].to_uppercase();
            if arg_upper == "GET" || arg_upper == "SET" || arg_upper == "RESETSTAT" || arg_upper == "REWRITE" {
                result.push_str(&format!("{} ?", args[0]));
            } else {
                result.push('?');
            }
        }
        _ => result.push('?'),
    }

    result
}

fn obfuscate_redis_cmd(out: &mut String, cmd: &str, args: &[String]) {
    out.push_str(cmd);
    if args.is_empty() {
        return;
    }

    out.push(' ');

    let mut args = args.to_vec();
    let cmd_upper = cmd.to_uppercase();

    match cmd_upper.as_str() {
        "AUTH" => {
            if !args.is_empty() {
                args[0] = "?".to_string();
                args.truncate(1);
            }
        }

        "APPEND" | "GETSET" | "LPUSHX" | "GEORADIUSBYMEMBER" | "RPUSHX" | "SET" | "SETNX" | "SISMEMBER" | "ZRANK"
        | "ZREVRANK" | "ZSCORE" => {
            obfuscate_arg_n(&mut args, 1);
        }

        "HSETNX" | "LREM" | "LSET" | "SETBIT" | "SETEX" | "PSETEX" | "SETRANGE" | "ZINCRBY" | "SMOVE" | "RESTORE" => {
            obfuscate_arg_n(&mut args, 2);
        }

        "LINSERT" => {
            obfuscate_arg_n(&mut args, 3);
        }

        "GEOHASH" | "GEOPOS" | "GEODIST" | "LPUSH" | "RPUSH" | "SREM" | "ZREM" | "SADD" => {
            if args.len() > 1 {
                args[1] = "?".to_string();
                args.truncate(2);
            }
        }

        "GEOADD" => {
            obfuscate_args_step(&mut args, 1, 3);
        }

        "HSET" | "HMSET" => {
            obfuscate_args_step(&mut args, 1, 2);
        }

        "MSET" | "MSETNX" => {
            obfuscate_args_step(&mut args, 0, 2);
        }

        "CONFIG" => {
            if !args.is_empty() && args[0].to_uppercase() == "SET" {
                obfuscate_arg_n(&mut args, 2);
            }
        }

        "BITFIELD" => {
            let mut set_idx = None;
            for (i, arg) in args.iter().enumerate() {
                if arg.to_uppercase() == "SET" {
                    set_idx = Some(i);
                }
                if let Some(n) = set_idx {
                    if i == n + 3 {
                        args[i] = "?".to_string();
                        break;
                    }
                }
            }
        }

        "ZADD" => {
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "NX" | "XX" | "CH" | "INCR" => i += 1,
                    _ => break,
                }
            }
            obfuscate_args_step(&mut args, i, 2);
        }

        _ => {}
    }

    out.push_str(&args.join(" "));
}

fn obfuscate_arg_n(args: &mut [String], n: usize) {
    if args.len() > n {
        args[n] = "?".to_string();
    }
}

fn obfuscate_args_step(args: &mut [String], start: usize, step: usize) {
    if start + step < 1 || start + step > args.len() {
        return;
    }
    let mut i = start + step - 1;
    while i < args.len() {
        args[i] = "?".to_string();
        i += step;
    }
}

fn compact_whitespaces(s: &str) -> String {
    s.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, PartialEq)]
enum TokenType {
    Command,
    Argument,
}

#[derive(Debug, PartialEq)]
enum State {
    Command,
    Argument,
}

struct RedisTokenizer<'a> {
    data: &'a [u8],
    off: usize,
    done: bool,
    state: State,
}

impl<'a> RedisTokenizer<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            off: 0,
            done: data.is_empty(),
            state: State::Command,
        }
    }

    fn scan(&mut self) -> (String, TokenType, bool) {
        match self.state {
            State::Command => self.scan_command(),
            State::Argument => self.scan_arg(),
        }
    }

    fn scan_command(&mut self) -> (String, TokenType, bool) {
        let mut result = String::new();

        while self.off < self.data.len() {
            let ch = self.data[self.off];
            self.off += 1;

            match ch {
                b' ' => {
                    if !result.is_empty() {
                        self.skip_space();
                        self.state = State::Argument;
                        return (result, TokenType::Command, self.done);
                    }
                    self.skip_space();
                }
                b'\n' => {
                    self.state = State::Command;
                    return (result, TokenType::Command, self.off >= self.data.len());
                }
                _ => {
                    result.push(ch as char);
                }
            }
        }

        self.done = true;
        (result, TokenType::Command, true)
    }

    fn scan_arg(&mut self) -> (String, TokenType, bool) {
        let mut result = String::new();
        let mut quoted = false;
        let mut escape = false;

        while self.off < self.data.len() {
            let ch = self.data[self.off];
            self.off += 1;

            match ch {
                b'\\' => {
                    result.push('\\');
                    if !escape {
                        escape = true;
                        continue;
                    }
                    escape = false;
                }
                b'"' => {
                    result.push('"');
                    if escape {
                        escape = false;
                        continue;
                    }
                    quoted = !quoted;
                }
                b' ' => {
                    if quoted {
                        result.push(' ');
                        continue;
                    }
                    if !result.is_empty() {
                        self.skip_space();
                        return (result, TokenType::Argument, self.done);
                    }
                    self.skip_space();
                }
                b'\n' => {
                    if quoted {
                        result.push('\n');
                        continue;
                    }
                    self.state = State::Command;
                    return (result, TokenType::Argument, self.off >= self.data.len());
                }
                _ => {
                    result.push(ch as char);
                    escape = false;
                }
            }
        }

        self.done = true;
        (result, TokenType::Argument, true)
    }

    fn skip_space(&mut self) {
        while self.off < self.data.len() && self.data[self.off] == b' ' {
            self.off += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redis_quantizer() {
        let cases = vec![
            // Regression test for DataDog/datadog-trace-agent#421
            ("CLIENT", "CLIENT"),
            ("CLIENT LIST", "CLIENT LIST"),
            ("get my_key", "GET"),
            ("SET le_key le_value", "SET"),
            ("\n\n  \nSET foo bar  \n  \n\n  ", "SET"),
            ("CONFIG SET parameter value", "CONFIG SET"),
            ("SET toto tata \n \n  EXPIRE toto 15  ", "SET EXPIRE"),
            ("MSET toto tata toto tata toto tata \n ", "MSET"),
            (
                "MULTI\nSET k1 v1\nSET k2 v2\nSET k3 v3\nSET k4 v4\nDEL to_del\nEXEC",
                "MULTI SET SET ...",
            ),
            (
                "DEL k1\nDEL k2\nHMSET k1 \"a\" 1 \"b\" 2 \"c\" 3\nHMSET k2 \"d\" \"4\" \"e\" \"4\"\nDEL k3\nHMSET k3 \"f\" \"5\"\nDEL k1\nDEL k2\nHMSET k1 \"a\" 1 \"b\" 2 \"c\" 3\nHMSET k2 \"d\" \"4\" \"e\" \"4\"\nDEL k3\nHMSET k3 \"f\" \"5\"\nDEL k1\nDEL k2\nHMSET k1 \"a\" 1 \"b\" 2 \"c\" 3\nHMSET k2 \"d\" \"4\" \"e\" \"4\"\nDEL k3\nHMSET k3 \"f\" \"5\"\nDEL k1\nDEL k2\nHMSET k1 \"a\" 1 \"b\" 2 \"c\" 3\nHMSET k2 \"d\" \"4\" \"e\" \"4\"\nDEL k3\nHMSET k3 \"f\" \"5\"",
                "DEL DEL HMSET ...",
            ),
            ("GET...", "..."),
            ("GET k...", "GET"),
            ("GET k1\nGET k2\nG...", "GET GET ..."),
            ("GET k1\nGET k2\nDEL k3\nGET k...", "GET GET DEL ..."),
            ("GET k1\nGET k2\nHDEL k3 a\nG...", "GET GET HDEL ..."),
            ("GET k...\nDEL k2\nMS...", "GET DEL ..."),
            ("GET k...\nDE...\nMS...", "GET ..."),
            ("GET k1\nDE...\nGET k2", "GET GET"),
            ("GET k1\nDE...\nGET k2\nHDEL k3 a\nGET k4\nDEL k5", "GET GET HDEL ..."),
            ("UNKNOWN 123", "UNKNOWN"),
        ];

        for (query, expected) in cases {
            let result = quantize_redis_string(query);
            assert_eq!(result, expected, "Failed for query: {}", query);
        }
    }

    #[test]
    fn test_redis_obfuscator() {
        let config = RedisObfuscationConfig {
            enabled: true,
            remove_all_args: false,
        };

        let cases = vec![
            ("AUTH my-secret-password", "AUTH ?"),
            ("AUTH james my-secret-password", "AUTH ?"),
            ("AUTH", "AUTH"),
            ("APPEND key value", "APPEND key ?"),
            ("GETSET key value", "GETSET key ?"),
            ("LPUSHX key value", "LPUSHX key ?"),
            ("GEORADIUSBYMEMBER key member radius m|km|ft|mi [WITHCOORD] [WITHDIST] [WITHHASH] [COUNT count] [ASC|DESC] [STORE key] [STOREDIST key]",
             "GEORADIUSBYMEMBER key ? radius m|km|ft|mi [WITHCOORD] [WITHDIST] [WITHHASH] [COUNT count] [ASC|DESC] [STORE key] [STOREDIST key]"),
            ("RPUSHX key value", "RPUSHX key ?"),
            ("SET key value", "SET key ?"),
            ("SET key value [expiration EX seconds|PX milliseconds] [NX|XX]",
             "SET key ? [expiration EX seconds|PX milliseconds] [NX|XX]"),
            ("SETNX key value", "SETNX key ?"),
            ("SISMEMBER key member", "SISMEMBER key ?"),
            ("ZRANK key member", "ZRANK key ?"),
            ("ZREVRANK key member", "ZREVRANK key ?"),
            ("ZSCORE key member", "ZSCORE key ?"),
            ("BITFIELD key GET type offset SET type offset value INCRBY type",
             "BITFIELD key GET type offset SET type offset ? INCRBY type"),
            ("BITFIELD key SET type offset value INCRBY type",
             "BITFIELD key SET type offset ? INCRBY type"),
            ("BITFIELD key GET type offset INCRBY type",
             "BITFIELD key GET type offset INCRBY type"),
            ("BITFIELD key SET type offset", "BITFIELD key SET type offset"),
            ("CONFIG SET parameter value", "CONFIG SET parameter ?"),
            ("CONFIG foo bar baz", "CONFIG foo bar baz"),
            ("GEOADD key longitude latitude member longitude latitude member longitude latitude member",
             "GEOADD key longitude latitude ? longitude latitude ? longitude latitude ?"),
            ("GEOADD key longitude latitude member longitude latitude member",
             "GEOADD key longitude latitude ? longitude latitude ?"),
            ("GEOADD key longitude latitude member", "GEOADD key longitude latitude ?"),
            ("GEOADD key longitude latitude", "GEOADD key longitude latitude"),
            ("GEOADD key", "GEOADD key"),
            ("GEOHASH key\nGEOPOS key\n GEODIST key", "GEOHASH key\nGEOPOS key\nGEODIST key"),
            ("GEOHASH key member\nGEOPOS key member\nGEODIST key member\n",
             "GEOHASH key ?\nGEOPOS key ?\nGEODIST key ?"),
            ("GEOHASH key member member member\nGEOPOS key member member \n  GEODIST key member member member",
             "GEOHASH key ?\nGEOPOS key ?\nGEODIST key ?"),
            ("GEOPOS key member [member ...]", "GEOPOS key ?"),
            ("SREM key member [member ...]", "SREM key ?"),
            ("ZREM key member [member ...]", "ZREM key ?"),
            ("SADD key member [member ...]", "SADD key ?"),
            ("GEODIST key member1 member2 [unit]", "GEODIST key ?"),
            ("LPUSH key value [value ...]", "LPUSH key ?"),
            ("RPUSH key value [value ...]", "RPUSH key ?"),
            ("HSET key field value \nHSETNX key field value\nBLAH",
             "HSET key field ?\nHSETNX key field ?\nBLAH"),
            ("HSET key field value", "HSET key field ?"),
            ("HSET key field1 value field2 value", "HSET key field1 ? field2 ?"),
            ("HSETNX key field value", "HSETNX key field ?"),
            ("LREM key count value", "LREM key count ?"),
            ("LSET key index value", "LSET key index ?"),
            ("SETBIT key offset value", "SETBIT key offset ?"),
            ("SETRANGE key offset value", "SETRANGE key offset ?"),
            ("SETEX key seconds value", "SETEX key seconds ?"),
            ("PSETEX key milliseconds value", "PSETEX key milliseconds ?"),
            ("ZINCRBY key increment member", "ZINCRBY key increment ?"),
            ("SMOVE source destination member", "SMOVE source destination ?"),
            ("RESTORE key ttl serialized-value [REPLACE]", "RESTORE key ttl ? [REPLACE]"),
            ("LINSERT key BEFORE pivot value", "LINSERT key BEFORE pivot ?"),
            ("LINSERT key AFTER pivot value", "LINSERT key AFTER pivot ?"),
            ("HMSET key field value field value", "HMSET key field ? field ?"),
            ("HMSET key field value \n HMSET key field value\n\n ",
             "HMSET key field ?\nHMSET key field ?"),
            ("HMSET key field", "HMSET key field"),
            ("MSET key value key value", "MSET key ? key ?"),
            ("MSET\nMSET key value", "MSET\nMSET key ?"),
            ("MSET key value", "MSET key ?"),
            ("MSETNX key value key value", "MSETNX key ? key ?"),
            ("ZADD key score member score member", "ZADD key score ? score ?"),
            ("ZADD key NX score member score member", "ZADD key NX score ? score ?"),
            ("ZADD key NX CH score member score member", "ZADD key NX CH score ? score ?"),
            ("ZADD key NX CH INCR score member score member",
             "ZADD key NX CH INCR score ? score ?"),
            ("ZADD key XX INCR score member score member",
             "ZADD key XX INCR score ? score ?"),
            ("ZADD key XX INCR score member", "ZADD key XX INCR score ?"),
            ("ZADD key XX INCR score", "ZADD key XX INCR score"),
            ("\nCONFIG command\nSET k v\n\t\t\t", "CONFIG command\nSET k ?"),
        ];

        for (input, expected) in cases {
            let result = obfuscate_redis_string(input, &config);
            assert_eq!(result, expected, "Failed for input: {}", input);
        }
    }

    #[test]
    fn test_remove_all_redis_args() {
        let config = RedisObfuscationConfig {
            enabled: true,
            remove_all_args: true,
        };

        let cases = vec![
            ("SET key value", "SET ?"),
            ("SET key value NX", "SET ?"),
            ("SET key value EX 1000", "SET ?"),
            ("SET key value PX 1000000", "SET ?"),
            ("SET key value EXAT 1000", "SET ?"),
            ("SET key value PXAT 1000000", "SET ?"),
            ("SET key value KEEPTTL", "SET ?"),
            ("SET key value GET", "SET ?"),
            ("GET k", "GET ?"),
            ("MGET k1 k2 k3", "MGET ?"),
            ("MSET k1 v1 k2 v2 k3 v3", "MSET ?"),
            ("LPUSH queue job1", "LPUSH ?"),
            ("HMSET hash field1 value1 field2 value2", "HMSET ?"),
            ("ZADD z 1 one 2 two 3 three", "ZADD ?"),
            ("SADD set member", "SADD ?"),
            ("AUTH password", "AUTH ?"),
            ("FAKECMD key value hash", "FAKECMD ?"),
            ("GET", "GET"),
            ("SET", "SET"),
            ("PING", "PING"),
        ];

        for (input, expected) in cases {
            let result = obfuscate_redis_string(input, &config);
            assert_eq!(result, expected, "Failed for input: {}", input);
        }
    }

    #[test]
    fn test_redis_tokenizer() {
        let mut tokenizer = RedisTokenizer::new(b"SET key value");
        let (tok, typ, done) = tokenizer.scan();
        assert_eq!(tok, "SET");
        assert_eq!(typ, TokenType::Command);
        assert!(!done);

        let (tok, typ, done) = tokenizer.scan();
        assert_eq!(tok, "key");
        assert_eq!(typ, TokenType::Argument);
        assert!(!done);

        let (tok, typ, done) = tokenizer.scan();
        assert_eq!(tok, "value");
        assert_eq!(typ, TokenType::Argument);
        assert!(done);
    }

    #[test]
    fn test_redis_tokenizer_quoted() {
        let mut tokenizer = RedisTokenizer::new(b"SET key \"value with spaces\"");
        tokenizer.scan(); // SET
        tokenizer.scan(); // key

        let (tok, _, _) = tokenizer.scan();
        assert_eq!(tok, "\"value with spaces\"");
    }
}
