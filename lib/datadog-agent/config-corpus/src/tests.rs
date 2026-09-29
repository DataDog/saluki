//! One test per rule family, on small inline corpora.

use crate::*;

const HEADER: &str = r#"{"agent_commit":"281d921619d52ce7b99aef40607285992c9c2e89","container_image":"i","containerized":false,"features":[],"format":1,"go_version":"go1.26.7","goarch":"arm64","goos":"linux","inputs_digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","type":"header"}"#;

const BASELINE: &str = r#"{"case":"baseline-default","group":"baseline","inputs":{},"origin":"datadog.yaml","type":"case","why":[]}
{"case":"baseline-default","key":"a","reads":{"snapshot":{"getters":[{"getter":"GetInt","result":1}],"go_type":"int"}},"snapshot":{"source":"default","value":1},"type":"key"}
{"case":"baseline-default","key":"b","reads":{"snapshot":{"getters":[{"getter":"GetString","result":"x"}],"go_type":"string"}},"snapshot":{"source":"default","value":"x"},"type":"key"}
{"case":"baseline-default","key":"c","reads":{"snapshot":{"getters":[{"getter":"GetBool","result":true}],"go_type":"bool"}},"snapshot":{"source":"default","value":true},"type":"key"}"#;

fn corpus(lines: &str) -> Vec<u8> {
    format!("{HEADER}\n{BASELINE}\n{lines}\n").into_bytes()
}

fn ok(lines: &str) -> Corpus {
    read(&corpus(lines)).unwrap_or_else(|v| panic!("{v:#?}"))
}

/// The rules broken, the first one being the line's own; later ones are knock-on effects.
fn rules(lines: &str) -> Vec<Rule> {
    read(&corpus(lines))
        .expect_err("violations")
        .iter()
        .map(|v| v.rule)
        .collect()
}

fn first(rules: Vec<Rule>) -> Rule {
    rules[0]
}

fn started<'a>(c: &'a Corpus, name: &str) -> &'a Started {
    match &c.case(name).expect("case").outcome {
        Outcome::Started(s) => s,
        Outcome::StartupError(e) => panic!("startup error {e}"),
    }
}

const CASE: &str =
    r#"{"case":"z","group":"behavior","inputs":{"yaml":"a: 2\n"},"origin":"datadog.yaml","type":"case","why":["w"]}"#;

fn key(extra_read: &str) -> String {
    format!(
        r#"{{"case":"z","key":"a","reads":{{"snapshot":{{"getters":[{extra_read}],"go_type":"int"}}}},"snapshot":{{"source":"file","value":2}},"type":"key"}}"#
    )
}

#[test]
fn canonical_form() {
    let good = key(r#"{"getter":"GetInt","result":2}"#);
    ok(&format!("{CASE}\n{good}"));
    // Whitespace between tokens.
    assert_eq!(
        first(rules(&format!("{CASE}\n{}", good.replacen(":", ": ", 1)))),
        Rule::Canonical
    );
    // Member order at depth.
    let unsorted = good.replace(r#"{"source":"file","value":2}"#, r#"{"value":2,"source":"file"}"#);
    assert_eq!(first(rules(&format!("{CASE}\n{unsorted}"))), Rule::Canonical);
    // Go escapes U+2028 outside `value`; protojson does not, and `value` keeps its escaping.
    let why = |w: &str| CASE.replace(r#"["w"]"#, &format!("[\"{w}\"]"));
    ok(&format!("{}\n{good}", why("\\u2028")));
    assert_eq!(first(rules(&format!("{}\n{good}", why("\u{2028}")))), Rule::Canonical);
    assert_eq!(first(rules(&format!("{}\n{good}", why("\\u0041")))), Rule::Canonical);
    let raw_value = good.replace(r#""value":2"#, "\"value\":\"\u{2028}\"");
    ok(&format!("{CASE}\n{raw_value}"));
}

#[test]
fn shape_and_nulls() {
    let good = key(r#"{"getter":"GetInt","result":2}"#);
    let null_opt = CASE.replace(r#""group""#, r#""features":null,"group""#);
    assert_eq!(first(rules(&format!("{null_opt}\n{good}"))), Rule::Shape);
    let unknown = CASE.replace(r#""why":["w"]}"#, r#""why":["w"],"zz":1}"#);
    assert_eq!(first(rules(&format!("{unknown}\n{good}"))), Rule::Shape);
    let bad_source = good.replace(r#""source":"file""#, r#""source":"nope""#);
    assert_eq!(first(rules(&format!("{CASE}\n{bad_source}"))), Rule::Shape);
    let bad_group = CASE.replace(r#""behavior""#, r#""other""#);
    assert!(rules(&format!("{bad_group}\n{good}")).contains(&Rule::Shape));
    // A header with no case lines.
    let empty = read(format!("{HEADER}\n").as_bytes()).expect_err("empty");
    assert_eq!(empty[0].rule, Rule::File);
}

#[test]
fn keys_reconstructed_when_omitted() {
    let c = ok(&format!("{CASE}\n{}", key(r#"{"getter":"GetInt","result":2}"#)));
    let keys = &c.case("z").unwrap().inputs.keys;
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key, "a");
    assert!(keys[0].getters.is_none());
    // Written though nothing needs it.
    let written = CASE.replace(r#""yaml""#, r#""keys":[{"key":"a"}],"yaml""#);
    assert_eq!(
        first(rules(&format!(
            "{written}\n{}",
            key(r#"{"getter":"GetInt","result":2}"#)
        ))),
        Rule::Reconstruction
    );
}

const UPD_CASE: &str = r#"{"case":"z","group":"behavior","inputs":{"updates":[{"key":"a","source":"remote-config","value":3}]},"origin":"datadog.yaml","type":"case","updates":[{"seq_delta":1}],"why":["w"]}"#;

fn upd_key(events: &str, final_source: &str) -> String {
    format!(
        r#"{{"case":"z","events":[{events}],"key":"a","reads":{{"final":{{"getters":[{{"getter":"GetInt","result":3}}],"go_type":"int"{final_source}}},"snapshot":{{"getters":[{{"getter":"GetInt","result":1}}],"go_type":"int"}}}},"snapshot":{{"source":"default","value":1}},"type":"key"}}"#
    )
}

#[test]
fn event_update_and_read_source_reconstructed() {
    let c = ok(&format!(
        "{UPD_CASE}\n{}",
        upd_key(r#"{"seq":1,"source":"remote-config","value":3}"#, "")
    ));
    let line = &started(&c, "z").keys[0];
    assert_eq!(line.events[0].update, 0);
    assert_eq!(line.events[0].seq.get(), 1);
    assert_eq!(line.reads.snapshot.source, Source::Default);
    assert_eq!(line.reads.final_.as_ref().unwrap().source, Source::RemoteConfig);
    // `update` written though it is implied.
    assert_eq!(
        first(rules(&format!(
            "{UPD_CASE}\n{}",
            upd_key(r#"{"seq":1,"source":"remote-config","update":0,"value":3}"#, "")
        ))),
        Rule::Reconstruction
    );
    // `source` written though it equals the last event's.
    assert_eq!(
        first(rules(&format!(
            "{UPD_CASE}\n{}",
            upd_key(
                r#"{"seq":1,"source":"remote-config","value":3}"#,
                r#","source":"remote-config""#
            )
        ))),
        Rule::Reconstruction
    );
    // seq 0 is not an event.
    assert_eq!(
        first(rules(&format!(
            "{UPD_CASE}\n{}",
            upd_key(r#"{"seq":0,"source":"remote-config"}"#, "")
        ))),
        Rule::Shape
    );
}

#[test]
fn startup_failure() {
    let failed = r#"{"case":"z","group":"behavior","inputs":{"keys":[{"key":"a"}],"updates":[{"key":"a","source":"remote-config","value":3}]},"startup_error":"bad","type":"case","why":["w"]}"#;
    let c = ok(failed);
    let case = c.case("z").unwrap();
    assert!(matches!(&case.outcome, Outcome::StartupError(e) if e == "bad"));
    assert_eq!(case.inputs.updates.len(), 1);
    assert!(c.first_snapshot("z").is_none());
    let with_updates = failed.replace(r#""type""#, r#""type":"case","updates":[{"seq_delta":1}],"x""#);
    assert!(!rules(&with_updates).is_empty());
    let with_updates = failed.replace(r#","type":"case","#, r#","type":"case","updates":[{"seq_delta":1}],"#);
    assert_eq!(first(rules(&with_updates)), Rule::Consistency);
}

fn result(getter: &str, json: &str) -> Result<GetterResult, Vec<Violation>> {
    let line = key(&format!(r#"{{"getter":"{getter}","result":{json}}}"#));
    read(&corpus(&format!("{CASE}\n{line}"))).map(|c| started(&c, "z").keys[0].reads.snapshot.getters[0].result.clone())
}

#[test]
fn getter_result_shapes() {
    assert!(matches!(
        result("GetInt", "2"),
        Ok(GetterResult::Int(Number { value: 2, .. }))
    ));
    assert!(result("GetInt32", "2147483648").is_err());
    assert!(result("GetSizeInBytes", "-1").is_err());
    assert!(matches!(
        result("GetDuration", "10000000000"),
        Ok(GetterResult::Duration(_))
    ));
    let Ok(GetterResult::Float64(GoFloat::Finite(f))) = result("GetFloat64", "1.0") else {
        panic!("float")
    };
    assert_eq!(f.token, "1.0");
    assert!(result("GetFloat64", "1").is_err(), "an integer token is not a float");
    assert!(matches!(
        result("GetFloat64", r#"{"$float":"NaN"}"#),
        Ok(GetterResult::Float64(GoFloat::NaN))
    ));
    assert!(result("GetFloat64", r#"{"$float":"nan"}"#).is_err());
    assert!(matches!(
        result("GetStringSlice", "null"),
        Ok(GetterResult::StringSlice(None))
    ));
    assert!(result("GetStringSlice", "[1]").is_err());
    assert!(result("GetStringMapStringSlice", "null").is_err());
    assert!(matches!(
        result("GetStringMapStringSlice", r#"{"k":null}"#),
        Ok(GetterResult::StringMapStringSlice(_))
    ));
    assert!(result("GetStringMapString", r#"{"k":1}"#).is_err());
    let Ok(GetterResult::Get(GoValue::List(items))) = result("Get", r#"[1,1.0,{"$float":"-Inf"}]"#) else {
        panic!("generic")
    };
    assert!(matches!(items[0], GoValue::Int(_)));
    assert!(matches!(items[1], GoValue::Float(GoFloat::Finite(_))));
    assert!(matches!(items[2], GoValue::Float(GoFloat::NegInf)));
    let err = result("GetBool", "\"yes\"").unwrap_err();
    assert_eq!(err[0].rule, Rule::GetterResult);
}

#[test]
fn baseline_case_is_required_and_unique() {
    // No case of group baseline at all.
    let good = key(r#"{"getter":"GetInt","result":2}"#);
    let no_baseline = format!("{HEADER}\n{CASE}\n{good}\n").into_bytes();
    let errs = read(&no_baseline).expect_err("no baseline case");
    assert!(
        errs.iter()
            .any(|v| v.rule == Rule::Consistency && v.message.contains("no case of group baseline")),
        "{errs:#?}"
    );

    // Two cases of group baseline.
    let second = BASELINE.replace("baseline-default", "other-baseline");
    let two_baselines = format!("{HEADER}\n{BASELINE}\n{second}\n").into_bytes();
    let errs = read(&two_baselines).expect_err("two baseline cases");
    assert!(
        errs.iter()
            .any(|v| v.rule == Rule::Consistency && v.message.contains("2 cases of group baseline")),
        "{errs:#?}"
    );

    // A misnamed lone baseline case.
    let renamed = BASELINE.replace("baseline-default", "baseline-other");
    let bytes = format!("{HEADER}\n{renamed}\n").into_bytes();
    let errs = read(&bytes).expect_err("misnamed baseline case");
    assert!(
        errs.iter()
            .any(|v| v.rule == Rule::Consistency && v.message.contains("not \"baseline-default\"")),
        "{errs:#?}"
    );

    // The ordinary case, exactly one case named baseline-default of group baseline, is fine: every
    // other test in this module builds on `corpus()`, which includes it.
}

#[test]
fn streamed_source_may_be_empty_but_others_may_not() {
    // record.md §5.1: a streamed setting's `source` is legally `""`.
    let empty_snapshot_source =
        key(r#"{"getter":"GetInt","result":2}"#).replace(r#""source":"file""#, r#""source":"""#);
    // The read's own source must then be written, since it cannot equal `""` (below), and here it
    // differs from the streamed source anyway.
    let with_read_source = empty_snapshot_source.replace(r#""go_type":"int""#, r#""go_type":"int","source":"unknown""#);
    ok(&format!("{CASE}\n{with_read_source}"));

    // `unset_source` stays omitted when `""`; writing it explicitly is still a violation.
    let empty_unset_source =
        key(r#"{"getter":"GetInt","result":2}"#).replace(r#""source":"file""#, r#""source":"file","unset_source":"""#);
    assert_eq!(first(rules(&format!("{CASE}\n{empty_unset_source}"))), Rule::Shape);

    // A read's own source (`GetSource(key).String()`) keeps its current rules: never `""`.
    let empty_read_source =
        key(r#"{"getter":"GetInt","result":2}"#).replace(r#""go_type":"int""#, r#""go_type":"int","source":"""#);
    assert_eq!(first(rules(&format!("{CASE}\n{empty_read_source}"))), Rule::Shape);

    // An update's source keeps its current rules too: `""` is not in `Source::UPDATE`.
    let empty_update_source = UPD_CASE.replace(r#""source":"remote-config""#, r#""source":"""#);
    assert_eq!(first(rules(&empty_update_source)), Rule::Shape);
}

#[test]
fn getters_override_rejects_a_repeated_getter() {
    let dup = CASE.replace(
        r#""yaml""#,
        r#""keys":[{"getters":["GetInt","GetInt"],"key":"a"}],"yaml""#,
    );
    let line = key(r#"{"getter":"GetInt","result":2}"#);
    assert_eq!(first(rules(&format!("{dup}\n{line}"))), Rule::Shape);

    // A non-repeating override is unaffected.
    let ok_override = CASE.replace(
        r#""yaml""#,
        r#""keys":[{"getters":["GetInt","GetString"],"key":"a"}],"yaml""#,
    );
    let line = r#"{"case":"z","key":"a","reads":{"snapshot":{"getters":[{"getter":"GetInt","result":2},{"getter":"GetString","result":"2"}],"go_type":"int"}},"snapshot":{"source":"file","value":2},"type":"key"}"#;
    ok(&format!("{ok_override}\n{line}"));
}

#[test]
fn first_snapshot_layers() {
    // Side effects: `b` changes, `c` is absent. Key line: `a` is null.
    let case = r#"{"case":"z","group":"behavior","inputs":{"yaml":"a: 2\n"},"origin":"datadog.yaml","side_effects":[{"key":"b","source":"file","value":"y"},{"absent":true,"key":"c"}],"type":"case","why":["w"]}"#;
    let line = r#"{"case":"z","key":"a","reads":{"snapshot":{"getters":[{"getter":"Get","result":null}],"go_type":"<nil>","source":"unknown"}},"snapshot":null,"type":"key"}"#;
    let c = ok(&format!("{case}\n{line}"));
    let snap = c.first_snapshot("z").unwrap();
    assert_eq!(snap.keys().copied().collect::<Vec<_>>(), ["b"]);
    assert_eq!(snap["b"].source, Source::File);
    let base = c.first_snapshot("baseline-default").unwrap();
    assert_eq!(base.len(), 3);
}
