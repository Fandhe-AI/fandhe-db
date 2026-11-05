//! [`super::schema::OP_SCHEMAS`] から `POST /v1/query` の要求本文スキーマを
//! OpenAPI 3.1 文書（[`engine::json::JsonValue`] 木）として機械的に生成する
//! テスト支援（Issue #2104。親 #2103。NOSQL-20・TASK-253 ポインタ:
//! `docs/spec/04-behavior/nosql-surface.md`・`docs/spec/05-tasks.md`）。
//!
//! 役割: op 別の要求スキーマの単一情報源は静的表 `OP_SCHEMAS` であり、本モジュールは
//! それを読み取り専用で OpenAPI へ写像するだけで検証ロジックは持たない。後続の
//! #2105 のゴールデン検査（`tests/http_nosql/nosql20_openapi_golden.rs`。置き場は
//! `docs/openapi.json`）、および `examples/openapi_dump.rs` から呼ばれる（配信は `http::openapi_endpoint` が
//! 埋め込んだゴールデンを返し、本モジュールの出力との一致を同ゴールデンの検査が固定する）。実行時のカタログ（表名・列名・
//! テナントのデータ）は参照しないため、文書にテナント情報は含まれない。
//!
//! 方針:
//! - バージョンは `3.1.0`。Issue 本文は 3.0 の最小要件と書くが、親・後続・NOSQL-20 は
//!   3.1 前提であり、最小要件（`openapi`・`info`・`paths`・各 operation の `responses`）は
//!   3.0 と共通。3.1 なので `type` 配列・`const` を使い、`nullable` は使わない。
//! - 入れ子の `ObjectSchema` は `components/schemas` ＋ `$ref` にする（`engine::json` の
//!   深さ上限 16 に往復パースが当たらないようにするため）。
//! - 文字列エスケープは engine の `write_canonical` に委譲し、独自実装を持たない。
//! - 別実体で同名の `ObjectSchema` は `$ref` が衝突するため [`OpenApiError`] で拒否する。
//!
//! 対象外: 応答スキーマ・他パス・securitySchemes・語彙の列挙値（#2103）、配信は #2244 で実装済み。
//! `info.version` は `CARGO_PKG_VERSION` なので、クレート版を上げたらゴールデンの
//! 再生成が要る（手順は `examples/openapi_dump.rs`）。
//!
//! 手元検証: `out="$(mktemp -d)"; cargo run -q -p fandhe-db-wire-server --example
//! openapi_dump > "$out/openapi.json"` の後、`python3 -I -c 'import json,sys;
//! d=json.load(open(sys.argv[1])); assert d["openapi"].startswith("3.")' "$out/openapi.json"`。
//! 任意で外部の OpenAPI linter に通せる（リポの依存にはしない）。終わったら `rm -rf "$out"`。

use std::collections::BTreeMap;

use engine::json::{write_canonical, JsonValue};

use super::schema::{ElementType, FieldSpec, FieldType, ObjectSchema, Presence, OP_SCHEMAS};

/// 出力する OpenAPI のバージョン。
pub const OPENAPI_VERSION: &str = "3.1.0";
/// 要求を受けるパス。
pub const QUERY_PATH: &str = "/v1/query";

const REF_PREFIX: &str = "#/components/schemas/";

/// 文書生成の失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenApiError {
    /// 別実体の `ObjectSchema` が同じ `name` を持つ（`$ref` の参照先が衝突する）。
    DuplicateComponentName { name: &'static str },
}

fn s(v: &str) -> JsonValue {
    JsonValue::String(v.to_string())
}

fn obj(entries: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn type_of(t: &str) -> JsonValue {
    obj(vec![("type", s(t))])
}

fn ref_to(schema: &ObjectSchema) -> JsonValue {
    obj(vec![("$ref", s(&format!("{REF_PREFIX}{}", schema.name)))])
}

fn element_schema(e: &ElementType) -> JsonValue {
    match e {
        ElementType::Any => obj(vec![]),
        ElementType::String => type_of("string"),
        ElementType::Number => type_of("number"),
        ElementType::Object(o) => ref_to(o),
    }
}

fn field_schema(f: &FieldSpec) -> JsonValue {
    let base = match &f.ty {
        FieldType::Bool => type_of("boolean"),
        FieldType::Number => type_of("number"),
        FieldType::String => type_of("string"),
        FieldType::Scalar => obj(vec![(
            "type",
            JsonValue::Array(vec![s("string"), s("number"), s("boolean")]),
        )]),
        FieldType::Array(e) => obj(vec![("type", s("array")), ("items", element_schema(e))]),
        FieldType::StringOrArray(ElementType::String) => obj(vec![(
            "oneOf",
            JsonValue::Array(vec![
                type_of("string"),
                obj(vec![("type", s("array")), ("items", type_of("string"))]),
            ]),
        )]),
        FieldType::StringOrArray(e) => {
            obj(vec![("type", s("array")), ("items", element_schema(e))])
        }
        FieldType::Object(o) => ref_to(o),
        FieldType::AnyObject => type_of("object"),
    };
    if f.nullable {
        obj(vec![(
            "anyOf",
            JsonValue::Array(vec![base, type_of("null")]),
        )])
    } else {
        base
    }
}

fn object_schema(o: &ObjectSchema, op_name: Option<&str>) -> JsonValue {
    let mut props: BTreeMap<String, JsonValue> = o
        .fields
        .iter()
        .map(|f| (f.key.to_string(), field_schema(f)))
        .collect();
    if let Some(op) = op_name {
        props.insert(
            "op".to_string(),
            obj(vec![("type", s("string")), ("const", s(op))]),
        );
    }
    let required: Vec<JsonValue> = o
        .fields
        .iter()
        .filter(|f| f.presence == Presence::Required)
        .map(|f| s(f.key))
        .collect();
    let mut entries = vec![
        ("type", s("object")),
        ("properties", JsonValue::Object(props)),
        ("additionalProperties", JsonValue::Bool(false)),
    ];
    if !required.is_empty() {
        entries.push(("required", JsonValue::Array(required)));
    }
    obj(entries)
}

/// `o` から到達するスキーマを `seen` へ集める。訪問済みなら止まる（循環対策）。
/// 別実体で同名なら衝突として拒否する（`BTreeMap` で黙って上書きしない fail-closed）。
fn collect(
    o: &'static ObjectSchema,
    seen: &mut Vec<&'static ObjectSchema>,
) -> Result<(), OpenApiError> {
    if seen.iter().any(|x| std::ptr::eq(*x, o)) {
        return Ok(());
    }
    if seen.iter().any(|x| x.name == o.name) {
        return Err(OpenApiError::DuplicateComponentName { name: o.name });
    }
    seen.push(o);
    for f in o.fields {
        // variant 追加時にコンパイルエラーで更新を強制するため、ワイルドカードを使わない。
        match &f.ty {
            FieldType::Object(c)
            | FieldType::Array(ElementType::Object(c))
            | FieldType::StringOrArray(ElementType::Object(c)) => collect(c, seen)?,
            FieldType::Bool
            | FieldType::Number
            | FieldType::String
            | FieldType::Scalar
            | FieldType::AnyObject
            | FieldType::Array(ElementType::Any | ElementType::String | ElementType::Number)
            | FieldType::StringOrArray(
                ElementType::Any | ElementType::String | ElementType::Number,
            ) => {}
        }
    }
    Ok(())
}

fn build(ops: &[(&'static str, &'static ObjectSchema)]) -> Result<JsonValue, OpenApiError> {
    let mut seen: Vec<&'static ObjectSchema> = Vec::new();
    for (_, o) in ops {
        collect(o, &mut seen)?;
    }
    let mut components: BTreeMap<String, JsonValue> = BTreeMap::new();
    for c in &seen {
        let op_name = ops
            .iter()
            .find(|(_, o)| std::ptr::eq(*o, *c))
            .map(|(n, _)| *n);
        components.insert(c.name.to_string(), object_schema(c, op_name));
    }
    let one_of: Vec<JsonValue> = ops.iter().map(|(_, o)| ref_to(o)).collect();
    let mapping: BTreeMap<String, JsonValue> = ops
        .iter()
        .map(|(n, o)| (n.to_string(), s(&format!("{REF_PREFIX}{}", o.name))))
        .collect();
    let schema = obj(vec![
        ("oneOf", JsonValue::Array(one_of)),
        (
            "discriminator",
            obj(vec![
                ("propertyName", s("op")),
                ("mapping", JsonValue::Object(mapping)),
            ]),
        ),
    ]);
    let post = obj(vec![
        ("operationId", s("query")),
        (
            "requestBody",
            obj(vec![
                ("required", JsonValue::Bool(true)),
                (
                    "content",
                    obj(vec![("application/json", obj(vec![("schema", schema)]))]),
                ),
            ]),
        ),
        (
            "responses",
            obj(vec![(
                "default",
                obj(vec![(
                    "description",
                    s("Response schema is not described yet"),
                )]),
            )]),
        ),
    ]);
    Ok(obj(vec![
        ("openapi", s(OPENAPI_VERSION)),
        (
            "info",
            obj(vec![
                ("title", s("fandhe-db NoSQL surface")),
                ("version", s(env!("CARGO_PKG_VERSION"))),
            ]),
        ),
        ("paths", obj(vec![(QUERY_PATH, obj(vec![("post", post)]))])),
        (
            "components",
            obj(vec![("schemas", JsonValue::Object(components))]),
        ),
    ]))
}

/// `OP_SCHEMAS` から OpenAPI 3.1 文書を組み立てる。
pub fn document() -> Result<JsonValue, OpenApiError> {
    build(&OP_SCHEMAS)
}

/// [`document`] を 2 空白インデント・末尾改行付きで出力する（ゴールデン比較向け）。
pub fn render_pretty() -> Result<String, OpenApiError> {
    let doc = document()?;
    let mut out = String::new();
    write_pretty(&doc, &mut out);
    out.push('\n');
    Ok(out)
}

/// `v` を 2 空白インデントで整形出力する（末尾改行なし）。エスケープは
/// `engine::json::write_canonical` に委譲する。
pub fn write_pretty(v: &JsonValue, out: &mut String) {
    write_pretty_at(v, out, 0);
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_pretty_at(v: &JsonValue, out: &mut String, depth: usize) {
    match v {
        JsonValue::Array(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                indent(out, depth + 1);
                write_pretty_at(item, out, depth + 1);
            }
            out.push('\n');
            indent(out, depth);
            out.push(']');
        }
        JsonValue::Object(map) if !map.is_empty() => {
            out.push_str("{\n");
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                indent(out, depth + 1);
                write_canonical(&JsonValue::String(k.clone()), out);
                out.push_str(": ");
                write_pretty_at(val, out, depth + 1);
            }
            out.push('\n');
            indent(out, depth);
            out.push('}');
        }
        other => write_canonical(other, out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::json::{parse_json, JsonNumber};

    fn get<'a>(v: &'a JsonValue, k: &str) -> &'a JsonValue {
        match v {
            JsonValue::Object(m) => m.get(k).unwrap_or_else(|| panic!("missing key {k}")),
            _ => panic!("not an object"),
        }
    }

    fn strs(v: &JsonValue) -> Vec<String> {
        match v {
            JsonValue::Array(a) => a
                .iter()
                .map(|x| match x {
                    JsonValue::String(t) => t.clone(),
                    _ => panic!("not a string"),
                })
                .collect(),
            _ => panic!("not an array"),
        }
    }

    fn walk_refs(v: &JsonValue, out: &mut Vec<String>) {
        match v {
            JsonValue::Object(m) => {
                for (k, x) in m {
                    if k == "$ref" {
                        if let JsonValue::String(t) = x {
                            out.push(t.clone());
                        }
                    } else {
                        walk_refs(x, out);
                    }
                }
            }
            JsonValue::Array(a) => a.iter().for_each(|x| walk_refs(x, out)),
            _ => {}
        }
    }

    /// 生成器とは独立に `OP_SCHEMAS` から到達するスキーマを数える。
    fn reachable() -> Vec<&'static ObjectSchema> {
        fn go(o: &'static ObjectSchema, seen: &mut Vec<&'static ObjectSchema>) {
            if seen.iter().any(|x| std::ptr::eq(*x, o)) {
                return;
            }
            seen.push(o);
            for f in o.fields {
                match &f.ty {
                    FieldType::Object(c)
                    | FieldType::Array(ElementType::Object(c))
                    | FieldType::StringOrArray(ElementType::Object(c)) => go(c, seen),
                    _ => {}
                }
            }
        }
        let mut seen = Vec::new();
        for (_, o) in OP_SCHEMAS.iter() {
            go(o, &mut seen);
        }
        seen
    }

    #[test]
    fn nosql20_roundtrip_pretty_and_canonical() {
        let doc = document().unwrap();
        let pretty = render_pretty().unwrap();
        assert_eq!(parse_json(&pretty).unwrap(), doc);
        let mut compact = String::new();
        write_canonical(&doc, &mut compact);
        assert_eq!(parse_json(&compact).unwrap(), doc);
    }

    #[test]
    fn nosql20_minimum_requirements() {
        let doc = document().unwrap();
        assert_eq!(get(&doc, "openapi"), &s("3.1.0"));
        let info = get(&doc, "info");
        for k in ["title", "version"] {
            match get(info, k) {
                JsonValue::String(t) => assert!(!t.is_empty()),
                _ => panic!("info.{k} not a string"),
            }
        }
        let post = get(get(get(&doc, "paths"), QUERY_PATH), "post");
        assert_eq!(
            get(get(post, "requestBody"), "required"),
            &JsonValue::Bool(true)
        );
        match get(post, "responses") {
            JsonValue::Object(m) => {
                assert!(!m.is_empty());
                for r in m.values() {
                    assert!(matches!(get(r, "description"), JsonValue::String(_)));
                }
            }
            _ => panic!("responses not an object"),
        }
    }

    #[test]
    fn nosql20_one_of_matches_op_table() {
        let doc = document().unwrap();
        let post = get(get(get(&doc, "paths"), QUERY_PATH), "post");
        let schema = get(
            get(get(get(post, "requestBody"), "content"), "application/json"),
            "schema",
        );
        let JsonValue::Array(items) = get(schema, "oneOf") else {
            panic!("oneOf not an array");
        };
        assert_eq!(items.len(), OP_SCHEMAS.len());
        for (item, (_, o)) in items.iter().zip(OP_SCHEMAS.iter()) {
            assert_eq!(item, &ref_to(o));
        }
        let JsonValue::Object(mapping) = get(get(schema, "discriminator"), "mapping") else {
            panic!("mapping not an object");
        };
        let keys: Vec<&str> = mapping.keys().map(String::as_str).collect();
        let mut names: Vec<&str> = OP_SCHEMAS.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        assert_eq!(keys, names);
    }

    #[test]
    fn nosql20_all_refs_resolve_and_component_count_matches() {
        let doc = document().unwrap();
        let JsonValue::Object(cm) = get(get(&doc, "components"), "schemas") else {
            panic!("schemas not an object");
        };
        let mut refs = Vec::new();
        walk_refs(&doc, &mut refs);
        assert!(!refs.is_empty());
        for r in refs {
            let name = r.strip_prefix(REF_PREFIX).expect("ref prefix");
            assert!(cm.contains_key(name), "dangling ref {r}");
        }
        let reach = reachable();
        assert_eq!(cm.len(), reach.len());
        let mut names: Vec<&str> = reach.iter().map(|o| o.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), reach.len(), "component names must be unique");
    }

    #[test]
    fn nosql20_components_match_static_table() {
        let doc = document().unwrap();
        let JsonValue::Object(cm) = get(get(&doc, "components"), "schemas") else {
            panic!("schemas not an object");
        };
        for o in reachable() {
            let c = cm.get(o.name).expect("component");
            assert_eq!(get(c, "type"), &s("object"));
            assert_eq!(get(c, "additionalProperties"), &JsonValue::Bool(false));
            let JsonValue::Object(props) = get(c, "properties") else {
                panic!("properties");
            };
            let op_entry = OP_SCHEMAS.iter().find(|(_, x)| std::ptr::eq(*x, o));
            let mut want: Vec<&str> = o.fields.iter().map(|f| f.key).collect();
            if op_entry.is_some() && !want.contains(&"op") {
                want.push("op");
            }
            want.sort_unstable();
            let got: Vec<&str> = props.keys().map(String::as_str).collect();
            assert_eq!(got, want);
            match op_entry {
                Some((n, _)) => assert_eq!(get(&props["op"], "const"), &s(n)),
                None => {
                    if let Some(p) = props.get("op") {
                        assert!(!matches!(p, JsonValue::Object(m) if m.contains_key("const")));
                    }
                }
            }
            let req: Vec<String> = o
                .fields
                .iter()
                .filter(|f| f.presence == Presence::Required)
                .map(|f| f.key.to_string())
                .collect();
            if req.is_empty() {
                assert!(!matches!(c, JsonValue::Object(m) if m.contains_key("required")));
            } else {
                assert_eq!(strs(get(c, "required")), req);
            }
        }
        for (n, o) in OP_SCHEMAS.iter() {
            let req = strs(get(&cm[o.name], "required"));
            assert!(req.contains(&"table".to_string()), "{n} requires table");
        }
    }

    const fn f(key: &'static str, presence: Presence, ty: FieldType, nullable: bool) -> FieldSpec {
        FieldSpec {
            key,
            presence,
            ty,
            nullable,
        }
    }

    static LEAF: ObjectSchema = ObjectSchema {
        name: "leaf",
        fields: &[f("x", Presence::Required, FieldType::String, false)],
    };

    static ALL: ObjectSchema = ObjectSchema {
        name: "all",
        fields: &[
            f("b", Presence::Required, FieldType::Bool, false),
            f("n", Presence::Optional, FieldType::Number, true),
            f("s", Presence::Optional, FieldType::String, false),
            f("sc", Presence::Optional, FieldType::Scalar, false),
            f(
                "a_any",
                Presence::Optional,
                FieldType::Array(ElementType::Any),
                false,
            ),
            f(
                "a_str",
                Presence::Optional,
                FieldType::Array(ElementType::String),
                false,
            ),
            f(
                "a_num",
                Presence::Optional,
                FieldType::Array(ElementType::Number),
                false,
            ),
            f(
                "a_obj",
                Presence::Optional,
                FieldType::Array(ElementType::Object(&LEAF)),
                false,
            ),
            f(
                "soa_str",
                Presence::Optional,
                FieldType::StringOrArray(ElementType::String),
                false,
            ),
            f(
                "soa_num",
                Presence::Optional,
                FieldType::StringOrArray(ElementType::Number),
                false,
            ),
            f("o", Presence::Optional, FieldType::Object(&LEAF), false),
            f("any", Presence::Optional, FieldType::AnyObject, false),
        ],
    };

    static CLASH: ObjectSchema = ObjectSchema {
        name: "leaf",
        fields: &[],
    };
    static USES_CLASH: ObjectSchema = ObjectSchema {
        name: "uses_clash",
        fields: &[f("c", Presence::Optional, FieldType::Object(&CLASH), false)],
    };
    static USES_LEAF_TWICE: ObjectSchema = ObjectSchema {
        name: "twice",
        fields: &[
            f("a", Presence::Optional, FieldType::Object(&LEAF), false),
            f("b", Presence::Optional, FieldType::Object(&LEAF), false),
        ],
    };

    fn parse(t: &str) -> JsonValue {
        parse_json(t).unwrap()
    }

    #[test]
    fn nosql20_mapping_covers_every_variant() {
        let doc = build(&[("all_op", &ALL)]).unwrap();
        let comps = get(get(&doc, "components"), "schemas");
        let comp = get(comps, "all").clone();
        let leaf_ref = r##"{"$ref":"#/components/schemas/leaf"}"##;
        let expected = parse(&format!(
            r#"{{"type":"object","additionalProperties":false,"required":["b"],
            "properties":{{
              "op":{{"type":"string","const":"all_op"}},
              "b":{{"type":"boolean"}},
              "n":{{"anyOf":[{{"type":"number"}},{{"type":"null"}}]}},
              "s":{{"type":"string"}},
              "sc":{{"type":["string","number","boolean"]}},
              "a_any":{{"type":"array","items":{{}}}},
              "a_str":{{"type":"array","items":{{"type":"string"}}}},
              "a_num":{{"type":"array","items":{{"type":"number"}}}},
              "a_obj":{{"type":"array","items":{leaf_ref}}},
              "soa_str":{{"oneOf":[{{"type":"string"}},{{"type":"array","items":{{"type":"string"}}}}]}},
              "soa_num":{{"type":"array","items":{{"type":"number"}}}},
              "o":{leaf_ref},
              "any":{{"type":"object"}}
            }}}}"#
        ));
        assert_eq!(comp, expected);
        // 入れ子コンポーネントに const は付かない。
        let leaf = get(comps, "leaf");
        assert!(!matches!(get(leaf, "properties"), JsonValue::Object(m) if m.contains_key("op")));
    }

    #[test]
    fn nosql20_duplicate_name_is_rejected_and_shared_ref_is_not() {
        assert_eq!(
            build(&[("u", &USES_CLASH), ("l", &LEAF)]),
            Err(OpenApiError::DuplicateComponentName { name: "leaf" })
        );
        let ok = build(&[("t", &USES_LEAF_TWICE), ("l", &LEAF)]).unwrap();
        let JsonValue::Object(cm) = get(get(&ok, "components"), "schemas") else {
            panic!("schemas");
        };
        assert_eq!(cm.len(), 2);
    }

    #[test]
    fn nosql20_render_is_deterministic() {
        assert_eq!(render_pretty().unwrap(), render_pretty().unwrap());
    }

    #[test]
    fn nosql20_write_pretty_roundtrips_special_values() {
        let mut m = BTreeMap::new();
        m.insert("q\"\\\u{1}é".to_string(), s("a\"b\\c\n\u{7}日本"));
        m.insert("empty_a".to_string(), JsonValue::Array(vec![]));
        m.insert("empty_o".to_string(), JsonValue::Object(BTreeMap::new()));
        m.insert(
            "nested".to_string(),
            JsonValue::Array(vec![
                JsonValue::Null,
                JsonValue::Bool(true),
                JsonValue::Number(JsonNumber::PosInt(3)),
            ]),
        );
        let v = JsonValue::Object(m);
        let mut out = String::new();
        write_pretty(&v, &mut out);
        assert_eq!(parse_json(&out).unwrap(), v);
        let pretty = render_pretty().unwrap();
        assert!(pretty.ends_with("}\n") && !pretty.ends_with("\n\n"));
    }
}
