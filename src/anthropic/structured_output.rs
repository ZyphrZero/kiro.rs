//! Structured output (`output_config.format = json_schema`) emulation.
//!
//! The upstream has no native JSON-schema mode, so we instruct the model via the
//! system prompt and then pull the first conforming JSON value out of its reply.

use serde_json::Value;

/// 固定标记行，便于测试/调试识别
const MARKER: &str = "[Structured output requirement]";

/// Schema 递归深度上限，防止恶意 schema 打爆栈
const MAX_DEPTH: usize = 64;

/// 提取时最多尝试的失败解析次数，避免病态输入造成二次方开销
const MAX_FAILED_PARSES: usize = 2000;

/// System-prompt text instructing the model to answer with a single JSON value
/// conforming to `schema` (schema pretty-printed inside).
pub fn system_instruction(schema: &Value) -> String {
    let pretty = serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string());
    format!(
        "{MARKER}\n\
         Respond with ONLY a single JSON value that validates against the JSON Schema below. \
         Do not include any prose, explanation, markdown, or code fences, and nothing before \
         or after the JSON value.\n\
         \n\
         JSON Schema:\n\
         {pretty}"
    )
}

/// Minimal JSON Schema validation. Supports `type` (string or array), `required`,
/// `properties` (recursive), `additionalProperties`, `enum`, `const`, `items`
/// (single schema), `anyOf` / `oneOf` / `allOf`. Unknown keywords are ignored.
/// Exceeding the recursion depth guard counts as non-conforming.
pub fn conforms(schema: &Value, value: &Value) -> bool {
    conforms_at(schema, schema, value, 0)
}

fn conforms_at(root: &Value, schema: &Value, value: &Value, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let obj = match schema {
        Value::Bool(b) => return *b,
        Value::Object(o) => o,
        // 非 object/boolean 的 schema 视为接受一切
        _ => return true,
    };
    let sub = |s: &Value, v: &Value| conforms_at(root, s, v, depth + 1);

    // 本地 `$ref`（如 `#/$defs/X`）按 JSON Pointer 解析；远程/无法解析的引用忽略。
    if let Some(Value::String(reference)) = obj.get("$ref")
        && let Some(pointer) = reference.strip_prefix('#')
        && let Some(target) = root.pointer(pointer)
        && !sub(target, value)
    {
        return false;
    }

    if let Some(t) = obj.get("type") {
        match t {
            Value::String(name) if !type_matches(name, value) => return false,
            Value::Array(names)
                if !names
                    .iter()
                    .any(|n| n.as_str().is_some_and(|n| type_matches(n, value))) =>
            {
                return false;
            }
            _ => {}
        }
    }

    if let Some(Value::Array(options)) = obj.get("enum")
        && !options.iter().any(|o| json_eq(o, value))
    {
        return false;
    }
    if let Some(c) = obj.get("const")
        && !json_eq(c, value)
    {
        return false;
    }

    if let Value::Object(map) = value {
        if let Some(Value::Array(req)) = obj.get("required")
            && !req
                .iter()
                .filter_map(Value::as_str)
                .all(|k| map.contains_key(k))
        {
            return false;
        }
        let props = obj.get("properties").and_then(Value::as_object);
        if let Some(props) = props {
            for (k, ps) in props {
                if let Some(v) = map.get(k)
                    && !sub(ps, v)
                {
                    return false;
                }
            }
        }
        // 有 patternProperties 时无法在不引入正则的情况下判断“额外”属性，放行。
        let has_pattern_properties = obj.contains_key("patternProperties");
        match obj.get("additionalProperties") {
            _ if has_pattern_properties => {}
            Some(Value::Bool(false)) => {
                if map
                    .keys()
                    .any(|k| !props.is_some_and(|p| p.contains_key(k)))
                {
                    return false;
                }
            }
            Some(extra @ Value::Object(_)) => {
                for (k, v) in map {
                    if !props.is_some_and(|p| p.contains_key(k)) && !sub(extra, v) {
                        return false;
                    }
                }
            }
            _ => {}
        }
    }

    // tuple 形式的 items（数组）忽略
    if let Value::Array(arr) = value
        && let Some(items @ (Value::Object(_) | Value::Bool(_))) = obj.get("items")
        && !arr.iter().all(|v| sub(items, v))
    {
        return false;
    }

    if let Some(Value::Array(list)) = obj.get("allOf")
        && !list.iter().all(|s| sub(s, value))
    {
        return false;
    }
    if let Some(Value::Array(list)) = obj.get("anyOf")
        && !list.iter().any(|s| sub(s, value))
    {
        return false;
    }
    if let Some(Value::Array(list)) = obj.get("oneOf")
        && list.iter().filter(|s| sub(s, value)).count() != 1
    {
        return false;
    }

    true
}

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => match value {
            Value::Number(n) => {
                n.is_i64()
                    || n.is_u64()
                    || n.as_f64()
                        .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
            }
            _ => false,
        },
        _ => false,
    }
}

/// 结构相等，数值按 f64 比较（1 与 1.0 相等）
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if x == y {
                return true;
            }
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

/// Finds the first JSON value in `text` (scanning every `{` / `[`candidate, also
/// inside ```json fences) that parses AND conforms to `schema`; returns its compact
/// serialization.
///
/// A candidate that parses but does not conform is skipped as a whole (scanning
/// resumes after its end), so sub-objects nested inside a non-conforming outer
/// value are never picked. Scalar roots are only accepted when the whole trimmed
/// text (optionally wrapped in a code fence) is that JSON value.
pub fn extract_conforming_json(text: &str, schema: &Value) -> Option<String> {
    // 整段文本（或围栏内容）本身就是一个合法值，覆盖标量根
    let whole = strip_fence(text.trim());
    if let Ok(v) = serde_json::from_str::<Value>(whole)
        && conforms(schema, &v)
    {
        return serde_json::to_string(&v).ok();
    }

    let bytes = text.as_bytes();
    let mut pos = 0;
    let mut failed = 0;
    while pos < bytes.len() {
        // '{' 和 '[' 都是 ASCII，按字节定位必在 char 边界上
        let Some(rel) = bytes[pos..].iter().position(|&b| b == b'{' || b == b'[') else {
            break;
        };
        let start = pos + rel;
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        match stream.next() {
            Some(Ok(v)) => {
                if conforms(schema, &v) {
                    return serde_json::to_string(&v).ok();
                }
                pos = start + stream.byte_offset().max(1);
            }
            _ => {
                failed += 1;
                if failed >= MAX_FAILED_PARSES {
                    return None;
                }
                pos = start + 1;
            }
        }
    }
    None
}

/// 去掉最外层 ``` / ```json 围栏（若存在）
fn strip_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    let Some(rest) = rest.strip_suffix("```") else {
        return s;
    };
    // 跳过围栏首行的语言标记
    match rest.find('\n') {
        Some(i) => rest[i + 1..].trim(),
        None => rest.trim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn goal_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "ok": {"type": "boolean"},
                "reason": {"type": "string"},
                "impossible": {"type": "boolean"}
            },
            "required": ["ok", "reason"],
            "additionalProperties": false
        })
    }

    fn extract(text: &str) -> Option<String> {
        extract_conforming_json(text, &goal_schema())
    }

    // ---- system_instruction ----

    #[test]
    fn instruction_has_marker_and_pretty_schema() {
        let schema = goal_schema();
        let s = system_instruction(&schema);
        assert!(s.starts_with(MARKER));
        assert!(s.contains(&serde_json::to_string_pretty(&schema).unwrap()));
        assert!(s.contains("ONLY a single JSON value"));
        assert!(s.contains("code fences"));
    }

    // ---- conforms ----

    #[test]
    fn type_keyword() {
        assert!(conforms(&json!({"type": "object"}), &json!({})));
        assert!(!conforms(&json!({"type": "object"}), &json!([])));
        assert!(conforms(&json!({"type": "array"}), &json!([1])));
        assert!(!conforms(&json!({"type": "array"}), &json!({})));
        assert!(conforms(&json!({"type": "string"}), &json!("a")));
        assert!(!conforms(&json!({"type": "string"}), &json!(1)));
        assert!(conforms(&json!({"type": "boolean"}), &json!(false)));
        assert!(!conforms(&json!({"type": "boolean"}), &json!("false")));
        assert!(conforms(&json!({"type": "null"}), &json!(null)));
        assert!(!conforms(&json!({"type": "null"}), &json!(0)));
        assert!(conforms(&json!({"type": "number"}), &json!(1.5)));
        assert!(!conforms(&json!({"type": "number"}), &json!("1.5")));
        assert!(!conforms(&json!({"type": "bogus"}), &json!(1)));
    }

    #[test]
    fn type_array() {
        let s = json!({"type": ["string", "null"]});
        assert!(conforms(&s, &json!("x")));
        assert!(conforms(&s, &json!(null)));
        assert!(!conforms(&s, &json!(1)));
    }

    #[test]
    fn integer_vs_number() {
        let int = json!({"type": "integer"});
        assert!(conforms(&int, &json!(1)));
        assert!(conforms(&int, &json!(1.0)));
        assert!(conforms(&int, &json!(-7)));
        assert!(!conforms(&int, &json!(1.5)));
        assert!(!conforms(&int, &json!("1")));
        let num = json!({"type": "number"});
        assert!(conforms(&num, &json!(1)));
        assert!(conforms(&num, &json!(1.0)));
    }

    #[test]
    fn required_keyword() {
        let s = json!({"required": ["a", "b"]});
        assert!(conforms(&s, &json!({"a": 1, "b": 2, "c": 3})));
        assert!(!conforms(&s, &json!({"a": 1})));
        // 非 object 时忽略
        assert!(conforms(&s, &json!([1])));
    }

    #[test]
    fn properties_keyword() {
        let s = json!({"properties": {"a": {"type": "integer"}}});
        assert!(conforms(&s, &json!({})));
        assert!(conforms(&s, &json!({"a": 1, "other": "x"})));
        assert!(!conforms(&s, &json!({"a": "x"})));
        // 非 object 时忽略
        assert!(conforms(&s, &json!("str")));
    }

    #[test]
    fn nested_properties() {
        let s = json!({
            "type": "object",
            "properties": {
                "a": {
                    "type": "object",
                    "properties": {"b": {"type": "array", "items": {"type": "string"}}},
                    "required": ["b"]
                }
            },
            "required": ["a"]
        });
        assert!(conforms(&s, &json!({"a": {"b": ["x", "y"]}})));
        assert!(!conforms(&s, &json!({"a": {}})));
        assert!(!conforms(&s, &json!({"a": {"b": ["x", 1]}})));
        assert!(!conforms(&s, &json!({"a": 1})));
    }

    #[test]
    fn additional_properties_false() {
        let s = json!({"properties": {"a": {}}, "additionalProperties": false});
        assert!(conforms(&s, &json!({"a": 1})));
        assert!(conforms(&s, &json!({})));
        assert!(!conforms(&s, &json!({"a": 1, "b": 2})));
        // 无 properties 时任何 key 都是额外的
        let s = json!({"additionalProperties": false});
        assert!(conforms(&s, &json!({})));
        assert!(!conforms(&s, &json!({"x": 1})));
        // 非 object 值忽略
        assert!(conforms(&s, &json!([1])));
    }

    #[test]
    fn additional_properties_schema_and_true() {
        let s = json!({"properties": {"a": {}}, "additionalProperties": {"type": "integer"}});
        assert!(conforms(&s, &json!({"a": "anything", "b": 1})));
        assert!(!conforms(&s, &json!({"a": 1, "b": "x"})));
        let s = json!({"properties": {"a": {}}, "additionalProperties": true});
        assert!(conforms(&s, &json!({"a": 1, "b": "x"})));
        let s = json!({"properties": {"a": {}}});
        assert!(conforms(&s, &json!({"a": 1, "b": "x"})));
    }

    #[test]
    fn enum_keyword() {
        let s = json!({"enum": ["a", 1, null, {"k": [1]}]});
        assert!(conforms(&s, &json!("a")));
        assert!(conforms(&s, &json!(1)));
        assert!(conforms(&s, &json!(1.0)));
        assert!(conforms(&s, &json!(null)));
        assert!(conforms(&s, &json!({"k": [1]})));
        assert!(!conforms(&s, &json!("b")));
        assert!(!conforms(&s, &json!({"k": [2]})));
    }

    #[test]
    fn const_keyword() {
        let s = json!({"const": {"a": 1}});
        assert!(conforms(&s, &json!({"a": 1})));
        assert!(!conforms(&s, &json!({"a": 2})));
        assert!(!conforms(&json!({"const": "x"}), &json!("y")));
        assert!(conforms(&json!({"const": null}), &json!(null)));
    }

    #[test]
    fn items_keyword() {
        let s = json!({"type": "array", "items": {"type": "number"}});
        assert!(conforms(&s, &json!([])));
        assert!(conforms(&s, &json!([1, 2.5])));
        assert!(!conforms(&s, &json!([1, "x"])));
        // tuple 形式忽略
        let s = json!({"items": [{"type": "string"}]});
        assert!(conforms(&s, &json!([1, 2])));
        // 非数组忽略
        let s = json!({"items": {"type": "string"}});
        assert!(conforms(&s, &json!({"a": 1})));
    }

    #[test]
    fn any_one_all_of() {
        let any = json!({"anyOf": [{"type": "string"}, {"type": "integer"}]});
        assert!(conforms(&any, &json!("x")));
        assert!(conforms(&any, &json!(2)));
        assert!(!conforms(&any, &json!(true)));

        // 1 同时匹配 number 与 integer -> oneOf 失败
        let one = json!({"oneOf": [{"type": "number"}, {"type": "integer"}]});
        assert!(!conforms(&one, &json!(1)));
        assert!(conforms(&one, &json!(1.5)));
        assert!(!conforms(&one, &json!("x")));

        let all = json!({"allOf": [{"type": "object"}, {"required": ["a"]}]});
        assert!(conforms(&all, &json!({"a": 1})));
        assert!(!conforms(&all, &json!({})));
    }

    #[test]
    fn boolean_and_trivial_schemas() {
        assert!(conforms(&json!(true), &json!({"x": 1})));
        assert!(!conforms(&json!(false), &json!(null)));
        assert!(conforms(&json!({}), &json!([1, 2])));
        assert!(conforms(&json!("weird"), &json!(1)));
        assert!(conforms(&json!(null), &json!(1)));
        // 属性 schema 为 false：出现即失败
        let s = json!({"properties": {"a": false}});
        assert!(conforms(&s, &json!({})));
        assert!(!conforms(&s, &json!({"a": 1})));
    }

    #[test]
    fn unknown_keywords_ignored() {
        let s = json!({"type": "string", "minLength": 100, "format": "email", "x-foo": 1});
        assert!(conforms(&s, &json!("a")));
    }

    #[test]
    fn local_ref_and_defs() {
        let schema = json!({
            "$ref": "#/$defs/R",
            "$defs": {
                "R": {
                    "type": "object",
                    "properties": {"inner": {"$ref": "#/$defs/I"}},
                    "required": ["inner"]
                },
                "I": {"type": "string"}
            }
        });
        assert!(conforms(&schema, &json!({"inner": "x"})));
        assert!(!conforms(&schema, &json!({"inner": 1})));
        assert!(!conforms(&schema, &json!({})));
        assert!(!conforms(&schema, &json!([1])));
        // 无法解析的引用被忽略
        assert!(conforms(&json!({"$ref": "#/$defs/missing"}), &json!(1)));
        assert!(conforms(
            &json!({"$ref": "https://example.com/s.json"}),
            &json!(1)
        ));
        // 不会因自引用 schema 栈溢出
        assert!(!conforms(&json!({"$ref": "#"}), &json!(1)));
        assert_eq!(
            extract_conforming_json("see [1] and {\"note\":1} then {\"inner\":\"ok\"}", &schema)
                .as_deref(),
            Some("{\"inner\":\"ok\"}")
        );
    }

    #[test]
    fn pattern_properties_relaxes_additional_properties() {
        let schema = json!({
            "type": "object",
            "patternProperties": {"^x-": {"type": "string"}},
            "additionalProperties": false
        });
        assert!(conforms(&schema, &json!({"x-a": "v"})));
    }

    #[test]
    fn recursion_guard() {
        // 构造远超深度上限的嵌套 schema 和对应 value
        let depth = 500;
        let mut schema = json!({"type": "integer"});
        let mut value = json!(1);
        for _ in 0..depth {
            schema = json!({"type": "array", "items": schema});
            value = json!([value]);
        }
        // 不 panic / 不栈溢出，超限视为不符合
        assert!(!conforms(&schema, &value));
        // 浅层仍然正常
        let mut schema = json!({"type": "integer"});
        let mut value = json!(1);
        for _ in 0..10 {
            schema = json!({"type": "array", "items": schema});
            value = json!([value]);
        }
        assert!(conforms(&schema, &value));
    }

    // ---- extract_conforming_json ----

    #[test]
    fn extract_pure_json() {
        assert_eq!(
            extract(r#"{"ok":true,"reason":"done"}"#).as_deref(),
            Some(r#"{"ok":true,"reason":"done"}"#)
        );
        // 紧凑化
        assert_eq!(
            extract("  {\n \"ok\": false,\n \"reason\": \"no\"\n}\n").as_deref(),
            Some(r#"{"ok":false,"reason":"no"}"#)
        );
    }

    #[test]
    fn extract_after_prose_openings() {
        for prefix in [
            "The condition is not met yet. ",
            "I'm checking the files first. ",
            "The core condition holds.\n\n",
            "The core con",
        ] {
            let text = format!(r#"{prefix}{{"ok":true,"reason":"done"}}"#);
            assert_eq!(
                extract(&text).as_deref(),
                Some(r#"{"ok":true,"reason":"done"}"#),
                "prefix: {prefix:?}"
            );
        }
    }

    #[test]
    fn extract_none_without_json() {
        assert_eq!(extract("Nothing structured here."), None);
        assert_eq!(extract(""), None);
        assert_eq!(extract("   \n"), None);
        assert_eq!(extract("{ not json"), None);
        assert_eq!(extract("[unclosed"), None);
        assert_eq!(extract("}{]["), None);
    }

    #[test]
    fn extract_skips_nonconforming_then_picks_conforming() {
        let text = r#"First try: {"ok":"yes"} but actually {"ok":true,"reason":"fine"}"#;
        assert_eq!(
            extract(text).as_deref(),
            Some(r#"{"ok":true,"reason":"fine"}"#)
        );
        // 缺 required 与多余字段同样被跳过
        let text = r#"{"ok":true} {"ok":true,"reason":"r","extra":1} {"ok":false,"reason":"z"}"#;
        assert_eq!(
            extract(text).as_deref(),
            Some(r#"{"ok":false,"reason":"z"}"#)
        );
    }

    #[test]
    fn extract_from_code_fence() {
        let text = "Here is the result:\n```json\n{\"ok\": true, \"reason\": \"all good\"}\n```\nHope that helps!";
        assert_eq!(
            extract(text).as_deref(),
            Some(r#"{"ok":true,"reason":"all good"}"#)
        );
        let text = "```\n{\"ok\": true, \"reason\": \"x\"}\n```";
        assert_eq!(
            extract(text).as_deref(),
            Some(r#"{"ok":true,"reason":"x"}"#)
        );
    }

    #[test]
    fn extract_braces_inside_strings() {
        let text = r#"Result: {"ok":true,"reason":"use {x} and } and \" quote {"} trailing"#;
        let got = extract(text).expect("should extract");
        let v: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["reason"], "use {x} and } and \" quote {");
    }

    #[test]
    fn extract_with_stray_braces_in_prose() {
        let text = r#"I saw a { in the file and a [ too, e.g. [note]. {"ok":true,"reason":"r"}"#;
        assert_eq!(
            extract(text).as_deref(),
            Some(r#"{"ok":true,"reason":"r"}"#)
        );
    }

    #[test]
    fn extract_does_not_pick_nested_from_nonconforming_outer() {
        // 外层不符合（多余字段 wrapper），内层符合但不应被选中
        let text = r#"{"wrapper":{"ok":true,"reason":"inner"}}"#;
        assert_eq!(extract(text), None);
        let text = r#"{"ok":true,"reason":"r","extra":{"ok":true,"reason":"inner"}}"#;
        assert_eq!(extract(text), None);
        // 外层是数组同理
        let text = r#"[{"ok":true,"reason":"inner"}]"#;
        assert_eq!(extract(text), None);
    }

    #[test]
    fn extract_unicode_and_emoji() {
        let text =
            "完成了 🎉 条件已满足。\n{\"ok\":true,\"reason\":\"文件已创建 ✅ {ok}\"}\n谢谢 😀";
        let got = extract(text).expect("should extract");
        let v: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["reason"], "文件已创建 ✅ {ok}");
        // 多字节字符紧邻 '{' / '['
        assert_eq!(extract("日本語{[é😀{"), None);
        assert!(extract("😀{\"ok\":true,\"reason\":\"😀\"}😀").is_some());
    }

    #[test]
    fn extract_array_root_and_scalar_root() {
        let arr = json!({"type": "array", "items": {"type": "integer"}});
        assert_eq!(
            extract_conforming_json("Numbers: [1, 2, 3] ok", &arr).as_deref(),
            Some("[1,2,3]")
        );
        let s = json!({"type": "string"});
        assert_eq!(
            extract_conforming_json(" \"hello\" ", &s).as_deref(),
            Some("\"hello\"")
        );
        // 标量不从 prose 中提取
        assert_eq!(extract_conforming_json("say \"hello\" now", &s), None);
        let n = json!({"type": "integer"});
        assert_eq!(extract_conforming_json("42", &n).as_deref(), Some("42"));
        assert_eq!(
            extract_conforming_json("```json\n42\n```", &n).as_deref(),
            Some("42")
        );
        assert_eq!(extract_conforming_json("the answer is 42", &n), None);
        let b = json!({"type": "boolean"});
        assert_eq!(extract_conforming_json("true", &b).as_deref(), Some("true"));
    }

    #[test]
    fn extract_pathological_inputs_finish_fast() {
        let start = std::time::Instant::now();
        assert_eq!(extract(&"{".repeat(100_000)), None);
        assert_eq!(extract(&"[".repeat(100_000)), None);
        assert_eq!(extract(&"{\"a\":".repeat(50_000)), None);
        assert_eq!(extract(&"{[".repeat(50_000)), None);
        assert_eq!(extract(&"[1,".repeat(30_000)), None);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn extract_many_nonconforming_values_is_linear() {
        let mut text = r#"{"ok":1} "#.repeat(20_000);
        text.push_str(r#"{"ok":true,"reason":"end"}"#);
        assert_eq!(
            extract(&text).as_deref(),
            Some(r#"{"ok":true,"reason":"end"}"#)
        );
    }
}
