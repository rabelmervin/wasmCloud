//! Standalone test for the compiler host plugin pipeline.
//!
//! Reads real betty_models.sql + betty_model_properties.sql, generates seaORM
//! entity files, runs wash build, and pushes to OCI registry.
//!
//! Usage:
//!   set GRAPHILY_REPO_ROOT=C:\Users\rabel mervin\static-graphily
//!   set REGISTRY_URL=http://localhost:5001
//!   set IMAGE_TAG=test
//!   set SKIP_DEPLOY=1
//!   set MODELS_SQL=C:\Users\rabel mervin\static-graphily\mail-billing-slips\mail-billing-slips\betty_models.sql
//!   set PROPS_SQL=C:\Users\rabel mervin\static-graphily\mail-billing-slips\mail-billing-slips\betty_model_properties.sql
//!   cargo run --bin test_compile

use std::collections::HashMap;

fn main() {
    let repo_root = std::env::var("GRAPHILY_REPO_ROOT")
        .unwrap_or_else(|_| ".".to_string());
    let registry_url = std::env::var("REGISTRY_URL")
        .unwrap_or_else(|_| "http://localhost:5001".to_string());
    let image_tag = std::env::var("IMAGE_TAG")
        .unwrap_or_else(|_| "test".to_string());

    let models_sql_path = std::env::var("MODELS_SQL").unwrap_or_else(|_| {
        format!(
            r"{}\mail-billing-slips\mail-billing-slips\betty_models.sql",
            repo_root
        )
    });
    let props_sql_path = std::env::var("PROPS_SQL").unwrap_or_else(|_| {
        format!(
            r"{}\mail-billing-slips\mail-billing-slips\betty_model_properties.sql",
            repo_root
        )
    });

    println!("=== Graphily Compiler Plugin Test ===");
    println!("  repo_root   : {}", repo_root);
    println!("  registry    : {}", registry_url);
    println!("  image_tag   : {}", image_tag);
    println!("  models sql  : {}", models_sql_path);
    println!("  props sql   : {}", props_sql_path);
    println!();

    // ── Read SQL files ────────────────────────────────────────────────────────
    let models_sql = std::fs::read_to_string(&models_sql_path)
        .unwrap_or_else(|e| { eprintln!("✗ Cannot read {}: {}", models_sql_path, e); std::process::exit(1); });
    let props_sql = std::fs::read_to_string(&props_sql_path)
        .unwrap_or_else(|e| { eprintln!("✗ Cannot read {}: {}", props_sql_path, e); std::process::exit(1); });

    // Concatenate — the parser looks for both tables in one SQL string
    let combined_sql = format!("{}\n{}", models_sql, props_sql);
    let cleaned = clean_sql(&combined_sql);

    // ── Parse schema ─────────────────────────────────────────────────────────
    println!("[1/5] Parsing SQL files ...");
    let schema = match build_schema(&cleaned) {
        Ok(s) => s,
        Err(e) => { eprintln!("✗ Parse failed: {}", e); std::process::exit(1); }
    };
    println!("      Found {} models:", schema.models.len());
    let mut names: Vec<&str> = schema.models.keys().map(|s| s.as_str()).collect();
    names.sort();
    for name in &names {
        let m = &schema.models[*name];
        println!("        {} → table: {}, props: {}", name, m.table_name, m.properties.len());
    }
    println!();

    // ── Generate entity files ─────────────────────────────────────────────────
    println!("[2/5] Generating entity .rs files ...");
    let mut module_names: Vec<String> = schema.models.values()
        .map(|m| m.table_name.clone())
        .collect();
    module_names.sort();

    let mut entity_files: Vec<(String, String)> = Vec::new();
    for model in schema.models.values() {
        let content = generate_entity_rs(model);
        println!("      {}.rs ({} bytes)", model.table_name, content.len());
        entity_files.push((format!("{}.rs", model.table_name), content));
    }
    entity_files.push(("mod.rs".to_string(), generate_mod_rs(&module_names)));
    entity_files.push(("prelude.rs".to_string(), generate_prelude_rs(&module_names)));
    println!("      mod.rs + prelude.rs");
    println!();

    // ── Run executor ─────────────────────────────────────────────────────────
    println!("[3/5] Running executor (write → wash build → oras push) ...");
    match graphily_compiler_provider::executor::compile_and_deploy(
        &entity_files,
        &repo_root,
        &registry_url,
        &image_tag,
    ) {
        Ok(msg) => println!("\n✓ {}\n", msg),
        Err(e)  => { eprintln!("\n✗ Failed: {}\n", e); std::process::exit(1); }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Schema types
// ══════════════════════════════════════════════════════════════════════════════

struct Schema {
    models: HashMap<String, Model>,
}

struct Model {
    table_name: String,
    properties: Vec<Property>,
}

struct Property {
    name: String,
    kind: String,
    required: bool,
    references: Option<String>,   // camelCase model name
    foreign_key: Option<String>,
    bridge_table: Option<String>,
}

// ══════════════════════════════════════════════════════════════════════════════
// SQL parsing (mirrors json-compiler logic exactly)
// ══════════════════════════════════════════════════════════════════════════════

fn clean_sql(sql: &str) -> String {
    sql.lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty()
                && !t.starts_with("--")
                && !t.starts_with('#')
                && !t.starts_with("/*")
                && !t.starts_with("SET ")
                && !t.starts_with("/*!")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_schema(sql: &str) -> Result<Schema, String> {
    let model_rows = parse_inserts(sql, "betty_models");
    let mut models_by_id: HashMap<String, (String, String)> = HashMap::new(); // id → (table_name, display_name)
    for row in &model_rows {
        let id = row.get("id").cloned().unwrap_or_default();
        let table_name = row.get("table_name").cloned().unwrap_or_default();
        let name = row.get("name").cloned().unwrap_or_else(|| table_name.clone());
        if !id.is_empty() && !table_name.is_empty() {
            models_by_id.insert(id, (table_name, name));
        }
    }

    // Build uuid → camelCase display name map (for relationship resolution)
    let uuid_to_camel: HashMap<String, String> = models_by_id
        .iter()
        .map(|(id, (_, display))| (id.clone(), snake_to_camel(display)))
        .collect();

    let prop_rows = parse_inserts(sql, "betty_model_properties");
    let mut props_by_model: HashMap<String, Vec<Property>> = HashMap::new();

    for row in prop_rows {
        let model_id = row.get("model_id").cloned().unwrap_or_default();
        let name = row.get("name").cloned().unwrap_or_default();
        let kind = row.get("kind").cloned().unwrap_or_default();
        let options = row.get("options").cloned().filter(|s| !s.is_empty());

        if model_id.is_empty() || name.is_empty() { continue; }

        let is_rel = matches!(kind.as_str(), "belongs_to" | "has_many" | "has_and_belongs_to_many");
        let (foreign_key, references, bridge_table) = if is_rel {
            if let Some(opts) = &options {
                let (fk, ref_uuid) = extract_relation_options(opts);
                let ref_name = ref_uuid.as_deref()
                    .and_then(|u| uuid_to_camel.get(u))
                    .cloned()
                    .or(ref_uuid);
                let bridge = if kind == "has_and_belongs_to_many" {
                    let table = models_by_id.get(&model_id).map(|(t, _)| t.as_str()).unwrap_or("");
                    ref_name.as_ref().map(|r| format!("{}_{}_list", table, r.to_lowercase()))
                } else { None };
                (fk, ref_name, bridge)
            } else { (None, None, None) }
        } else { (None, None, None) };

        props_by_model.entry(model_id).or_default().push(Property {
            name,
            kind,
            required: false,
            references,
            foreign_key,
            bridge_table,
        });
    }

    let mut models = HashMap::new();
    for (id, (table_name, _display)) in models_by_id {
        let properties = props_by_model.remove(&id).unwrap_or_default();
        models.insert(table_name.clone(), Model { table_name, properties });
    }

    Ok(Schema { models })
}

fn extract_relation_options(opts: &str) -> (Option<String>, Option<String>) {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(opts) {
        let fk  = v.get("foreign_key").and_then(|x| x.as_str()).map(String::from);
        let mdl = v.get("model").and_then(|x| x.as_str()).map(String::from);
        return (fk, mdl);
    }
    (None, None)
}

fn parse_inserts(sql: &str, table: &str) -> Vec<HashMap<String, String>> {
    let needle = format!("INSERT INTO `{}`", table).to_uppercase();
    let needle2 = format!("INSERT INTO {}", table).to_uppercase();
    let mut result = Vec::new();
    let mut stmt = String::new();
    let mut in_insert = false;

    for line in sql.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        if !in_insert {
            let upper = trimmed.to_uppercase();
            if upper.starts_with(&needle) || upper.starts_with(&needle2) {
                in_insert = true;
                stmt = trimmed.to_string();
            } else { continue; }
        } else {
            stmt.push(' ');
            stmt.push_str(trimmed);
        }

        if stmt.trim_end().ends_with(';') {
            let columns = parse_column_names(&stmt);
            if !columns.is_empty() {
                if let Some(pos) = stmt.to_uppercase().find("VALUES") {
                    result.extend(extract_tuples(&stmt[pos + 6..].trim_start(), &columns));
                }
            }
            stmt.clear();
            in_insert = false;
        }
    }

    // handle statement with no trailing semicolon
    if in_insert && !stmt.is_empty() {
        let columns = parse_column_names(&stmt);
        if !columns.is_empty() {
            if let Some(pos) = stmt.to_uppercase().find("VALUES") {
                result.extend(extract_tuples(&stmt[pos + 6..].trim_start(), &columns));
            }
        }
    }

    result
}

fn parse_column_names(line: &str) -> Vec<String> {
    if let (Some(s), Some(e)) = (line.find('('), line.find(')')) {
        line[s + 1..e].split(',')
            .map(|c| c.trim().trim_matches('`').to_string())
            .collect()
    } else { vec![] }
}

fn extract_tuples(after_values: &str, columns: &[String]) -> Vec<HashMap<String, String>> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = None;
    let mut in_quote = false;
    let chars: Vec<(usize, char)> = after_values.char_indices().collect();
    let mut i = 0;

    while i < chars.len() {
        let (idx, ch) = chars[i];
        match ch {
            '\\' if in_quote => { i += 1; }
            '\'' => { in_quote = !in_quote; }
            '(' if !in_quote => {
                depth += 1;
                if depth == 1 { start = Some(idx + 1); }
            }
            ')' if !in_quote => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        let tuple_str = &after_values[s..idx];
                        let values = split_values(tuple_str);
                        if values.len() == columns.len() {
                            let mut row = HashMap::new();
                            for (col, val) in columns.iter().zip(values.iter()) {
                                row.insert(col.clone(), unquote_value(val));
                            }
                            result.push(row);
                        }
                        start = None;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    result
}

fn split_values(row: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut chars = row.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if in_quote => { current.push(ch); if let Some(n) = chars.next() { current.push(n); } }
            '\'' => { in_quote = !in_quote; current.push(ch); }
            ',' if !in_quote => { values.push(current.trim().to_string()); current = String::new(); }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() { values.push(current.trim().to_string()); }
    values
}

fn unquote_value(s: &str) -> String {
    let s = s.trim();
    // X'...' hex notation (MySQL binary UUIDs) — keep as-is (hex string)
    if (s.starts_with("X'") || s.starts_with("x'")) && s.ends_with('\'') {
        return s[2..s.len() - 1].to_string();
    }
    // regular quoted string
    if s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1]
            .replace("\\'", "'")
            .replace("\\\\", "\\")
            .replace("\\n", "\n")
            .replace("\\r", "\r");
    }
    if s.eq_ignore_ascii_case("NULL") { return String::new(); }
    s.to_string()
}

fn snake_to_camel(s: &str) -> String {
    let mut result = String::new();
    let mut cap_next = false;
    for (i, ch) in s.chars().enumerate() {
        if ch == '_' { cap_next = true; }
        else if cap_next || i == 0 {
            if i == 0 { result.extend(ch.to_uppercase()); } else { result.extend(ch.to_uppercase()); }
            cap_next = false;
        } else { result.push(ch); }
    }
    result
}

// ══════════════════════════════════════════════════════════════════════════════
// Entity file generation (mirrors schema-compiler logic exactly)
// ══════════════════════════════════════════════════════════════════════════════

fn map_kind_to_rust(kind: &str) -> &'static str {
    match kind {
        "string" | "text" | "email_address" | "url" | "phone_number"
        | "iban" | "password" | "rich_text" | "list" | "file" | "image" => "String",
        "integer" => "i32",
        "boolean" => "bool",
        "date" => "Date",
        "date_time" | "datetime" => "DateTime",
        "decimal" | "price" => "Decimal",
        "time" => "Time",
        _ => "String",
    }
}

fn is_relationship(kind: &str) -> bool {
    matches!(kind, "belongs_to" | "has_many" | "has_and_belongs_to_many")
}

fn pascal_case(s: &str) -> String {
    s.split('_').map(|w| {
        let mut c = w.chars();
        match c.next() {
            None => String::new(),
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        }
    }).collect()
}

fn camel_to_snake(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 { out.push('_'); }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

fn generate_entity_rs(model: &Model) -> String {
    let mut code = format!(
        "//! `SeaORM` Entity, @generated by graphily-compiler\n\nuse sea_orm::entity::prelude::*;\n\n\
         #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]\n\
         #[sea_orm(table_name = \"{}\")]\npub struct Model {{\n",
        model.table_name
    );

    // always emit id first
    code.push_str("    #[sea_orm(primary_key)]\n    pub id: i32,\n");

    for prop in model.properties.iter().filter(|p| !is_relationship(&p.kind) && p.name != "id") {
        let rust_type = map_kind_to_rust(&prop.kind);
        let field_type = if prop.required { rust_type.to_string() } else { format!("Option<{}>", rust_type) };
        code.push_str(&format!("    pub {}: {},\n", prop.name, field_type));
    }
    code.push_str("}\n\n");

    let rel_props: Vec<&Property> = model.properties.iter().filter(|p| is_relationship(&p.kind)).collect();

    code.push_str("#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]\npub enum Relation {\n");
    for prop in &rel_props {
        let to_module = camel_to_snake(prop.references.as_deref().unwrap_or("unknown"));
        let variant = pascal_case(&to_module);
        match prop.kind.as_str() {
            "belongs_to" => {
                let fk = pascal_case(prop.foreign_key.as_deref().unwrap_or(&prop.name));
                code.push_str(&format!(
                    "    #[sea_orm(belongs_to = \"super::{}::Entity\", from = \"Column::{}\", to = \"super::{}::Column::Id\")]\n    {},\n",
                    to_module, fk, to_module, variant
                ));
            }
            "has_many" => {
                code.push_str(&format!(
                    "    #[sea_orm(has_many = \"super::{}::Entity\")]\n    {},\n",
                    to_module, variant
                ));
            }
            "has_and_belongs_to_many" => {
                code.push_str(&format!(
                    "    #[sea_orm(has_many = \"super::{}::Entity\")] // many-to-many via {}\n    {},\n",
                    to_module, prop.bridge_table.as_deref().unwrap_or("junction_table"), variant
                ));
            }
            _ => {}
        }
    }
    code.push_str("}\n\n");

    for prop in &rel_props {
        let to_module = camel_to_snake(prop.references.as_deref().unwrap_or("unknown"));
        let variant = pascal_case(&to_module);
        if prop.kind == "has_and_belongs_to_many" {
            code.push_str(&format!("// TODO: Related<super::{}::Entity> requires junction table\n\n", to_module));
        } else {
            code.push_str(&format!(
                "impl Related<super::{}::Entity> for Entity {{\n    fn to() -> RelationDef {{ Relation::{}::def() }}\n}}\n\n",
                to_module, variant
            ));
        }
    }
    code.push_str("impl ActiveModelBehavior for ActiveModel {}\n");
    code
}

fn generate_mod_rs(module_names: &[String]) -> String {
    let mut code = String::from(
        "//! `SeaORM` Entity, @generated by graphily-compiler\n\npub mod prelude;\n\n"
    );
    for name in module_names {
        code.push_str(&format!("pub mod {};\n", name));
    }
    code.push_str("\nseaography::register_entity_modules!([\n");
    for name in module_names {
        code.push_str(&format!("    {},\n", name));
    }
    code.push_str("]);\n");
    code
}

fn generate_prelude_rs(module_names: &[String]) -> String {
    let mut code = String::from("//! `SeaORM` Entity, @generated by graphily-compiler\n\n");
    for name in module_names {
        code.push_str(&format!("pub use super::{}::Entity as {};\n", name, pascal_case(name)));
    }
    code
}
