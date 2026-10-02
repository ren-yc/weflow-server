//! Regenerate `clients/rust/src/generated/gen.rs` from the server's OpenAPI
//! description. The generated file is committed; CI reruns this binary with
//! `--check` and asserts no diff, so a type can never silently drift from the
//! description it claims to describe.
//!
//! The description needs mechanical normalization before a generator can
//! consume it, all applied here in one deterministic pass:
//!
//! 1. We call the server's `document()` directly instead of reading the golden
//!    snapshot: the snapshot masks volatile values with placeholder strings and
//!    wraps the body, neither of which is valid description.
//! 2. The description is OpenAPI 3.1; the generator's schema parser is 3.0.x,
//!    so `type: [T, null]` becomes `T` plus `nullable: true`.
//! 3. utoipa spells `Option<T>` as `oneOf: [T, {type: null}]`; the null arm is
//!    dropped - the handlers omit the key entirely when absent, so plain `T`
//!    with a missing key is the semantics the wire actually has.
//! 4. Path-template placeholders must carry matching `parameters` declarations.
//!    The server emits them now; the rewrite is kept here so regeneration also
//!    works against a description fetched over HTTP.
//!
//! The no-diff gate therefore pins the whole normalize+generate pipeline, not
//! the raw bytes the server emits. That is the intent: generated types must
//! track the description's semantics, and this file is what makes the
//! transformation reproducible rather than folklore.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("clients/regen sits two levels below the repo root");
    let out = repo.join("clients/rust/src/generated/gen.rs");

    let doc = weflow_server::server::openapi::document();
    let mut value = serde_json::to_value(&doc).expect("description must serialize");
    normalize(&mut value);
    // The generator's parser only accepts 3.0.x; every construct we emit maps 1:1
    // onto a 3.0 feature, so the version stamp is the only remaining 3.1 marker.
    if let Some(obj) = value.as_object_mut() {
        obj.insert("openapi".into(), serde_json::json!("3.0.3"));
    }
    if std::env::args().any(|a| a == "--dump-spec") {
        // For sibling generators: the Python regen reads this instead of the
        // golden snapshot, whose placeholder masking would burn the wrong
        // types (a volatile integer property restored as string) into the
        // generated models.
        println!("{}", serde_json::to_string_pretty(&value).expect("spec must serialize"));
        return;
    }
    let spec: openapiv3::OpenAPI =
        serde_json::from_value(value).expect("normalized description must parse as OpenAPI 3.0");

    let mut settings = progenitor_impl::GenerationSettings::new();
    settings.with_interface(progenitor_impl::InterfaceStyle::Positional);
    let mut generator = progenitor_impl::Generator::new(&settings);
    let tokens = generator
        .generate_tokens(&spec)
        .expect("generation must succeed on the normalized description");
    let file: syn::File =
        syn::parse2(tokens).expect("generated tokens must parse as a Rust file");
    let code = prettyplease::unparse(&file);

    if std::env::args().any(|a| a == "--check") {
        let existing = std::fs::read_to_string(&out).unwrap_or_default();
        if existing != code {
            eprintln!(
                "generated client is stale: {} differs from a fresh run; rerun `cargo run -p weflow-regen` and commit",
                out.display()
            );
            std::process::exit(1);
        }
        println!("generated client is up to date: {}", out.display());
    } else {
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(&out, &code).expect("write generated client");
        println!(
            "wrote {} ({} bytes, {} types)",
            out.display(),
            code.len(),
            generator.get_type_space().iter_types().count()
        );
    }
}

/// 3.1-to-3.0 normalization, in place. Deterministic: same input, same output.
fn normalize(node: &mut serde_json::Value) {
    match node {
        serde_json::Value::Array(items) => items.iter_mut().for_each(normalize),
        serde_json::Value::Object(map) => {
            let mut nullable = false;
            if let Some(serde_json::Value::Array(types)) = map.get_mut("type") {
                if types.len() == 2 {
                    if let Some(pos) = types.iter().position(|t| t == "null") {
                        types.remove(pos);
                        nullable = true;
                    }
                }
                if let [only] = types.as_slice() {
                    let only = only.clone();
                    map.insert("type".into(), only);
                }
            }
            if nullable {
                map.insert("nullable".into(), serde_json::Value::Bool(true));
            }
            if let Some(serde_json::Value::Array(arms)) = map.get_mut("oneOf") {
                arms.retain(|arm| arm.get("type") != Some(&serde_json::json!("null")));
                if arms.len() == 1 {
                    let only = arms.remove(0);
                    map.remove("oneOf");
                    if let serde_json::Value::Object(inner) = only {
                        for (k, v) in inner {
                            map.insert(k, v);
                        }
                    }
                }
            }
            for (_k, v) in map.iter_mut() {
                normalize(v);
            }
        }
        _ => {}
    }
}
