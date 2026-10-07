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

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

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
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
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
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
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

fn jsx_attribute_nodes(tag: &str, parent_id: &str, line_no: u32) -> Vec<SemanticNode> {
    let Some(name) = jsx_tag_name(tag) else {
        return Vec::new();
    };
    let mut cursor = 1 + name.len();
    let mut nodes = Vec::new();
    while cursor < tag.len() {
        while tag[cursor..].starts_with(char::is_whitespace) {
            cursor += tag[cursor..].chars().next().unwrap().len_utf8();
        }
        if cursor == tag.len() || tag[cursor..].starts_with('>') || tag[cursor..].starts_with("/>")
        {
            break;
        }
        let start = cursor;
        while cursor < tag.len() {
            let ch = tag[cursor..].chars().next().unwrap();
            if ch.is_whitespace() || matches!(ch, '=' | '>' | '/') {
                break;
            }
            cursor += ch.len_utf8();
        }
        while tag[cursor..].starts_with(char::is_whitespace) {
            cursor += tag[cursor..].chars().next().unwrap().len_utf8();
        }
        if !tag[cursor..].starts_with('=') {
            if cursor == start {
                cursor += tag[cursor..].chars().next().unwrap().len_utf8();
            }
            continue;
        }
        cursor += 1;
        while tag[cursor..].starts_with(char::is_whitespace) {
            cursor += tag[cursor..].chars().next().unwrap().len_utf8();
        }
        let mut context = LexicalState::default();
        while cursor < tag.len() {
            let ch = tag[cursor..].chars().next().unwrap();
            if context.neutral()
                && (ch.is_whitespace() || ch == '>' || tag[cursor..].starts_with("/>"))
            {
                break;
            }
            context.push(ch);
            cursor += ch.len_utf8();
        }
        let attr = &tag[start..cursor];
        let mut attribute = leaf(
            &format!("{parent_id}.{}", nodes.len()),
            "jsx_attribute",
            attr,
            line_no,
        );
        let point = |offset| {
            let prefix = &tag[..offset];
            (
                line_no + prefix.bytes().filter(|&b| b == b'\n').count() as u32,
                prefix.rsplit('\n').next().unwrap_or("").len() as u32,
            )
        };
        (attribute.position.start_line, attribute.position.start_col) = point(start);
        (attribute.position.end_line, attribute.position.end_col) = point(cursor);
        nodes.push(attribute);
    }
    nodes
}

// Opening tags may span lines. Keep quote/expression state so a quoted `>` or
// an arrow in a JSX expression does not prematurely end the opening tag.
#[derive(Default)]
struct LexicalState {
    quote: Option<char>,
    escaped: bool,
    braces: usize,
}
impl LexicalState {
    fn neutral(&self) -> bool {
        self.quote.is_none() && self.braces == 0
    }
    fn push(&mut self, ch: char) {
        if let Some(quote) = self.quote {
            if self.escaped {
                self.escaped = false;
            } else if ch == '\\' {
                self.escaped = true;
            } else if ch == quote {
                self.quote = None;
            }
        } else {
            match ch {
                '\'' | '"' | '`' => self.quote = Some(ch),
                '{' => self.braces += 1,
                '}' => self.braces = self.braces.saturating_sub(1),
                _ => {}
            }
        }
    }
}
#[derive(Default)]
struct PendingTag {
    text: String,
    line: u32,
    col: u32,
    context: LexicalState,
}
impl PendingTag {
    fn push(&mut self, ch: char) -> bool {
        self.text.push(ch);
        let end = self.context.neutral() && ch == '>';
        self.context.push(ch);
        end
    }
}

fn prose(text: &str, line: u32, col: usize, children: &mut Vec<SemanticNode>, id: &mut usize) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let mut paragraph = leaf(&format!("0.{}", *id), "paragraph", trimmed, line);
    *id += 1;
    paragraph.position.start_col = (col + text.len() - text.trim_start().len()) as u32;
    paragraph.position.end_col = (col + text.trim_end().len()) as u32;
    children.push(paragraph);
}

fn jsx_line(
    line: &str,
    lineno: u32,
    pending: &mut Option<PendingTag>,
    expression: &mut LexicalState,
    stack: &mut Vec<(String, usize)>,
    children: &mut Vec<SemanticNode>,
    id: &mut usize,
) -> Result<(), &'static str> {
    let mut cursor = 0;
    let mut prose_start = 0;
    let mut inline_ticks = 0;
    while cursor < line.len() {
        if pending.is_none() {
            let ch = line[cursor..].chars().next().unwrap();
            if !expression.neutral() || (inline_ticks == 0 && ch == '{') {
                expression.push(ch);
                cursor += ch.len_utf8();
                continue;
            }
            if line.as_bytes()[cursor] == b'`' {
                let count = line[cursor..].bytes().take_while(|&b| b == b'`').count();
                if inline_ticks == 0 {
                    inline_ticks = count;
                } else if inline_ticks == count {
                    inline_ticks = 0;
                }
                cursor += count;
                continue;
            }
            let rest = &line[cursor..];
            let tag = rest.strip_prefix("</").or_else(|| rest.strip_prefix('<'));
            let is_tag = inline_ticks == 0
                && (cursor == 0 || line.as_bytes()[cursor - 1] != b'\\')
                && tag
                    .and_then(|s| s.chars().next())
                    .is_some_and(char::is_uppercase);
            if !is_tag {
                cursor += rest.chars().next().unwrap().len_utf8();
                continue;
            }
            prose(
                &line[prose_start..cursor],
                lineno,
                prose_start,
                children,
                id,
            );
            *pending = Some(PendingTag {
                line: lineno,
                col: cursor as u32,
                ..Default::default()
            });
        }
        let ch = line[cursor..].chars().next().unwrap();
        cursor += ch.len_utf8();
        if pending.as_mut().unwrap().push(ch) {
            let tag = pending.take().unwrap();
            if let Some(close) = tag.text.strip_prefix("</") {
                let name = close.trim_end_matches('>').trim();
                let (open, index) = stack.pop().ok_or("Unmatched JSX closing tag")?;
                if open != name {
                    return Err("Mismatched JSX closing tag");
                }
                children[index].position.end_line = lineno;
                children[index].position.end_col = cursor as u32;
            } else {
                let name = jsx_tag_name(&tag.text).ok_or("Invalid JSX component tag")?;
                let label = jsx_component_label(&tag.text).ok_or("Invalid JSX component label")?;
                let node_id = format!("0.{}", *id);
                *id += 1;
                let mut attrs = jsx_attribute_nodes(&tag.text, &node_id, tag.line);
                for attr in &mut attrs {
                    if attr.position.start_line == tag.line {
                        attr.position.start_col += tag.col;
                    }
                    if attr.position.end_line == tag.line {
                        attr.position.end_col += tag.col;
                    }
                }
                let mut component = node(&node_id, "jsx_component", &label, tag.line, attrs);
                component.position.start_col = tag.col;
                component.position.end_line = lineno;
                component.position.end_col = cursor as u32;
                if !tag.text.trim_end_matches('>').trim_end().ends_with('/') {
                    stack.push((name.to_string(), children.len()));
                }
                children.push(component);
            }
            prose_start = cursor;
        }
    }
    if let Some(tag) = pending {
        tag.push('\n');
    } else {
        prose(&line[prose_start..], lineno, prose_start, children, id);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main parse
// ---------------------------------------------------------------------------

fn parse_mdx(source: &str) -> String {
    let mut children: Vec<SemanticNode> = Vec::new();
    let mut id_counter: usize = 0;
    let mut code_fence: Option<(char, usize)> = None;
    let mut pending_tag: Option<PendingTag> = None;
    let mut expression = LexicalState::default();
    let mut code_lang = String::new();
    let mut code_start_line: u32 = 0;
    let mut jsx_stack: Vec<(String, usize)> = Vec::new();
    let mut in_frontmatter = false;
    let mut frontmatter_done = false;

    for (line_idx, line) in source.lines().enumerate() {
        let lineno = line_idx as u32;
        let trimmed = line.trim();

        if pending_tag.is_some() || !expression.neutral() {
            if let Err(error) = jsx_line(
                line,
                lineno,
                &mut pending_tag,
                &mut expression,
                &mut jsx_stack,
                &mut children,
                &mut id_counter,
            ) {
                return serde_json::json!({"error": error}).to_string();
            }
            continue;
        }

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

        // A shorter fence or a different delimiter remains code content.
        let fence_char = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'));
        let fence_len = fence_char.map_or(0, |c| trimmed.chars().take_while(|&x| x == c).count());
        if let Some((delimiter, length)) = code_fence {
            if fence_char == Some(delimiter)
                && fence_len >= length
                && trimmed[fence_len..].trim().is_empty()
            {
                let label = if code_lang.is_empty() {
                    "text"
                } else {
                    &code_lang
                };
                let id = format!("0.{}", id_counter);
                id_counter += 1;
                let mut block = leaf(&id, "code_block", label, code_start_line);
                block.position.end_line = lineno;
                block.position.end_col = line.trim_end().len() as u32;
                children.push(block);
                code_fence = None;
                code_lang.clear();
            }
            continue;
        }
        if fence_len >= 3 {
            code_fence = Some((fence_char.unwrap(), fence_len));
            code_lang = trimmed[fence_len..].trim().to_string();
            code_start_line = lineno;
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

        if let Err(error) = jsx_line(
            line,
            lineno,
            &mut pending_tag,
            &mut expression,
            &mut jsx_stack,
            &mut children,
            &mut id_counter,
        ) {
            return serde_json::json!({"error": error}).to_string();
        }
    }

    if code_fence.is_some() || pending_tag.is_some() || !jsx_stack.is_empty() {
        return serde_json::json!({"error": "Unterminated MDX code block or component"})
            .to_string();
    }
    let lines: Vec<_> = source.lines().collect();
    for child in &mut children {
        if let Some(line) = lines.get(child.position.start_line as usize) {
            if child.node_type != "jsx_component" && child.node_type != "paragraph" {
                child.position.start_col = (line.len() - line.trim_start().len()) as u32;
            }
            if child.position.end_col == 0 && child.position.end_line == child.position.start_line {
                child.position.end_col = line.trim_end().len() as u32;
            }
        }
    }
    let mut root = node("0", "document", "document", 0, children);
    root.position.end_line = source.bytes().filter(|&b| b == b'\n').count() as u32;
    root.position.end_col = source.rsplit('\n').next().unwrap_or("").len() as u32;
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
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn multiline_attribute_end_column_does_not_inherit_inline_start_offset() {
        let tree: serde_json::Value =
            serde_json::from_str(&parse_mdx("é <Card title=\"a\nb\"/> ")).unwrap();
        let card = tree["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["node_type"] == "jsx_component")
            .unwrap();
        assert_eq!(card["children"][0]["position"]["start_col"], 9);
        assert_eq!(card["children"][0]["position"]["end_line"], 1);
        assert_eq!(card["children"][0]["position"]["end_col"], 2);
    }

    #[test]
    fn attribute_words_and_expression_tails_remain_semantic() {
        for source in [
            "<Card\n title=\"Hello world\"\n/>",
            "<Card title = {hello + world} />",
        ] {
            let before: serde_json::Value = serde_json::from_str(&parse_mdx(source)).unwrap();
            let after: serde_json::Value =
                serde_json::from_str(&parse_mdx(&source.replace("world", "there"))).unwrap();
            assert!(before.get("error").is_none(), "{before}");
            assert!(after.get("error").is_none(), "{after}");
            assert_ne!(before["structural_hash"], after["structural_hash"]);
        }
    }

    #[test]
    fn mdx_expressions_do_not_turn_strings_or_comparisons_into_tags() {
        for source in [
            r#"{"Use <Card> here"}"#,
            r#"{score <MAX ? "low" : "high"}"#,
            "{\n 'Use <Card> here'\n}\n<Real />",
        ] {
            let tree: serde_json::Value = serde_json::from_str(&parse_mdx(source)).unwrap();
            assert!(tree.get("error").is_none(), "{tree}");
            assert!(!tree["children"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["label"] == "Card"));
        }
    }

    #[test]
    fn jsx_delimiters_inside_quotes_and_code_do_not_close_components() {
        let source =
            "é <Outer><Card\n title=\">\" check={x => x > 1}\n/></Outer> after\n`<NotAComponent>`";
        let tree: serde_json::Value = serde_json::from_str(&parse_mdx(source)).unwrap();
        assert!(tree.get("error").is_none(), "{tree}");
        let nodes = tree["children"].as_array().unwrap();
        let components: Vec<_> = nodes
            .iter()
            .filter(|n| n["node_type"] == "jsx_component")
            .collect();
        assert_eq!(components.len(), 2);
        assert_eq!(components[0]["position"]["start_col"], 3);
        assert_eq!(components[0]["position"]["end_line"], 2);
        assert_eq!(components[0]["position"]["end_col"], 10);
        assert_eq!(components[1]["position"]["end_col"], 2);
        assert!(nodes.iter().any(|n| n["label"] == "after"));
    }

    #[test]
    fn multiline_self_closing_component_keeps_props_and_extent() {
        let source = "<Card\n  title=\"Hello\"\n/>";
        let tree: serde_json::Value = serde_json::from_str(&parse_mdx(source)).unwrap();
        assert!(tree.get("error").is_none(), "{tree}");
        let card = &tree["children"][0];
        assert_eq!(card["node_type"], "jsx_component");
        assert_eq!(card["position"]["end_line"], 2);
        assert_eq!(card["position"]["end_col"], 2);
        assert_eq!(card["children"][0]["position"]["start_line"], 1);
        let changed: serde_json::Value =
            serde_json::from_str(&parse_mdx(&source.replace("Hello", "World"))).unwrap();
        assert_ne!(tree["structural_hash"], changed["structural_hash"]);
    }

    #[test]
    fn component_after_prose_keeps_prose_and_component_span() {
        let source = "Intro <Callout>\ntext\n</Callout>";
        let tree: serde_json::Value = serde_json::from_str(&parse_mdx(source)).unwrap();
        assert!(tree.get("error").is_none(), "{tree}");
        let nodes = tree["children"].as_array().unwrap();
        assert!(nodes.iter().any(|n| n["label"] == "Intro"));
        let component = nodes
            .iter()
            .find(|n| n["node_type"] == "jsx_component")
            .unwrap();
        assert_eq!(component["position"]["start_col"], 6);
        assert_eq!(component["position"]["end_line"], 2);
        assert_eq!(component["position"]["end_col"], 10);
        assert!(nodes.iter().any(|n| n["label"] == "text"));
    }

    #[test]
    fn code_fence_requires_matching_character_and_sufficient_length() {
        for source in ["````md\n```\n~~~\n````", "~~~~md\n~~~\n```\n~~~~"] {
            let tree: serde_json::Value = serde_json::from_str(&parse_mdx(source)).unwrap();
            assert!(tree.get("error").is_none(), "{tree}");
            let nodes = tree["children"].as_array().unwrap();
            assert_eq!(nodes.len(), 1);
            assert_eq!(nodes[0]["node_type"], "code_block");
            assert_eq!(nodes[0]["label"], "md");
            assert_eq!(nodes[0]["position"]["end_line"], 3);
            assert_eq!(nodes[0]["position"]["end_col"], 4);
        }
    }

    #[test]
    fn incomplete_component_and_fence_return_explicit_errors() {
        for source in [
            "<Callout>\ntext",
            "```bash\necho hello",
            "<Callout>\n</Other>",
        ] {
            let tree: serde_json::Value = serde_json::from_str(&parse_mdx(source)).unwrap();
            assert!(tree["error"].is_string());
        }
    }

    #[test]
    fn source_spans_cover_import_and_multiline_code_and_component() {
        let source = "  import { Callout } from './components'\n\n<Callout type=\"info\">\n  Use café.\n</Callout>\n\n```bash\necho hello\n```\n";
        let tree: serde_json::Value = serde_json::from_str(&parse_mdx(source)).unwrap();
        let nodes = tree["children"].as_array().unwrap();
        assert_eq!(nodes[0]["position"]["start_col"], 2);
        assert_eq!(nodes[0]["position"]["end_col"], 40);
        let component = nodes
            .iter()
            .find(|n| n["node_type"] == "jsx_component")
            .unwrap();
        assert_eq!(component["position"]["end_line"], 4);
        assert_eq!(component["position"]["end_col"], 10);
        let code = nodes
            .iter()
            .find(|n| n["node_type"] == "code_block")
            .unwrap();
        assert_eq!(code["position"]["start_line"], 6);
        assert_eq!(code["position"]["end_line"], 8);
        assert_eq!(code["position"]["end_col"], 3);
        assert!(!nodes.iter().any(|n| n["label"] == "</Callout>"));
    }

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
