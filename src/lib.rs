//! MDX (Markdown + JSX) parser plugin — full-parse mode.
//!
//! Handles `.mdx` files: Markdown documents with embedded JSX/TSX components.
//! No mature tree-sitter grammar covers MDX reliably, so this plugin uses
//! line-by-line string scanning to extract the structural elements that matter
//! for semantic diffing.
//!
//! Semantic node types produced:
//!
//!   document             — root node
//!   import_statement     — `import Foo from './Foo'` (label = module path)
//!   component_definition — `export function Foo` / `export const Foo = () =>`
//!                          (label = component name)
//!   section              — Markdown heading (label = heading text, h1–h6)
//!   code_block           — Fenced code block (label = language identifier)
//!   jsx_component        — Top-level JSX element usage `<ComponentName …>`
//!                          (label = tag name)
//!
//! Detection: `.mdx` file extension only.

use intentdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct MdxParser;

// ---------------------------------------------------------------------------
// Node construction helpers
// ---------------------------------------------------------------------------

fn node(
    id: &str,
    node_type: &str,
    label: &str,
    line: u32,
    children: Vec<SemanticNode>,
) -> SemanticNode {
    let hash = structural_hash(node_type, label, line, &children);
    SemanticNodeBuilder::new(id, node_type, label, line, 0, line, 0, hash)
        .children(children)
        .build()
}

fn leaf(id: &str, node_type: &str, label: &str, line: u32) -> SemanticNode {
    node(id, node_type, label, line, Vec::new())
}

fn structural_hash(node_type: &str, label: &str, _line: u32, children: &[SemanticNode]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for part in [node_type, "\0", label] {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    for child in children {
        for byte in child.structural_hash.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Extract module path from an import line.
/// `import Foo from './Foo'` → `./Foo`
/// `import { A, B } from '../lib'` → `../lib`
fn import_module(line: &str) -> &str {
    // Find last quoted token
    let line = line.trim();
    if let Some(from_pos) = line.rfind(" from ") {
        let after = line[from_pos + 6..].trim();
        let inner = after.trim_matches(|c| c == '\'' || c == '"' || c == '`' || c == ';');
        return inner;
    }
    // Bare `import './side-effect'`
    let after = line.strip_prefix("import").unwrap_or(line).trim();
    let inner = after.trim_matches(|c| c == '\'' || c == '"' || c == '`' || c == ';');
    inner
}

/// Extract component name from an export line.
/// `export default function Page` → `Page`
/// `export function MyComponent` → `MyComponent`
/// `export const MyComponent = ` → `MyComponent`
/// `export default function (` → `(anonymous)`
fn export_component_name(line: &str) -> &str {
    let line = line.trim();
    // Strip leading "export"
    let rest = line.strip_prefix("export").unwrap_or(line).trim_start();
    // Strip "default"
    let rest = rest.strip_prefix("default").unwrap_or(rest).trim_start();
    // Now: `function Foo`, `const Foo`, `class Foo`, `Foo =`, etc.
    let rest = rest
        .strip_prefix("function")
        .or_else(|| rest.strip_prefix("const"))
        .or_else(|| rest.strip_prefix("let"))
        .or_else(|| rest.strip_prefix("var"))
        .or_else(|| rest.strip_prefix("class"))
        .unwrap_or(rest)
        .trim_start();
    // Now first token is the name
    let name = rest
        .split(|c: char| c == '(' || c == ' ' || c == '=' || c == '<' || c == '{')
        .next()
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        "(anonymous)"
    } else {
        name
    }
}

/// Extract JSX tag name from a line starting with `<ComponentName`.
/// Only includes PascalCase tags (JSX components, not HTML elements).
fn jsx_tag_name(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('<')?;
    // Must start with uppercase (component) or namespace (Foo.Bar)
    let first_char = rest.chars().next()?;
    if !first_char.is_uppercase() {
        return None;
    }
    let name = rest
        .split(|c: char| c == ' ' || c == '/' || c == '>' || c == '\n')
        .next()?;
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn jsx_component_label(line: &str) -> Option<String> {
    let tag = jsx_tag_name(line)?;
    let trimmed = line.trim();
    for quote in ['"', '\''] {
        let needle = format!("name={quote}");
        if let Some(start) = trimmed.find(&needle) {
            let value_start = start + needle.len();
            if let Some(value_end) = trimmed[value_start..].find(quote) {
                let name = &trimmed[value_start..value_start + value_end];
                if !name.is_empty() {
                    return Some(format!("{tag} {name}"));
                }
            }
        }
    }
    Some(tag.to_string())
}

fn jsx_attribute_nodes(line: &str, parent_id: &str, line_no: u32) -> Vec<SemanticNode> {
    let trimmed = line.trim();
    let Some(after_tag) = trimmed
        .trim_start_matches('<')
        .split_once(|c: char| c == ' ' || c == '\t')
        .map(|(_, rest)| rest)
    else {
        return Vec::new();
    };
    let attrs = after_tag
        .trim()
        .trim_end_matches('>')
        .trim_end_matches('/')
        .trim();
    let mut nodes = Vec::new();
    for (idx, part) in attrs.split_whitespace().enumerate() {
        let attr = part
            .trim_end_matches('>')
            .trim_end_matches('/')
            .trim_end_matches(',')
            .trim();
        if attr.is_empty() || !attr.contains('=') {
            continue;
        }
        let label = attr
            .trim_matches(|c| c == '"' || c == '\'' || c == '{' || c == '}')
            .to_string();
        nodes.push(leaf(
            &format!("{parent_id}.{}", idx),
            "jsx_attribute",
            &label,
            line_no,
        ));
    }
    nodes
}

/// True if a line looks like it starts a JSX component usage.
fn is_jsx_usage(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('<') && t.len() > 1 && t.chars().nth(1).map(|c| c.is_uppercase()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Main parse
// ---------------------------------------------------------------------------

fn parse_mdx(source: &str) -> String {
    let mut children: Vec<SemanticNode> = Vec::new();
    let mut id_counter: usize = 0;
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_start_line: u32 = 0;
    let mut in_frontmatter = false;
    let mut frontmatter_done = false;

    for (line_idx, line) in source.lines().enumerate() {
        let lineno = line_idx as u32;
        let trimmed = line.trim();

        // --- YAML frontmatter (--- ... ---) ---
        if !frontmatter_done && lineno == 0 && trimmed == "---" {
            in_frontmatter = true;
            continue;
        }
        if in_frontmatter {
            if trimmed == "---" {
                in_frontmatter = false;
                frontmatter_done = true;
            }
            continue;
        }

        // --- Fenced code blocks (``` ... ```) ---
        if trimmed.starts_with("```") {
            if !in_code_block {
                in_code_block = true;
                code_lang = trimmed[3..].trim().to_string();
                code_start_line = lineno;
            } else {
                // Closing fence
                let label = if code_lang.is_empty() {
                    "text".to_string()
                } else {
                    code_lang.clone()
                };
                let id = format!("0.{}", id_counter);
                id_counter += 1;
                children.push(leaf(&id, "code_block", &label, code_start_line));
                in_code_block = false;
                code_lang.clear();
            }
            continue;
        }
        if in_code_block {
            continue;
        }

        // --- Import statements ---
        if trimmed.starts_with("import ") {
            let module = import_module(trimmed).to_string();
            let id = format!("0.{}", id_counter);
            id_counter += 1;
            children.push(leaf(&id, "import_statement", &module, lineno));
            continue;
        }

        // --- Export (component definitions) ---
        if trimmed.starts_with("export ") {
            let name = export_component_name(trimmed).to_string();
            let id = format!("0.{}", id_counter);
            id_counter += 1;
            children.push(leaf(&id, "component_definition", &name, lineno));
            continue;
        }

        // --- Markdown headings ---
        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|&c| c == '#').count();
            if level <= 6 && trimmed[level..].starts_with(|c: char| c == ' ' || c == '\t') {
                let heading_text = trimmed[level..].trim().to_string();
                let id = format!("0.{}", id_counter);
                id_counter += 1;
                children.push(leaf(&id, "section", &heading_text, lineno));
                continue;
            }
        }

        // --- Top-level JSX component usage ---
        if is_jsx_usage(trimmed) {
            if let Some(tag) = jsx_component_label(trimmed) {
                let id = format!("0.{}", id_counter);
                id_counter += 1;
                let attrs = jsx_attribute_nodes(trimmed, &id, lineno);
                children.push(node(&id, "jsx_component", &tag, lineno, attrs));
            }
            continue;
        }

        // --- Prose paragraphs (#46) --- previously captured NOWHERE, so an edit inside
        // body text ("Node.js 18+" -> "19+") hashed style-only.
        if !trimmed.is_empty() {
            let id = format!("0.{}", id_counter);
            id_counter += 1;
            children.push(leaf(&id, "paragraph", trimmed, lineno));
        }
    }

    let root = node("0", "document", "document", 0, children);
    match serde_json::to_string(&root) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for MdxParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "mdx".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.to_lowercase().ends_with(".mdx") {
            return "mdx".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        parse_mdx(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        vec![]
    }
    fn language_ids() -> Vec<String> {
        vec!["mdx".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "# Getting Started\n\nWelcome to the docs.\n\n## Installation\n\nRun `npm install` to get started.\n".to_string(),
            new: "import { Callout } from './components'\n\n# Getting Started\n\nWelcome to the docs. This guide helps you get up and running.\n\n<Callout type=\"info\">\n  Make sure you have Node.js 18+ installed.\n</Callout>\n\n## Installation\n\n```bash\nnpm install my-package\n```\n".to_string(),
        }
    }
}
export!(MdxParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!MdxParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = MdxParser::grammar_id();
        let ids = MdxParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = MdxParser::detect_language("test.mdx".to_string(), "".to_string());
        assert_eq!(r.as_str(), "mdx");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r =
            MdxParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = parse_mdx("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = parse_mdx("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }

    #[test]
    fn process_impl_hash_changes_when_component_is_added() {
        let old = parse_mdx("# Checklist\n\n<Step name=\"Build\" status=\"ready\" />\n");
        let new = parse_mdx(
            "# Checklist\n\n<Step name=\"Build\" status=\"ready\" />\n<Step name=\"Package\" status=\"blocked\" />\n",
        );
        let old_json: serde_json::Value = serde_json::from_str(&old).unwrap();
        let new_json: serde_json::Value = serde_json::from_str(&new).unwrap();

        assert_ne!(old_json["structural_hash"], new_json["structural_hash"]);
    }

    #[test]
    fn process_impl_labels_named_jsx_components() {
        let out = parse_mdx("<Step name=\"Package\" status=\"blocked\" />\n");
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        let labels: Vec<&str> = json["children"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|child| child["label"].as_str())
            .collect();

        assert!(labels.contains(&"Step Package"));
    }

    #[test]
    fn process_impl_hash_changes_when_component_prop_changes() {
        let old = parse_mdx("<Step name=\"Verify\" status=\"pending\" />\n");
        let new = parse_mdx("<Step name=\"Verify\" status=\"ready\" />\n");
        let old_json: serde_json::Value = serde_json::from_str(&old).unwrap();
        let new_json: serde_json::Value = serde_json::from_str(&new).unwrap();

        assert_eq!(old_json["children"][0]["label"], "Step Verify");
        assert_eq!(new_json["children"][0]["label"], "Step Verify");
        assert_ne!(
            old_json["children"][0]["structural_hash"],
            new_json["children"][0]["structural_hash"]
        );
    }
}
