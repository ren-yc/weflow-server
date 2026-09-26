//! `/openapi.json` 的**机器校验**。
//!
//! 为什么需要它：把描述生成出来只能证明「它生成了」，不能证明「它是对的」。一段引用了
//! 不存在 schema 的 `$ref`，在浏览器里表现为一个空块 —— 没人会为此开 issue，但生成出来
//! 的客户端会缺少整个类型。
//!
//! 这里只管**描述自身的完整性**（引用能否解析、operationId 是否唯一、每条路径是否声明了
//! 成功响应）。描述与**真实响应**是否一致由 golden 快照负责；路由本身（`GET /openapi.json`
//! 能否读到、免不免鉴权）也在 golden 里 —— 那边记的就是真实响应。两者互补。

use serde_json::Value;

fn document() -> Value {
    let doc = weflow_server::server::openapi::document();
    serde_json::to_value(&doc).expect("描述必须可序列化")
}

/// 收集文档里所有 `$ref` 字符串。
fn collect_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k == "$ref" {
                    if let Some(s) = val.as_str() {
                        out.push(s.to_string());
                    }
                } else {
                    collect_refs(val, out);
                }
            }
        }
        Value::Array(items) => {
            for it in items {
                collect_refs(it, out);
            }
        }
        _ => {}
    }
}

#[test]
fn openapi_document_is_self_consistent() {
    let doc = document();

    // 1. 顶层结构。
    assert!(doc.get("openapi").is_some(), "缺 openapi 版本字段");
    assert!(doc["info"]["title"].is_string(), "缺 info.title");
    assert!(doc["info"]["version"].is_string(), "缺 info.version");

    let schemas = doc["components"]["schemas"]
        .as_object()
        .expect("必须有 components.schemas");
    assert!(!schemas.is_empty(), "schema 列表为空 —— 是不是忘了登记 DTO？");

    // 2. 每个 `$ref` 都能解析。**这是本文件存在的主要理由**：引用了不存在的 schema，
    //    浏览器里只显示一个空块，不会有人报错。
    let mut refs = Vec::new();
    collect_refs(&doc, &mut refs);
    assert!(!refs.is_empty(), "文档里没有任何引用，说明 paths 没接上 schema");
    for r in &refs {
        let name = r
            .strip_prefix("#/components/schemas/")
            .unwrap_or_else(|| panic!("只支持指向 components.schemas 的引用，遇到 {r}"));
        assert!(schemas.contains_key(name), "引用了解析不了的 schema：{name}");
    }

    // 3. 每条路径至少有一个操作，且操作声明了成功响应；operationId 唯一。
    let paths = doc["paths"].as_object().expect("必须有 paths");
    assert!(!paths.is_empty(), "paths 为空");
    let mut ids: Vec<String> = Vec::new();
    for (path, item) in paths {
        let item = item.as_object().expect("path item 必须是对象");
        assert!(!item.is_empty(), "{path} 没有任何方法");
        for (method, op) in item {
            let id = op["operationId"]
                .as_str()
                .unwrap_or_else(|| panic!("{method} {path} 缺 operationId"));
            assert!(!ids.iter().any(|x| x == id), "operationId 重复：{id}");
            ids.push(id.to_string());
            assert!(
                op["responses"]["200"].is_object(),
                "{method} {path} 没声明 200 响应"
            );
        }
    }

    // 4. 多形状端点必须用 `oneOf` —— 用「所有字段都可选」的单个 schema 会让描述看起来
    //    合法，而实际没有任何取值组合是对的。
    for (path, method) in [
        ("/api/v1/sessions", "get"),
        ("/api/v1/messages", "get"),
        ("/api/v1/accounts", "post"),
    ] {
        let schema = &doc["paths"][path][method]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert!(
            schema["oneOf"].is_array(),
            "{method} {path} 是多形状端点，应该用 oneOf，实际是 {schema}"
        );
    }
}
