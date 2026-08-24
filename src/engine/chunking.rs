use tree_sitter::{Language, Node, Parser};

use crate::engine::types::Chunk;

const DESIRED_CHUNK_LENGTH_CHARS: usize = 1500;

struct ChunkBoundary {
    start: usize,
    end: usize,
    /// True when this boundary must not be merged into the one before it.
    barrier: bool,
}

fn get_language(name: &str) -> Option<Language> {
    let lang_fn = match name {
        "rust" => tree_sitter_rust::LANGUAGE,
        "python" => tree_sitter_python::LANGUAGE,
        "javascript" => tree_sitter_javascript::LANGUAGE,
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        "go" => tree_sitter_go::LANGUAGE,
        "java" => tree_sitter_java::LANGUAGE,
        "c" => tree_sitter_c::LANGUAGE,
        "cpp" => tree_sitter_cpp::LANGUAGE,
        "csharp" => tree_sitter_c_sharp::LANGUAGE,
        "css" => tree_sitter_css::LANGUAGE,
        "html" => tree_sitter_html::LANGUAGE,
        "kotlin" => tree_sitter_kotlin_ng::LANGUAGE,
        "ruby" => tree_sitter_ruby::LANGUAGE,
        "php" => tree_sitter_php::LANGUAGE_PHP,
        "swift" => tree_sitter_swift::LANGUAGE,
        _ => return None,
    };
    Some(Language::from(lang_fn))
}

fn is_definition_node(language: &str, node: &Node) -> bool {
    let kind = node.kind();
    match language {
        "rust" => matches!(
            kind,
            "function_item"
                | "impl_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "mod_item"
                | "const_item"
                | "static_item"
                | "type_item"
                | "macro_definition"
                | "attribute_item"
        ),
        "python" => matches!(
            kind,
            "function_definition" | "class_definition" | "decorated_definition"
        ),
        "javascript" => matches!(
            kind,
            "function_declaration"
                | "class_declaration"
                | "export_statement"
                | "lexical_declaration"
                | "variable_declaration"
        ),
        "typescript" => matches!(
            kind,
            "function_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "export_statement"
                | "lexical_declaration"
                | "variable_declaration"
        ),
        "go" => matches!(
            kind,
            "function_declaration" | "method_declaration" | "type_declaration"
        ),
        "java" => matches!(
            kind,
            "class_declaration"
                | "method_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "constructor_declaration"
                | "record_declaration"
        ),
        "c" => matches!(
            kind,
            "function_definition" | "struct_specifier" | "enum_specifier" | "declaration"
        ),
        "cpp" => matches!(
            kind,
            "function_definition"
                | "class_specifier"
                | "struct_specifier"
                | "enum_specifier"
                | "declaration"
                | "namespace_definition"
                | "template_declaration"
        ),
        "kotlin" => matches!(
            kind,
            "class_declaration"
                | "object_declaration"
                | "function_declaration"
                | "property_declaration"
                | "type_alias"
                | "companion_object"
                | "secondary_constructor"
        ),
        "ruby" => matches!(
            kind,
            "method" | "singleton_method" | "class" | "module" | "singleton_class" | "assignment"
        ),
        "php" => matches!(
            kind,
            "function_definition"
                | "method_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "trait_declaration"
                | "enum_declaration"
                | "namespace_definition"
        ),
        "swift" => matches!(
            kind,
            "function_declaration"
                | "class_declaration"
                | "protocol_declaration"
                | "extension_declaration"
                | "enum_declaration"
                | "struct_declaration"
                | "property_declaration"
                | "typealias_declaration"
        ),
        "csharp" => matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "record_struct_declaration"
                | "struct_declaration"
                | "enum_declaration"
                | "delegate_declaration"
                | "method_declaration"
                | "constructor_declaration"
                | "property_declaration"
        ),
        // An element is the unit a reader looks for in markup, the same way a
        // function is in code. Nested elements are boundaries too and the
        // merge below recombines the small ones, so a page of short list items
        // does not become a chunk each.
        "html" => matches!(
            kind,
            "element" | "script_element" | "style_element" | "doctype"
        ),
        // A rule set is the unit in css. The at rules are here because each
        // one owns the rules inside it, so breaking before it keeps a media
        // query with the rules it applies to.
        "css" => matches!(
            kind,
            "rule_set"
                | "media_statement"
                | "keyframes_statement"
                | "supports_statement"
                | "import_statement"
                | "charset_statement"
                | "at_rule"
        ),
        _ => false,
    }
}

/// A declaration that must start its own chunk rather than be merged into the
/// one before it.
///
/// Merging adjacent declarations keeps chunks near the desired length, which
/// is right within a type and wrong between two of them: a partial class is
/// written as several declarations of the same name, and merging two parts
/// produces a chunk that reads as one definition of something that has none.
/// A type is the unit a reader searches for, so a type declaration breaks.
fn is_chunk_barrier(language: &str, node: &Node) -> bool {
    language == "csharp"
        && matches!(
            node.kind(),
            "class_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "record_struct_declaration"
                | "struct_declaration"
                | "enum_declaration"
        )
}

/// The declarations a chunk can start at, descending through namespaces.
///
/// Every other language here declares at the top level. C# wraps the file in
/// a namespace, and in the block form that leaves the root with a single
/// child, so reading only the root's children would return the whole file as
/// one chunk.
fn collect_definitions(language: &str, node: &Node, out: &mut Vec<ChunkBoundary>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "namespace_declaration" | "file_scoped_namespace_declaration" | "declaration_list"
        ) {
            collect_definitions(language, &child, out);
            continue;
        }
        if is_definition_node(language, &child) {
            out.push(ChunkBoundary {
                start: child.start_byte(),
                end: child.end_byte(),
                barrier: is_chunk_barrier(language, &child),
            });
            // A type's members are boundaries in their own right, so a large
            // service class chunks by method instead of arriving as one chunk
            // that no search can point into. Markup nests the same way, with
            // the whole page inside a single root element.
            if is_chunk_barrier(language, &child) || language == "html" {
                collect_definitions(language, &child, out);
            }
        }
    }
}

fn chunk_with_tree_sitter(source: &str, language: &str) -> Option<Vec<ChunkBoundary>> {
    let ts_lang = get_language(language)?;
    let mut parser = Parser::new();
    parser.set_language(&ts_lang).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();

    let mut definitions: Vec<ChunkBoundary> = Vec::new();
    collect_definitions(language, &root, &mut definitions);
    definitions.sort_by_key(|d| d.start);
    let def_starts: Vec<usize> = definitions.iter().map(|d| d.start).collect();

    if def_starts.is_empty() {
        return None;
    }

    let mut boundaries = Vec::new();

    for (i, &start) in def_starts.iter().enumerate() {
        let end = if i + 1 < def_starts.len() {
            def_starts[i + 1]
        } else {
            source.len()
        };

        // For the first definition, include any leading content (imports, comments)
        let actual_start = if i == 0 { 0 } else { start };

        if actual_start < end {
            boundaries.push(ChunkBoundary {
                start: actual_start,
                end,
                barrier: definitions[i].barrier,
            });
        }
    }

    if boundaries.is_empty() {
        return None;
    }

    Some(merge_adjacent_chunks(
        &boundaries,
        DESIRED_CHUNK_LENGTH_CHARS,
    ))
}

fn merge_adjacent_chunks(chunks: &[ChunkBoundary], desired_length: usize) -> Vec<ChunkBoundary> {
    if chunks.is_empty() {
        return Vec::new();
    }

    let mut merged = Vec::new();
    let mut current_start = chunks[0].start;
    let mut current_end = chunks[0].end;
    let mut current_length = current_end - current_start;

    for group in &chunks[1..] {
        let length = group.end - group.start;

        if group.barrier || current_length + length > desired_length {
            merged.push(ChunkBoundary {
                start: current_start,
                end: current_end,
                barrier: false,
            });
            current_start = group.start;
            current_end = group.end;
            current_length = length;
            continue;
        }

        current_end = group.end;
        current_length += length;
    }

    merged.push(ChunkBoundary {
        start: current_start,
        end: current_end,
        barrier: false,
    });

    merged
}

fn chunk_lines(text: &str, desired_length: usize) -> Vec<ChunkBoundary> {
    if text.trim().is_empty() {
        return Vec::new();
    }

    let mut lines_as_groups = Vec::new();
    let mut index = 0;
    for line in text.split_inclusive('\n') {
        lines_as_groups.push(ChunkBoundary {
            start: index,
            end: index + line.len(),
            barrier: false,
        });
        index += line.len();
    }
    if index < text.len() {
        lines_as_groups.push(ChunkBoundary {
            start: index,
            end: text.len(),
            barrier: false,
        });
    }

    merge_adjacent_chunks(&lines_as_groups, desired_length)
}

pub fn chunk_source(source: &str, file_path: &str, language: Option<&str>) -> Vec<Chunk> {
    if source.trim().is_empty() {
        return Vec::new();
    }

    let boundaries = language
        .and_then(|lang| chunk_with_tree_sitter(source, lang))
        .unwrap_or_else(|| chunk_lines(source, DESIRED_CHUNK_LENGTH_CHARS));

    let mut chunks = Vec::new();
    for boundary in &boundaries {
        let end_index = boundary.end.max(boundary.start);
        let text = &source[boundary.start..end_index];

        let start_line = source[..boundary.start].matches('\n').count() + 1;
        let end_line = if end_index > 0 {
            source[..end_index].matches('\n').count() + 1
        } else {
            1
        };

        chunks.push(Chunk::new(
            text.to_string(),
            file_path.to_string(),
            start_line,
            end_line,
            language.map(String::from),
        ));
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_tree_sitter_chunking_small() {
        let source = r#"
use std::collections::HashMap;

fn foo() {
    println!("foo");
}

struct MyStruct {
    field: i32,
}
"#;
        let chunks = chunk_source(source, "test.rs", Some("rust"));
        assert!(!chunks.is_empty());
        let all_content: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(all_content.contains("fn foo"));
        assert!(all_content.contains("struct MyStruct"));
        assert!(all_content.contains("use std::collections"));
    }

    #[test]
    fn test_rust_tree_sitter_splits_large() {
        let long_body = "    let x = 1;\n".repeat(100);
        let source = format!(
            "fn foo() {{\n{long_body}}}\n\nfn bar() {{\n{long_body}}}\n\nfn baz() {{\n{long_body}}}\n"
        );
        let chunks = chunk_source(&source, "test.rs", Some("rust"));
        assert!(
            chunks.len() >= 2,
            "large source should split: got {} chunks",
            chunks.len()
        );
    }

    #[test]
    fn test_python_tree_sitter_chunking() {
        let long_body = "    x = 1\n".repeat(100);
        let source =
            format!("import os\n\nclass MyClass:\n{long_body}\ndef standalone():\n{long_body}\n");
        let chunks = chunk_source(&source, "test.py", Some("python"));
        assert!(
            chunks.len() >= 2,
            "large python source should split: got {} chunks",
            chunks.len()
        );
        let all_content: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(all_content.contains("class MyClass"));
        assert!(all_content.contains("def standalone"));
    }

    #[test]
    fn test_fallback_for_unknown_language() {
        let source = "line1\nline2\nline3\n";
        let chunks = chunk_source(source, "test.xyz", None);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_javascript_tree_sitter_chunking() {
        let source = r#"
const x = require('something');

function hello() {
    console.log("hello");
}

class Greeter {
    greet() {
        return "hi";
    }
}
"#;
        let chunks = chunk_source(source, "test.js", Some("javascript"));
        assert!(!chunks.is_empty());
        let all_content: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(all_content.contains("function hello"));
        assert!(all_content.contains("class Greeter"));
    }

    #[test]
    fn test_go_tree_sitter_chunking() {
        let source = r#"
package main

import "fmt"

func main() {
    fmt.Println("hello")
}

func helper() int {
    return 42
}
"#;
        let chunks = chunk_source(source, "test.go", Some("go"));
        assert!(!chunks.is_empty());
        let all_content: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(all_content.contains("func main"));
        assert!(all_content.contains("func helper"));
    }

    #[test]
    fn test_kotlin_tree_sitter_chunking_small() {
        let source = r#"
package com.example

import kotlin.collections.List

class Foo {
    fun bar() = 42
}

object Singleton {
    val x: Int = 1
}

fun topLevel(): String = "hi"

typealias Name = String
"#;
        let chunks = chunk_source(source, "Foo.kt", Some("kotlin"));
        assert!(!chunks.is_empty());
        let all_content: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(all_content.contains("class Foo"));
        assert!(all_content.contains("object Singleton"));
        assert!(all_content.contains("fun topLevel"));
        assert!(all_content.contains("typealias Name"));
    }

    #[test]
    fn test_kotlin_tree_sitter_uses_definition_boundaries() {
        let body = "    val x = 1\n".repeat(80);
        let source =
            format!("class A {{\n{body}}}\n\nclass B {{\n{body}}}\n\nclass C {{\n{body}}}\n");
        let chunks = chunk_source(&source, "Big.kt", Some("kotlin"));
        assert!(
            chunks.len() >= 3,
            "large kotlin source should split by class: got {} chunks",
            chunks.len()
        );
        assert!(chunks[0].content.contains("class A"));
        assert!(
            !chunks[0].content.contains("class B"),
            "first chunk should end at the next top-level definition"
        );
        assert!(chunks[1].content.trim_start().starts_with("class B"));
    }

    #[test]
    fn test_ruby_tree_sitter_chunking() {
        let source = r#"
class Greeter
  def initialize(name)
    @name = name
  end

  def hello
    "Hello, #{@name}!"
  end
end

module Utils
  def self.upcase(s)
    s.upcase
  end
end

def standalone
  42
end
"#;
        let chunks = chunk_source(source, "test.rb", Some("ruby"));
        assert!(!chunks.is_empty());
        let all_content: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(all_content.contains("class Greeter"));
        assert!(all_content.contains("module Utils"));
        assert!(all_content.contains("def standalone"));
    }

    #[test]
    fn test_php_tree_sitter_chunking() {
        let source = r#"<?php
namespace App\Controller;

class UserController {
    public function index() {
        return 'list users';
    }

    public function show(int $id) {
        return "user $id";
    }
}

function helper() {
    return 1;
}
"#;
        let chunks = chunk_source(source, "test.php", Some("php"));
        assert!(!chunks.is_empty());
        let all_content: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(all_content.contains("class UserController"));
        assert!(all_content.contains("function helper"));
    }

    /// The fixture the package names: a file scoped namespace with several
    /// declarations, which must chunk at each one rather than as a single
    /// namespace body.
    const CSHARP_FIXTURE: &str = "using System.Text;\n\nnamespace Navex.Orders.Api;\n\npublic interface IOrderService\n{\n    OrderDto Get(int id);\n}\n\npublic class OrderService : IOrderService\n{\n    public OrderDto Get(int id) { return new OrderDto(); }\n}\n\npublic record OrderDto(int Id);\n\npublic struct Money\n{\n    public decimal Amount;\n}\n\npublic enum Status\n{\n    Open,\n    Closed\n}\n";

    #[test]
    fn test_csharp_chunks_at_declared_boundaries() {
        let chunks = chunk_source(CSHARP_FIXTURE, "OrderService.cs", Some("csharp"));

        let starts: Vec<&str> = chunks.iter().map(|c| c.content.trim_start()).collect();
        for declaration in [
            "public interface IOrderService",
            "public class OrderService",
            "public record OrderDto",
            "public struct Money",
            "public enum Status",
        ] {
            assert!(
                chunks.iter().any(|c| c.content.contains(declaration)),
                "{declaration} is missing from the chunks"
            );
            assert!(
                starts.iter().any(|s| s.starts_with(declaration))
                    || declaration == "public interface IOrderService",
                "{declaration} must begin a chunk, starts were {starts:?}"
            );
        }
    }

    /// A block scoped namespace leaves the root with one child. Reading only
    /// the root's children would return the whole file as a single chunk.
    #[test]
    fn test_csharp_descends_a_block_namespace() {
        let source = "namespace Navex.Orders.Api\n{\n    public class A\n    {\n    }\n\n    public class B\n    {\n    }\n}\n";
        let chunks = chunk_source(source, "Types.cs", Some("csharp"));

        assert!(
            chunks.len() >= 2,
            "a block namespace must not collapse to one chunk: {chunks:?}"
        );
    }

    /// The two parts of a partial class are separate declarations and stay
    /// separate chunks, even though both are small enough to merge.
    #[test]
    fn test_csharp_partial_class_is_one_chunk_per_part() {
        let source = "namespace Navex.Api;\n\npublic partial class Svc\n{\n    public void One() { }\n}\n\npublic partial class Svc\n{\n    public void Two() { }\n}\n";
        let chunks = chunk_source(source, "Svc.cs", Some("csharp"));

        let with_one = chunks
            .iter()
            .filter(|c| c.content.contains("One()"))
            .count();
        let with_both = chunks
            .iter()
            .filter(|c| c.content.contains("One()") && c.content.contains("Two()"))
            .count();

        assert_eq!(with_one, 1, "each part appears once: {chunks:?}");
        assert_eq!(
            with_both, 0,
            "the two parts must not merge into one chunk: {chunks:?}"
        );
    }

    #[test]
    fn test_html_chunks_at_element_boundaries() {
        let source = "<!DOCTYPE html>\n<html>\n<body>\n<section id=\"one\">\n  <p>first</p>\n</section>\n<section id=\"two\">\n  <p>second</p>\n</section>\n</body>\n</html>\n";
        let chunks = chunk_source(source, "page.html", Some("html"));

        assert!(!chunks.is_empty(), "html must chunk");
        assert!(
            chunks.iter().any(|c| c.content.contains("id=\"one\"")),
            "the first section must appear: {chunks:?}"
        );
        assert!(
            chunks.iter().any(|c| c.content.contains("id=\"two\"")),
            "the second section must appear: {chunks:?}"
        );
    }

    /// The whole page sits inside one root element, so a walk that read only
    /// the root's children would return the file as a single chunk.
    #[test]
    fn test_html_descends_into_the_root_element() {
        let filler = "  <li>an item that carries enough text to matter</li>\n".repeat(60);
        let source = format!(
            "<html>\n<body>\n<ul>\n{filler}</ul>\n<footer>the end</footer>\n</body>\n</html>\n"
        );
        let chunks = chunk_source(&source, "big.html", Some("html"));

        assert!(
            chunks.len() >= 2,
            "a large page must split rather than arrive whole: {} chunks",
            chunks.len()
        );
    }

    #[test]
    fn test_css_chunks_at_rule_boundaries() {
        let body = "  color: red;\n  background: white;\n  border: 1px solid black;\n".repeat(20);
        let source = format!(
            ".card {{\n{body}}}\n\n.panel {{\n{body}}}\n\n@media (max-width: 600px) {{\n  .card {{ display: none; }}\n}}\n"
        );
        let chunks = chunk_source(&source, "site.css", Some("css"));

        assert!(
            chunks.len() >= 2,
            "css must split at rule boundaries: {} chunks",
            chunks.len()
        );
        assert!(
            chunks[1].content.trim_start().starts_with(".panel"),
            "a chunk begins at a rule, not mid declaration: {:?}",
            chunks[1].content.chars().take(40).collect::<String>()
        );
    }

    #[test]
    fn test_swift_tree_sitter_chunking() {
        let source = r#"
import Foundation

struct User {
    let name: String
    let age: Int
}

class Greeter {
    func hello(to user: User) -> String {
        return "Hello, \(user.name)"
    }
}

func standalone() -> Int {
    return 42
}
"#;
        let chunks = chunk_source(source, "test.swift", Some("swift"));
        assert!(!chunks.is_empty());
        let all_content: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(all_content.contains("struct User"));
        assert!(all_content.contains("class Greeter"));
        assert!(all_content.contains("func standalone"));
    }
}
