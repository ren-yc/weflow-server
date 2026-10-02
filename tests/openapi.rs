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

    // 3.5 路径模板里的每个占位符必须有同名的 path 参数声明（OpenAPI 规范要求）。
    //    golden 快照记录的是「输出了什么」，不校验「描述是否合法」，所以缺声明的文档
    //    能带着空参数表一路绿进 golden —— 直到客户端生成器把整份文档当非法输入拒绝。
    for (path, item) in paths {
        let want: Vec<&str> = path
            .split('/')
            .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            .collect();
        if want.is_empty() {
            continue;
        }
        for (method, op) in item.as_object().expect("path item 必须是对象") {
            let declared: Vec<&str> = op["parameters"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|p| {
                            p["in"].as_str().filter(|loc| *loc == "path").and_then(|_| p["name"].as_str())
                        })
                        .collect()
                })
                .unwrap_or_default();
            for name in &want {
                assert!(
                    declared.contains(name),
                    "{method} {path} 的模板占位符 {{{name}}} 没有对应的 path 参数声明"
                );
            }
        }
    }

    // 4. 多形状端点必须用 `oneOf` —— 用「所有字段都可选」的单个 schema 会让描述看起来
    //    合法，而实际没有任何取值组合是对的。
    let accounts = &doc["paths"]["/api/v1/accounts"]["post"]["responses"]["200"]["content"]
        ["application/json"]["schema"];
    assert!(
        accounts["oneOf"].is_array(),
        "post /api/v1/accounts 是多形状端点（注册受理 / 占用冲突），应该用 oneOf，实际是 {accounts}"
    );

    // 5. 反过来：**单形状**端点不得用 `oneOf`。老面在 ChatLab 形状搬走之后只剩一种形状，
    //    这条断言把「两个形状又被塞回老面」变成红的 —— 只钉「多形状要用 oneOf」的话，
    //    形状变少是无声的。
    for (path, method) in [
        ("/api/v1/sessions", "get"),
        ("/api/v1/messages", "get"),
        ("/chatlab/messages", "get"),
    ] {
        let schema = &doc["paths"][path][method]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert!(
            schema["$ref"].is_string() && schema["oneOf"].is_null(),
            "{method} {path} 只有一种形状，应当直接引用单个 schema，实际是 {schema}"
        );
    }
}
