//! The strict corpus reader: every rule of the corpus format that a file alone can show.
//!
//! This is written from the format's contract (record.md), not from the recorder's Go code, so
//! that a mismatch between the two surfaces here instead of being copied into both.

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use crate::getter;
use crate::json::{parse_line, JStr, Json};
use crate::lists::{Getter, Group, Level, Source};
use crate::model::*;

/// A rule failure before its line number is known.
struct Fail(Rule, String);

impl From<String> for Fail {
    fn from(s: String) -> Self {
        Fail(Rule::Shape, s)
    }
}

impl From<&str> for Fail {
    fn from(s: &str) -> Self {
        Fail(Rule::Shape, s.to_string())
    }
}

type R<T> = Result<T, Fail>;

fn fail<T>(rule: Rule, msg: impl Into<String>) -> R<T> {
    Err(Fail(rule, msg.into()))
}

/// Checked access to an object's members, so that every member not consumed is an error.
struct Obj<'a> {
    members: &'a [(JStr, Json)],
    used: Vec<bool>,
    what: &'static str,
}

impl<'a> Obj<'a> {
    fn new(v: &'a Json, what: &'static str) -> R<Self> {
        let Json::Obj(members) = v else {
            return fail(Rule::Shape, format!("{what}: expected an object, found {}", v.kind()));
        };
        for (k, _) in members {
            if !k.go_canonical {
                return fail(
                    Rule::Canonical,
                    format!("{what}: member name {:?} is not Go-escaped", k.text),
                );
            }
        }
        Ok(Obj {
            members,
            used: vec![false; members.len()],
            what,
        })
    }

    fn get(&mut self, name: &str) -> Option<&'a Json> {
        let i = self.members.iter().position(|(k, _)| k.text == name)?;
        self.used[i] = true;
        Some(&self.members[i].1)
    }

    fn req_nullable(&mut self, name: &str) -> R<&'a Json> {
        self.get(name)
            .ok_or_else(|| format!("{}: missing required member `{name}`", self.what).into())
    }

    fn req(&mut self, name: &str) -> R<&'a Json> {
        let v = self.req_nullable(name)?;
        if matches!(v, Json::Null) {
            return fail(Rule::Shape, format!("{}: `{name}` must not be null", self.what));
        }
        Ok(v)
    }

    /// An optional member: absent when it has no value, never `null` (record.md §1).
    fn opt(&mut self, name: &str) -> R<Option<&'a Json>> {
        match self.get(name) {
            Some(Json::Null) => fail(
                Rule::Shape,
                format!("{}: optional member `{name}` must be absent, not null", self.what),
            ),
            v => Ok(v),
        }
    }

    fn finish(self) -> R<()> {
        for ((k, _), used) in self.members.iter().zip(&self.used) {
            if !used {
                return fail(Rule::Shape, format!("{}: unknown member `{}`", self.what, k.text));
            }
        }
        Ok(())
    }
}

fn string<'a>(v: &'a Json, what: &str) -> R<&'a str> {
    match v {
        Json::Str(s) if s.go_canonical => Ok(&s.text),
        Json::Str(s) => fail(
            Rule::Canonical,
            format!("{what}: string {:?} is not Go-escaped", s.text),
        ),
        other => fail(
            Rule::Shape,
            format!("{what}: expected a string, found {}", other.kind()),
        ),
    }
}

fn boolean(v: &Json, what: &str) -> R<bool> {
    match v {
        Json::Bool(b) => Ok(*b),
        other => fail(
            Rule::Shape,
            format!("{what}: expected a boolean, found {}", other.kind()),
        ),
    }
}

fn uint(v: &Json, what: &str) -> R<u64> {
    match v {
        Json::Num(n) if n.bytes().all(|c| c.is_ascii_digit()) => n
            .parse()
            .map_err(|_| format!("{what}: integer {n} out of range").into()),
        other => fail(
            Rule::Shape,
            format!("{what}: expected a non-negative integer, found {other:?}"),
        ),
    }
}

fn array<'a>(v: &'a Json, what: &str) -> R<&'a [Json]> {
    match v {
        Json::Arr(a) => Ok(a),
        other => fail(
            Rule::Shape,
            format!("{what}: expected an array, found {}", other.kind()),
        ),
    }
}

/// An optional array, which must be omitted rather than written empty.
fn opt_nonempty<'a>(o: &mut Obj<'a>, name: &str) -> R<&'a [Json]> {
    match o.opt(name)? {
        None => Ok(&[]),
        Some(v) => {
            let a = array(v, name)?;
            if a.is_empty() {
                return fail(Rule::Shape, format!("{}: `{name}` must be omitted when empty", o.what));
            }
            Ok(a)
        }
    }
}

fn strings(v: &Json, what: &str) -> R<Vec<String>> {
    array(v, what)?
        .iter()
        .map(|s| string(s, what).map(str::to_string))
        .collect()
}

fn strictly_sorted(v: &[String]) -> bool {
    v.windows(2).all(|w| w[0].as_bytes() < w[1].as_bytes())
}

fn listed<T: Copy + std::fmt::Display>(v: &str, parse: fn(&str) -> Option<T>, all: &[T], what: &str) -> R<T> {
    parse(v).ok_or_else(|| {
        let names: Vec<String> = all.iter().map(ToString::to_string).collect();
        format!("{what}: {v:?} is not one of {names:?}").into()
    })
}

fn source(v: &str, what: &str) -> R<Source> {
    listed(v, Source::parse, Source::ALL, what)
}

fn getter_name(v: &str, what: &str) -> R<Getter> {
    listed(v, Getter::parse, Getter::ALL, what)
}

/// Any JSON outside a streamed `value`: every string and member name must be Go-escaped.
fn go_json(v: &Json, what: &str) -> R<()> {
    match v {
        Json::Str(_) => string(v, what).map(|_| ()),
        Json::Arr(a) => a.iter().try_for_each(|x| go_json(x, what)),
        Json::Obj(m) => m.iter().try_for_each(|(k, x)| {
            if !k.go_canonical {
                return fail(
                    Rule::Canonical,
                    format!("{what}: member name {:?} is not Go-escaped", k.text),
                );
            }
            go_json(x, what)
        }),
        _ => Ok(()),
    }
}

/// A typed value in `inputs` (case.md §7), kept as JSON.
fn typed_value(v: &Json, what: &str) -> R<serde_json::Value> {
    go_json(v, what)?;
    Ok(v.to_value().map_err(|e| format!("{what}: {e}"))?)
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

fn warning(v: &Json, what: &'static str) -> R<Warning> {
    let mut o = Obj::new(v, what)?;
    let level = listed(string(o.req("level")?, what)?, Level::parse, Level::ALL, what)?;
    let message = string(o.req("message")?, what)?.to_string();
    o.finish()?;
    Ok(Warning { level, message })
}

fn warnings(o: &mut Obj<'_>, what: &'static str) -> R<Vec<Warning>> {
    opt_nonempty(o, "warnings")?.iter().map(|w| warning(w, what)).collect()
}

/// The members of a streamed setting (record.md §5.1). `value` keeps protojson's escaping, so only
/// its JSON structure is checked.
fn streamed(o: &mut Obj<'_>) -> R<Setting> {
    let what = o.what;
    // record.md §5.1: `source` is written even when `""`, which is legal only here.
    let src = source(string(o.req("source")?, what)?, &format!("{what} source"))?;
    let unset_source = match o.opt("unset_source")? {
        Some(u) => {
            let s = source(string(u, what)?, &format!("{what} unset_source"))?;
            if s == Source::Empty {
                return fail(
                    Rule::Shape,
                    format!("{what} unset_source: \"\" is omitted, not written (record.md §5.1)"),
                );
            }
            Some(s)
        }
        None => None,
    };
    let value = match o.get("value") {
        Some(v) => Some(v.to_value().map_err(|e| format!("{what} value: {e}"))?),
        None => None,
    };
    Ok(Setting {
        source: src,
        unset_source,
        value,
    })
}

fn header(v: &Json) -> R<Header> {
    let mut o = Obj::new(v, "header")?;
    o.req("type")?;
    match o.req("format")? {
        Json::Num(n) if n == "1" => {}
        other => {
            return fail(
                Rule::Shape,
                format!("header: unknown format {other:?}; this reader knows format 1"),
            )
        }
    }
    let agent_commit = string(o.req("agent_commit")?, "header agent_commit")?.to_string();
    if !is_hex(&agent_commit, 40) {
        return fail(
            Rule::Shape,
            format!("header: agent_commit {agent_commit:?} is not 40 lowercase hex"),
        );
    }
    let inputs_digest = string(o.req("inputs_digest")?, "header inputs_digest")?.to_string();
    if !inputs_digest.strip_prefix("sha256:").is_some_and(|h| is_hex(h, 64)) {
        return fail(
            Rule::Shape,
            format!("header: inputs_digest {inputs_digest:?} is not sha256: plus 64 lowercase hex"),
        );
    }
    let mut text = |m: &str| -> R<String> { Ok(string(o.req(m)?, &format!("header {m}"))?.to_string()) };
    let goos = text("goos")?;
    let goarch = text("goarch")?;
    let go_version = text("go_version")?;
    let container_image = text("container_image")?;
    let containerized = boolean(o.req("containerized")?, "header containerized")?;
    let features = strings(o.req("features")?, "header features")?;
    if !strictly_sorted(&features) {
        return fail(Rule::Shape, "header: features are not sorted and unique");
    }
    o.finish()?;
    Ok(Header {
        agent_commit,
        goos,
        goarch,
        go_version,
        container_image,
        containerized,
        features,
        inputs_digest,
    })
}

/// What a case line says about the case once it started, before its key lines are read.
struct StartedLine {
    origin: String,
    containerized: bool,
    features: Vec<String>,
    side_effects: Vec<SideEffect>,
    updates: Vec<UpdateResult>,
}

/// A case line, plus the key lines read so far.
struct Pending {
    case: Case,
    line: usize,
    /// `inputs.keys` as written; `None` when omitted.
    written_keys: Option<Vec<KeySpec>>,
    started: Option<StartedLine>,
    key_lines: Vec<KeyLine>,
}

impl Pending {
    fn override_for(&self, key: &str) -> Option<&[Getter]> {
        self.written_keys
            .as_ref()?
            .iter()
            .find(|k| k.key == key)
            .and_then(|k| k.getters.as_deref())
    }

    /// Rules that need all of the case's key lines; builds the case.
    fn finish(self) -> R<Case> {
        let Pending {
            mut case,
            written_keys,
            started,
            key_lines,
            ..
        } = self;
        let Some(started) = started else {
            if !key_lines.is_empty() {
                return fail(Rule::Consistency, "a case with a startup_error must have no key lines");
            }
            let Some(keys) = written_keys else {
                return fail(
                    Rule::Reconstruction,
                    "a case with a startup_error must write inputs.keys",
                );
            };
            case.inputs.keys = keys;
            return Ok(case);
        };
        let seen: Vec<String> = key_lines.iter().map(|k| k.key.clone()).collect();
        let keys = match written_keys {
            None => seen
                .iter()
                .map(|k| KeySpec {
                    key: k.clone(),
                    getters: None,
                })
                .collect(),
            Some(entries) => {
                let listed: Vec<String> = entries.iter().map(|k| k.key.clone()).collect();
                let mut sorted = listed.clone();
                sorted.sort();
                if sorted != seen {
                    return fail(
                        Rule::Reconstruction,
                        format!("the key lines {seen:?} are not exactly inputs.keys {listed:?}"),
                    );
                }
                let needed = !strictly_sorted(&listed) || entries.iter().any(|k| k.getters.is_some());
                if !needed {
                    return fail(
                        Rule::Reconstruction,
                        "inputs.keys must be omitted: the keys are in byte order, have no getters override and \
                         the case started",
                    );
                }
                entries
            }
        };
        if keys.is_empty() {
            return fail(Rule::Consistency, "a case that started must have at least one key line");
        }
        if let Some(e) = started.side_effects.iter().find(|e| seen.contains(&e.key)) {
            return fail(
                Rule::Consistency,
                format!("side_effects names the case's own key {:?}", e.key),
            );
        }
        case.inputs.keys = keys;
        case.outcome = Outcome::Started(Started {
            origin: started.origin,
            containerized: started.containerized,
            features: started.features,
            side_effects: started.side_effects,
            updates: started.updates,
            keys: key_lines,
        });
        Ok(case)
    }
}

/// `inputs` (record.md §3.1); `keys` is returned separately since it may need reconstruction.
fn inputs(v: &Json) -> R<(Inputs, Option<Vec<KeySpec>>)> {
    let mut o = Obj::new(v, "inputs")?;
    let env = match o.opt("env")? {
        None => None,
        Some(env) => {
            let Json::Obj(members) = env else {
                return fail(Rule::Shape, "inputs.env: expected an object");
            };
            let mut vars = BTreeMap::new();
            for (name, value) in members {
                let valid = name.go_canonical
                    && name
                        .text
                        .bytes()
                        .enumerate()
                        .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
                    && !name.text.is_empty();
                if !valid {
                    return fail(
                        Rule::Shape,
                        format!("inputs.env: invalid variable name {:?}", name.text),
                    );
                }
                vars.insert(name.text.clone(), string(value, "inputs.env value")?.to_string());
            }
            Some(vars)
        }
    };
    let mut text = |m: &'static str| -> R<Option<String>> {
        match o.opt(m)? {
            Some(s) => Ok(Some(string(s, &format!("inputs.{m}"))?.to_string())),
            None => Ok(None),
        }
    };
    let yaml = text("yaml")?;
    let fleet_policy = text("fleet_policy")?;
    let mut cli = Vec::new();
    for c in opt_nonempty(&mut o, "cli")? {
        let mut e = Obj::new(c, "inputs.cli element")?;
        let key = string(e.req("key")?, "inputs.cli key")?.to_string();
        let value = typed_value(e.req_nullable("value")?, "inputs.cli value")?;
        e.finish()?;
        cli.push(CliOverride { key, value });
    }
    let mut updates = Vec::new();
    for u in opt_nonempty(&mut o, "updates")? {
        let mut e = Obj::new(u, "inputs.updates element")?;
        let key = string(e.req("key")?, "inputs.updates key")?.to_string();
        let unset = match e.opt("op")? {
            None => false,
            Some(op) if string(op, "inputs.updates op")? == "unset" => true,
            Some(_) => return fail(Rule::Shape, "inputs.updates: `op` is written only for \"unset\""),
        };
        let src = source(
            string(e.req("source")?, "inputs.updates source")?,
            "inputs.updates source",
        )?;
        if !Source::UPDATE.contains(&src) {
            return fail(
                Rule::Shape,
                format!("inputs.updates: source {src} may not be written by an update"),
            );
        }
        let op = match (unset, e.get("value")) {
            (true, Some(_)) => return fail(Rule::Shape, "inputs.updates: an unset has no `value`"),
            (false, None) => return fail(Rule::Shape, "inputs.updates: a set needs a `value`"),
            (false, Some(v)) => UpdateOp::Set(typed_value(v, "inputs.updates value")?),
            (true, None) => UpdateOp::Unset,
        };
        e.finish()?;
        updates.push(Update { key, op, source: src });
    }
    let keys = match o.opt("keys")? {
        None => None,
        Some(k) => {
            let mut entries: Vec<KeySpec> = Vec::new();
            for e in array(k, "inputs.keys")? {
                let mut eo = Obj::new(e, "inputs.keys element")?;
                let key = string(eo.req("key")?, "inputs.keys key")?.to_string();
                let getters = match eo.opt("getters")? {
                    None => None,
                    Some(g) => {
                        let g = strings(g, "inputs.keys getters")?;
                        if g.is_empty() {
                            return fail(Rule::Shape, "inputs.keys: a getters override must be non-empty");
                        }
                        let parsed: Vec<Getter> = g
                            .iter()
                            .map(|n| getter_name(n, "inputs.keys getters"))
                            .collect::<R<Vec<_>>>()?;
                        // case.md §8a item 4: a getters override must not name the same getter twice.
                        let mut sorted = parsed.clone();
                        sorted.sort();
                        if sorted.windows(2).any(|w| w[0] == w[1]) {
                            return fail(
                                Rule::Shape,
                                format!("inputs.keys: getters override {g:?} names the same getter twice"),
                            );
                        }
                        Some(parsed)
                    }
                };
                eo.finish()?;
                if entries.iter().any(|x| x.key == key) {
                    return fail(Rule::Shape, format!("inputs.keys: duplicate key {key:?}"));
                }
                entries.push(KeySpec { key, getters });
            }
            if entries.is_empty() {
                return fail(Rule::Shape, "inputs.keys: must list at least one key");
            }
            Some(entries)
        }
    };
    o.finish()?;
    let inputs = Inputs {
        env,
        yaml,
        fleet_policy,
        cli,
        updates,
        keys: Vec::new(),
    };
    Ok((inputs, keys))
}

fn case_line(v: &Json, header: &Header) -> R<Pending> {
    let mut o = Obj::new(v, "case line")?;
    o.req("type")?;
    let name = string(o.req("case")?, "case")?.to_string();
    let name_ok = name
        .bytes()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
    if !name_ok {
        return fail(
            Rule::Shape,
            format!("case name {name:?} does not match ^[a-z0-9][a-z0-9-]*$"),
        );
    }
    let group = listed(string(o.req("group")?, "group")?, Group::parse, Group::ALL, "group")?;
    let why = strings(o.req("why")?, "why")?;
    if group == Group::Behavior && why.is_empty() {
        return fail(Rule::Shape, "a behavior case must have a non-empty why");
    }
    let (inputs, written_keys) = inputs(o.req("inputs")?)?;
    let origin = match (o.opt("origin")?, o.opt("startup_error")?) {
        (Some(v), None) => Ok(string(v, "origin")?.to_string()),
        (None, Some(v)) => Err(string(v, "startup_error")?.to_string()),
        _ => {
            return fail(
                Rule::Consistency,
                "a case line must have exactly one of `origin` and `startup_error`",
            )
        }
    };
    let containerized = o.opt("containerized")?;
    let features = o.opt("features")?;
    let side_effects = opt_nonempty(&mut o, "side_effects")?;
    let recorded = opt_nonempty(&mut o, "updates")?;
    let construction_warnings = opt_nonempty(&mut o, "construction_warnings")?
        .iter()
        .map(|w| warning(w, "construction_warnings element"))
        .collect::<R<Vec<_>>>()?;
    let sort_key = |w: &Warning| (w.message.clone(), w.level.as_str());
    if construction_warnings
        .windows(2)
        .any(|w| sort_key(&w[0]) > sort_key(&w[1]))
    {
        return fail(
            Rule::Shape,
            "construction_warnings are not sorted by message, then level",
        );
    }
    let mut pending = Pending {
        case: Case {
            name,
            group,
            why,
            inputs,
            construction_warnings,
            outcome: Outcome::StartupError(String::new()),
        },
        line: 0,
        written_keys,
        started: None,
        key_lines: Vec::new(),
    };
    let origin = match origin {
        Ok(origin) => origin,
        Err(error) => {
            // record.md §3.3: no update ran, so `updates` is omitted; `inputs.updates` still lists them.
            if containerized.is_some() || features.is_some() || !side_effects.is_empty() || !recorded.is_empty() {
                return fail(
                    Rule::Consistency,
                    "on startup failure `features`, `containerized`, `side_effects` and `updates` must be omitted",
                );
            }
            o.finish()?;
            pending.case.outcome = Outcome::StartupError(error);
            return Ok(pending);
        }
    };
    let containerized = match containerized {
        None => header.containerized,
        Some(c) => {
            let c = boolean(c, "containerized")?;
            if c == header.containerized {
                return fail(
                    Rule::Consistency,
                    "`containerized` is written only when it differs from the header",
                );
            }
            c
        }
    };
    let features = match features {
        None => header.features.clone(),
        Some(f) => {
            let f = strings(f, "features")?;
            if !strictly_sorted(&f) {
                return fail(Rule::Shape, "features are not sorted and unique");
            }
            if f == header.features {
                return fail(
                    Rule::Consistency,
                    "`features` is written only when it differs from the header",
                );
            }
            f
        }
    };
    let mut effects: Vec<SideEffect> = Vec::new();
    for s in side_effects {
        let mut e = Obj::new(s, "side_effects element")?;
        let key = string(e.req("key")?, "side_effects key")?.to_string();
        let setting = if let Some(absent) = e.opt("absent")? {
            if !boolean(absent, "side_effects absent")? {
                return fail(Rule::Shape, "side_effects: `absent` is written only as true");
            }
            None
        } else {
            Some(streamed(&mut e)?)
        };
        e.finish()?;
        if effects.last().is_some_and(|p| p.key.as_bytes() >= key.as_bytes()) {
            return fail(
                Rule::Shape,
                format!("side_effects are not strictly sorted by key at {key:?}"),
            );
        }
        effects.push(SideEffect { key, setting });
    }
    let expected = pending.case.inputs.updates.len();
    if recorded.len() != expected {
        return fail(
            Rule::Consistency,
            format!(
                "`updates` has {} elements but inputs.updates has {expected}",
                recorded.len()
            ),
        );
    }
    let mut updates = Vec::new();
    for u in recorded {
        let mut e = Obj::new(u, "updates element")?;
        let seq_delta = uint(e.req("seq_delta")?, "seq_delta")?;
        let timed_out = match e.opt("timed_out")? {
            None => false,
            Some(t) => {
                if !boolean(t, "timed_out")? {
                    return fail(Rule::Shape, "updates: `timed_out` is written only as true");
                }
                if seq_delta == 0 {
                    return fail(Rule::Consistency, "updates: `timed_out` requires seq_delta > 0");
                }
                true
            }
        };
        let warnings = warnings(&mut e, "update warning")?;
        e.finish()?;
        updates.push(UpdateResult {
            seq_delta,
            timed_out,
            warnings,
        });
    }
    o.finish()?;
    pending.started = Some(StartedLine {
        origin,
        containerized,
        features,
        side_effects: effects,
        updates,
    });
    Ok(pending)
}

/// A read (record.md §5.3); returns it with its written `source`, which the caller reconstructs.
fn read_checkpoint(v: &Json, what: &'static str) -> R<(Vec<GetterRead>, String, Option<Source>)> {
    let mut o = Obj::new(v, what)?;
    let mut reads = Vec::new();
    let getters = array(o.req("getters")?, "getters")?;
    if getters.is_empty() {
        return fail(Rule::Shape, format!("{what}: getters must be non-empty"));
    }
    for g in getters {
        let mut r = Obj::new(g, "getter result")?;
        let name = getter_name(string(r.req("getter")?, "getter")?, "getter")?;
        let raw = r.req_nullable("result")?;
        go_json(raw, "getter result")?;
        let result = getter::decode(name, raw).map_err(|e| Fail(Rule::GetterResult, format!("{what}: {e}")))?;
        let warnings = warnings(&mut r, "getter warning")?;
        r.finish()?;
        reads.push(GetterRead {
            getter: name,
            result,
            warnings,
        });
    }
    let go_type = string(o.req("go_type")?, "go_type")?.to_string();
    let written = match o.opt("source")? {
        None => None,
        Some(s) => {
            let src = source(string(s, "read source")?, "read source")?;
            // record.md §5.1's `""` is legal only for a streamed setting's `source`; a read's
            // source (`GetSource(key).String()`) keeps its own rules and is never empty.
            if src == Source::Empty {
                return fail(Rule::Shape, format!("{what}.source must not be \"\""));
            }
            Some(src)
        }
    };
    o.finish()?;
    Ok((reads, go_type, written))
}

/// Reconstructs a read's `source` from the streamed setting it may be omitted in favour of (§5.3).
fn read_source(written: Option<Source>, streamed: Option<Source>, what: &str) -> R<Source> {
    match (written, streamed) {
        (None, None) => fail(
            Rule::Reconstruction,
            format!("{what}.source must be written when the compared setting is null"),
        ),
        (Some(w), Some(s)) if w == s => fail(
            Rule::Reconstruction,
            format!("{what}.source must be omitted when it equals the streamed source {s:?}"),
        ),
        (Some(w), _) => Ok(w),
        (None, Some(s)) => Ok(s),
    }
}

fn key_line(v: &Json, case: &mut Pending) -> R<()> {
    let Some(started) = &case.started else {
        return fail(Rule::Consistency, "a case with a startup_error must have no key lines");
    };
    let mut o = Obj::new(v, "key line")?;
    o.req("type")?;
    // The caller has matched `case` against the open case line.
    o.req("case")?;
    let key = string(o.req("key")?, "key")?.to_string();
    let snapshot = match o.req_nullable("snapshot")? {
        Json::Null => None,
        s => {
            let mut so = Obj::new(s, "snapshot")?;
            let setting = streamed(&mut so)?;
            so.finish()?;
            Some(setting)
        }
    };
    let mut raw_events = Vec::new();
    for e in opt_nonempty(&mut o, "events")? {
        let mut eo = Obj::new(e, "event")?;
        let setting = streamed(&mut eo)?;
        let seq = NonZeroU64::new(uint(eo.req("seq")?, "event seq")?).ok_or("event seq must be >= 1")?;
        let update = eo.opt("update")?.map(|u| uint(u, "event update")).transpose()?;
        eo.finish()?;
        raw_events.push((setting, seq, update));
    }
    // Reconstruct omitted event attributions (§5.2) and check written ones name an update (§3.3).
    let updates = &case.case.inputs.updates;
    let matching: Vec<usize> = (0..updates.len()).filter(|&i| updates[i].key == key).collect();
    let written = raw_events.iter().filter(|e| e.2.is_some()).count();
    if written != 0 && written != raw_events.len() {
        return fail(
            Rule::Reconstruction,
            "event `update` must be written on all of a line's events or on none",
        );
    }
    let attributed: Vec<usize> = if written == 0 {
        if !raw_events.is_empty() && matching.len() != 1 {
            return fail(
                Rule::Reconstruction,
                format!(
                    "event `update` omitted but {} of the case's updates have key {key:?}",
                    matching.len()
                ),
            );
        }
        raw_events.iter().map(|_| matching[0]).collect()
    } else {
        let idx: Vec<usize> = raw_events
            .iter()
            .map(|e| usize::try_from(e.2.unwrap_or_default()).unwrap_or(usize::MAX))
            .collect();
        if let Some(bad) = idx.iter().find(|&&i| i >= updates.len()) {
            return fail(
                Rule::Consistency,
                format!("event `update` {bad} names no update (the case has {})", updates.len()),
            );
        }
        if matching.len() == 1 && idx.iter().all(|&i| i == matching[0]) {
            return fail(
                Rule::Reconstruction,
                "event `update` must be omitted: every event belongs to the one update of this key",
            );
        }
        idx
    };
    let mut events = Vec::new();
    for ((setting, seq, _), update) in raw_events.into_iter().zip(attributed) {
        let delta = started.updates[update].seq_delta;
        if seq.get() > delta {
            return fail(
                Rule::Consistency,
                format!("event seq {seq} exceeds seq_delta {delta} of update {update}"),
            );
        }
        events.push(Event { setting, seq, update });
    }
    let mut ro = Obj::new(o.req("reads")?, "reads")?;
    let (snap_getters, go_type, written) = read_checkpoint(ro.req("snapshot")?, "reads.snapshot")?;
    let snap_source = read_source(written, snapshot.as_ref().map(|s| s.source), "reads.snapshot")?;
    let snapshot_read = Read {
        getters: snap_getters,
        go_type,
        source: snap_source,
    };
    let final_ = match (ro.opt("final")?, updates.is_empty()) {
        (Some(_), true) => return fail(Rule::Consistency, "reads.final is present but the case has no updates"),
        (None, false) => return fail(Rule::Consistency, "reads.final is missing but the case has updates"),
        (None, true) => None,
        (Some(f), false) => {
            let (getters, go_type, written) = read_checkpoint(f, "reads.final")?;
            let compared = events.last().map(|e| &e.setting).or(snapshot.as_ref());
            let source = read_source(written, compared.map(|s| s.source), "reads.final")?;
            let names = |g: &[GetterRead]| g.iter().map(|r| r.getter).collect::<Vec<_>>();
            if names(&getters) != names(&snapshot_read.getters) {
                return fail(
                    Rule::Consistency,
                    "reads.final and reads.snapshot call different getters",
                );
            }
            Some(Read {
                getters,
                go_type,
                source,
            })
        }
    };
    ro.finish()?;
    if let Some(over) = case.override_for(&key) {
        let called: Vec<Getter> = snapshot_read.getters.iter().map(|r| r.getter).collect();
        if over != called.as_slice() {
            return fail(
                Rule::Consistency,
                format!("reads call {called:?}, not the inputs.keys override {over:?}"),
            );
        }
    }
    o.finish()?;
    case.key_lines.push(KeyLine {
        key,
        snapshot,
        events,
        reads: Reads {
            snapshot: snapshot_read,
            final_,
        },
    });
    Ok(())
}

fn push(errors: &mut Vec<Violation>, line: usize, Fail(rule, message): Fail) {
    errors.push(Violation { line, rule, message });
}

/// Closes the open case: runs the rules that need all its key lines, then keeps it.
fn close(pending: Option<Pending>, cases: &mut Vec<Case>, errors: &mut Vec<Violation>) {
    if let Some(p) = pending {
        let (line, name) = (p.line, p.case.name.clone());
        match p.finish() {
            Ok(c) => cases.push(c),
            Err(Fail(rule, m)) => push(errors, line, Fail(rule, format!("case {name:?}: {m}"))),
        }
    }
}

fn sort_key(v: &Json) -> Option<(String, u8, String)> {
    let Json::Obj(m) = v else { return None };
    let get = |n: &str| m.iter().find(|(k, _)| k.text == n).and_then(|(_, v)| v.as_str());
    match get("type")? {
        "case" => Some((get("case")?.to_string(), 0, String::new())),
        "key" => Some((get("case")?.to_string(), 1, get("key")?.to_string())),
        _ => None,
    }
}

fn member<'a>(v: &'a Json, name: &str) -> Option<&'a str> {
    match v {
        Json::Obj(m) => m.iter().find(|(k, _)| k.text == name).and_then(|(_, v)| v.as_str()),
        _ => None,
    }
}

/// Reads a whole corpus (record.md), filling in every member a writer may omit.
///
/// # Errors
///
/// Returns every rule violation found, each with its line. The reader goes on past a bad line, so
/// one run reports all of them.
pub fn read(bytes: &[u8]) -> Result<Corpus, Vec<Violation>> {
    let mut errors = Vec::new();
    if bytes.starts_with(b"\xef\xbb\xbf") {
        push(&mut errors, 1, Fail(Rule::File, "starts with a BOM".into()));
    }
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        push(
            &mut errors,
            0,
            Fail(Rule::File, "the last line does not end in \\n".into()),
        );
    }
    let mut lines: Vec<&[u8]> = bytes.split(|&c| c == b'\n').collect();
    if bytes.ends_with(b"\n") || bytes.is_empty() {
        lines.pop();
    }
    let mut header_info: Option<Header> = None;
    let mut pending: Option<Pending> = None;
    let mut cases: Vec<Case> = Vec::new();
    let mut prev: Option<(String, u8, String)> = None;
    for (i, raw) in lines.iter().enumerate() {
        let n = i + 1;
        if raw.is_empty() {
            push(&mut errors, n, Fail(Rule::File, "blank line".into()));
            continue;
        }
        // Validated once per line; the scanner then works on `&str`.
        let text = match String::from_utf8(raw.to_vec()) {
            Ok(t) => t,
            Err(e) => {
                let at = e.utf8_error().valid_up_to() + 1;
                push(&mut errors, n, Fail(Rule::File, format!("not UTF-8 at byte {at}")));
                continue;
            }
        };
        let v = match parse_line(&text) {
            Ok(v) => v,
            Err(e) => {
                let e = e.strip_prefix("canonical form: ").unwrap_or(&e).to_string();
                push(&mut errors, n, Fail(Rule::Canonical, e));
                continue;
            }
        };
        let result = match (member(&v, "type"), n) {
            (Some("header"), 1) => header(&v).map(|h| header_info = Some(h)),
            (Some("header"), _) => fail(Rule::Order, "a header line may only be line 1"),
            (_, 1) => fail(Rule::Order, "the first line must be the header"),
            (Some("case"), _) => {
                close(pending.take(), &mut cases, &mut errors);
                match &header_info {
                    // Without a header, case lines cannot be checked against it; that is already reported.
                    None => Ok(()),
                    Some(h) => case_line(&v, h).and_then(|mut c| {
                        c.line = n;
                        if cases.iter().any(|x| x.name == c.case.name) {
                            return fail(Rule::Order, format!("case name {:?} is not unique", c.case.name));
                        }
                        pending = Some(c);
                        Ok(())
                    }),
                }
            }
            (Some("key"), _) => {
                let name = member(&v, "case");
                match pending.as_mut() {
                    Some(c) if Some(c.case.name.as_str()) == name => {
                        key_line(&v, c).map_err(|Fail(r, m)| Fail(r, format!("case {:?} key line: {m}", c.case.name)))
                    }
                    _ => fail(
                        Rule::Order,
                        format!("key line for case {name:?} does not follow its case line"),
                    ),
                }
            }
            (other, _) => fail(Rule::Shape, format!("line type {other:?} is not header, case or key")),
        };
        if let Err(f) = result {
            push(&mut errors, n, f);
        }
        if n > 1 {
            match sort_key(&v) {
                Some(k) => {
                    if let Some(p) = &prev {
                        let a = (p.0.as_bytes(), p.1, p.2.as_bytes());
                        let b = (k.0.as_bytes(), k.1, k.2.as_bytes());
                        if a >= b {
                            push(
                                &mut errors,
                                n,
                                Fail(Rule::Order, "lines are not strictly sorted by (case, rank, key)".into()),
                            );
                        }
                    }
                    prev = Some(k);
                }
                None => push(
                    &mut errors,
                    n,
                    Fail(Rule::Order, "line has no (case, key) to sort by".into()),
                ),
            }
        }
    }
    close(pending.take(), &mut cases, &mut errors);
    if lines.is_empty() {
        push(
            &mut errors,
            0,
            Fail(Rule::File, "the corpus is empty; it needs a header line".into()),
        );
    } else if header_info.is_some() && cases.is_empty() {
        push(&mut errors, 0, Fail(Rule::File, "the corpus has no case lines".into()));
    } else if header_info.is_some() {
        // record.md §3.2: `first_snapshot` needs the one `baseline-default` case; case.md §3.2
        // allows only one case of group `baseline`.
        let baseline: Vec<&Case> = cases.iter().filter(|c| c.group == Group::Baseline).collect();
        match baseline.len() {
            0 => push(
                &mut errors,
                0,
                Fail(
                    Rule::Consistency,
                    format!(
                        "the corpus has no case of group baseline named {BASELINE_CASE:?}; first_snapshot needs it"
                    ),
                ),
            ),
            1 if baseline[0].name != BASELINE_CASE => push(
                &mut errors,
                0,
                Fail(
                    Rule::Consistency,
                    format!(
                        "the corpus's one case of group baseline is named {:?}, not {BASELINE_CASE:?}",
                        baseline[0].name
                    ),
                ),
            ),
            1 => {}
            n => push(
                &mut errors,
                0,
                Fail(
                    Rule::Consistency,
                    format!("the corpus has {n} cases of group baseline; case.md §3.2 allows only one"),
                ),
            ),
        }
    }
    match header_info {
        Some(header) if errors.is_empty() => Ok(Corpus { header, cases }),
        _ => Err(errors),
    }
}
