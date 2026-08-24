//! C# symbols, usings, type references, and the resolution between them.
//!
//! C# is the one language here whose imports do not name a file. `using
//! Navex.Orders.Domain;` names a namespace, and a namespace is spread across
//! as many files as the author liked. Resolving it the way Java is resolved
//! would connect a file to every file in the namespace, which is not a
//! dependency, it is a neighbourhood.
//!
//! So a using contributes scope, not an edge. Edges come from type
//! references: a file depends on the file that declares a type it names, and
//! only when that type's namespace is in scope. That is what makes
//! `Order` in one namespace fail to resolve against an unrelated `Order` in
//! another, which is the whole reason to parse rather than grep.
//!
//! Three C# rules the resolution has to respect:
//!
//! - A namespace's ancestors are in scope. Code in `Navex.Orders.Api` sees
//!   `Navex.Orders` and `Navex` without a using.
//! - `global using` puts a namespace in scope for every file compiled with
//!   it, so a file can name a type without a using anywhere in it. Scope is
//!   therefore built across the project, not per file.
//! - Implicit usings do the same for framework namespaces. Nothing resolves
//!   there because `System.*` is not in the repository, which is correct: an
//!   edge to a file that does not exist is not an edge.

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use crate::engine::graph::{FileNode, Symbol};

/// The walker's name for this language.
pub const LANGUAGE: &str = "csharp";

/// A namespace put in scope by this file.
const NAMESPACE_USING: &str = "using:";

/// A namespace put in scope by every file in the project.
const GLOBAL_USING: &str = "gusing:";

/// A type named with at least one namespace segment, or a using static or
/// using alias, which name a type outright.
const QUALIFIED_REF: &str = "qref:";

/// A type named on its own, to be resolved through the file's scope.
const TYPE_REF: &str = "ref:";

/// The declaration kinds that can be the target of a type reference.
const TYPE_KINDS: [&str; 5] = ["class", "interface", "record", "struct", "enum"];

/// Every declaration this file makes, at any nesting depth.
///
/// Unlike the languages that declare at the top level, C# nests everything
/// inside a namespace and often inside a containing type, so this descends
/// rather than reading the root's children.
pub fn extract_symbols(source: &str, root: &Node) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    collect_symbols(source, root, &mut symbols);
    symbols
}

fn collect_symbols(source: &str, node: &Node, out: &mut Vec<Symbol>) {
    let kind = match node.kind() {
        "class_declaration" => Some("class"),
        "interface_declaration" => Some("interface"),
        "record_declaration" | "record_struct_declaration" => Some("record"),
        "struct_declaration" => Some("struct"),
        "enum_declaration" => Some("enum"),
        "method_declaration" => Some("method"),
        "constructor_declaration" => Some("constructor"),
        "property_declaration" => Some("property"),
        _ => None,
    };

    if let Some(kind) = kind
        && let Some(name) = node.child_by_field_name("name")
    {
        out.push(Symbol {
            name: source[name.byte_range()].to_string(),
            kind: kind.to_string(),
            line: node.start_position().row + 1,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols(source, &child, out);
    }
}

/// The namespace this file declares, block form or file scoped.
///
/// A file may declare more than one. The first is reported, which is what the
/// single package field on a node can hold and what all but a handful of real
/// files need.
pub fn extract_namespace(source: &str, root: &Node) -> Option<String> {
    fn find(source: &str, node: &Node) -> Option<String> {
        if matches!(
            node.kind(),
            "namespace_declaration" | "file_scoped_namespace_declaration"
        ) && let Some(name) = node.child_by_field_name("name")
        {
            return Some(source[name.byte_range()].to_string());
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = find(source, &child) {
                return Some(found);
            }
        }
        None
    }
    find(source, root)
}

/// Usings and type references, each tagged with what it is.
///
/// Tagging keeps one list on the node rather than four, matching how the rust
/// resolver already marks a module declaration.
pub fn extract_imports(source: &str, root: &Node) -> Vec<String> {
    let mut imports = Vec::new();
    collect_imports(source, root, &mut imports);
    imports.sort();
    imports.dedup();
    imports
}

fn collect_imports(source: &str, node: &Node, out: &mut Vec<String>) {
    match node.kind() {
        "using_directive" => {
            push_using(source, node, out);
            // A using names no types beyond its own target.
            return;
        }
        "base_list" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                push_type(source, &child, out);
            }
        }
        "parameter"
        | "variable_declaration"
        | "property_declaration"
        | "object_creation_expression" => {
            if let Some(ty) = node.child_by_field_name("type") {
                push_type(source, &ty, out);
            }
        }
        "method_declaration" => {
            if let Some(ty) = node.child_by_field_name("returns") {
                push_type(source, &ty, out);
            }
        }
        // A static call reads as Type.Member, so the left side is a type when
        // it is capitalised. An instance call has a lower case receiver and is
        // skipped, which costs nothing: the type it belongs to is already
        // named where the variable was declared.
        //
        // The name side is read too, because a generic method call carries its
        // type arguments there: `b.RegisterType<StatisticsService>()` names the
        // class only in that position. Miss it and a container wired codebase
        // has almost no edges into its implementations, so impact on a service
        // answers nothing and reads as safe to change.
        "member_access_expression" => {
            if let Some(expr) = node.child_by_field_name("expression") {
                push_type(source, &expr, out);
            }
            if let Some(name) = node.child_by_field_name("name")
                && name.kind() == "generic_name"
            {
                push_type(source, &name, out);
            }
        }
        // The same call written without a receiver, and any other bare generic
        // name in an expression.
        "invocation_expression" => {
            if let Some(function) = node.child_by_field_name("function")
                && function.kind() == "generic_name"
            {
                push_type(source, &function, out);
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_imports(source, &child, out);
    }
}

fn push_using(source: &str, node: &Node, out: &mut Vec<String>) {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).collect();

    let is_global = children.iter().any(|c| c.kind() == "global");
    // Both of these name a type outright rather than a namespace.
    let names_a_type = children.iter().any(|c| matches!(c.kind(), "static" | "="));

    let target = children
        .iter()
        .rev()
        .find(|c| matches!(c.kind(), "qualified_name" | "identifier"));
    let Some(target) = target else { return };
    let text = source[target.byte_range()].to_string();

    let tag = if names_a_type {
        QUALIFIED_REF
    } else if is_global {
        GLOBAL_USING
    } else {
        NAMESPACE_USING
    };
    out.push(format!("{tag}{text}"));
}

/// Record a type reference, unwrapping the type syntax around it.
fn push_type(source: &str, node: &Node, out: &mut Vec<String>) {
    match node.kind() {
        "identifier" => {
            let text = &source[node.byte_range()];
            if starts_upper(text) {
                out.push(format!("{TYPE_REF}{text}"));
            }
        }
        "qualified_name" => {
            out.push(format!("{QUALIFIED_REF}{}", &source[node.byte_range()]));
        }
        // List<Order> names both List and Order. RegisterType<Order> names
        // only Order, because the identifier there is a method. Both are
        // handled by pushing the identifier and the arguments and letting the
        // resolver drop what is not a declared type: a method name resolves to
        // nothing, which costs a lookup and never an edge.
        "generic_name" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "identifier" | "qualified_name" => push_type(source, &child, out),
                    "type_argument_list" => {
                        let mut inner = child.walk();
                        for arg in child.children(&mut inner) {
                            push_type(source, &arg, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        "nullable_type" | "array_type" | "pointer_type" => {
            if let Some(inner) = node.named_child(0) {
                push_type(source, &inner, out);
            }
        }
        _ => {}
    }
}

/// A C# identifier that could name a type. Locals and fields are camel or
/// underscore led by convention, so this drops them before they reach the
/// index rather than relying on a lookup miss.
fn starts_upper(text: &str) -> bool {
    text.chars().next().is_some_and(char::is_uppercase)
}

/// Every type the project declares, keyed by fully qualified name, plus the
/// namespaces that are in scope everywhere.
pub struct Resolver {
    /// Fully qualified type name to the files declaring it. Partial types put
    /// the same name in more than one file, which is why this is a list.
    types: HashMap<String, Vec<String>>,
    /// Namespaces in scope for every file, from `global using`.
    global_usings: Vec<String>,
}

impl Resolver {
    /// Build the project wide symbol table.
    pub fn build(files: &HashMap<String, FileNode>) -> Self {
        let mut types: HashMap<String, Vec<String>> = HashMap::new();
        let mut global_usings = HashSet::new();

        for (path, node) in files {
            if node.language != LANGUAGE {
                continue;
            }
            for import in &node.raw_imports {
                if let Some(ns) = import.strip_prefix(GLOBAL_USING) {
                    global_usings.insert(ns.to_string());
                }
            }
            for symbol in &node.symbols {
                if !TYPE_KINDS.contains(&symbol.kind.as_str()) {
                    continue;
                }
                let key = qualify(node.package_name.as_deref(), &symbol.name);
                types.entry(key).or_default().push(path.clone());
            }
        }

        let mut global_usings: Vec<String> = global_usings.into_iter().collect();
        global_usings.sort();

        Self {
            types,
            global_usings,
        }
    }

    /// The files `source_file` depends on.
    pub fn resolve(&self, source_file: &str, node: &FileNode) -> Vec<String> {
        let scope = self.scope_for(node);
        let mut found: Vec<String> = Vec::new();

        for import in &node.raw_imports {
            let candidates = if let Some(name) = import.strip_prefix(TYPE_REF) {
                self.lookup_in_scope(name, &scope)
            } else if let Some(qualified) = import.strip_prefix(QUALIFIED_REF) {
                self.lookup_qualified(qualified, &scope)
            } else {
                // A namespace using contributes scope, not an edge.
                continue;
            };

            for candidate in candidates {
                if candidate != source_file && !found.contains(&candidate) {
                    found.push(candidate);
                }
            }
        }

        found.sort();
        found
    }

    /// Namespaces this file can name a type from without qualifying it.
    fn scope_for(&self, node: &FileNode) -> Vec<String> {
        let mut scope: Vec<String> = Vec::new();

        // The global namespace, for a file that declares no namespace.
        scope.push(String::new());

        // A namespace and each of its ancestors are in scope.
        if let Some(own) = node.package_name.as_deref() {
            let mut prefix = String::new();
            for segment in own.split('.') {
                if !prefix.is_empty() {
                    prefix.push('.');
                }
                prefix.push_str(segment);
                scope.push(prefix.clone());
            }
        }

        for import in &node.raw_imports {
            if let Some(ns) = import.strip_prefix(NAMESPACE_USING) {
                scope.push(ns.to_string());
            }
        }

        scope.extend(self.global_usings.iter().cloned());
        scope.sort();
        scope.dedup();
        scope
    }

    /// A bare type name, tried against every namespace in scope.
    fn lookup_in_scope(&self, name: &str, scope: &[String]) -> Vec<String> {
        for namespace in scope {
            let key = qualify(
                if namespace.is_empty() {
                    None
                } else {
                    Some(namespace)
                },
                name,
            );
            if let Some(paths) = self.types.get(&key) {
                return paths.clone();
            }
        }
        Vec::new()
    }

    /// A dotted reference: fully qualified first, then read as relative to a
    /// namespace in scope, which is how `using Navex;` lets code write
    /// `Orders.Order`.
    fn lookup_qualified(&self, qualified: &str, scope: &[String]) -> Vec<String> {
        if let Some(paths) = self.types.get(qualified) {
            return paths.clone();
        }
        for namespace in scope {
            if namespace.is_empty() {
                continue;
            }
            if let Some(paths) = self.types.get(&format!("{namespace}.{qualified}")) {
                return paths.clone();
            }
        }
        Vec::new()
    }
}

/// A namespace and a name as one fully qualified key.
fn qualify(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => format!("{ns}.{name}"),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::graph::DependencyGraph;

    fn graph(files: &[(&str, &str)]) -> DependencyGraph {
        let mut graph = DependencyGraph::new();
        for (path, source) in files {
            graph.add_file(path, source, LANGUAGE);
        }
        graph.resolve_dependencies();
        graph
    }

    /// Two projects. Orders declares the types, Api consumes them, and Billing
    /// is the unrelated namespace that must not be resolved into.
    fn two_projects() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "src/Navex.Shared/GlobalUsings.cs",
                "global using Navex.Shared.Contracts;\n",
            ),
            (
                "src/Navex.Shared/Money.cs",
                "namespace Navex.Shared.Contracts;\n\npublic record Money(decimal Amount);\n",
            ),
            (
                "src/Navex.Orders/Order.cs",
                "namespace Navex.Orders.Domain;\n\npublic class Order\n{\n    public int Id { get; set; }\n}\n",
            ),
            (
                "src/Navex.Orders/OrderRepository.cs",
                "namespace Navex.Orders.Domain;\n\npublic class OrderRepository\n{\n    public Order Get(int id) { return new Order(); }\n}\n",
            ),
            (
                "src/Navex.Billing/Order.cs",
                "namespace Navex.Billing.Legacy;\n\npublic class Order\n{\n}\n",
            ),
            (
                "src/Navex.Api/OrderService.cs",
                "using Navex.Orders.Domain;\n\nnamespace Navex.Api;\n\npublic class OrderService\n{\n    private readonly OrderRepository _repo;\n    public Money Total() { return new Money(0m); }\n}\n",
            ),
        ]
    }

    #[test]
    fn a_caller_resolves_to_the_file_declaring_the_type() {
        let graph = graph(&two_projects());

        let deps = graph
            .deps("src/Navex.Api/OrderService.cs")
            .expect("the caller is in the graph");

        assert!(
            deps.depends_on
                .contains(&"src/Navex.Orders/OrderRepository.cs".to_string()),
            "the using puts the namespace in scope and the field names the type: {:?}",
            deps.depends_on
        );
    }

    #[test]
    fn a_type_reached_through_a_global_using_resolves() {
        let graph = graph(&two_projects());

        let deps = graph
            .deps("src/Navex.Api/OrderService.cs")
            .expect("the caller is in the graph");

        // OrderService names Money without a using of its own. The only thing
        // putting Navex.Shared.Contracts in scope is the global using in
        // another file entirely.
        assert!(
            deps.depends_on
                .contains(&"src/Navex.Shared/Money.cs".to_string()),
            "a global using is project wide scope: {:?}",
            deps.depends_on
        );
    }

    #[test]
    fn a_name_matching_in_an_unrelated_namespace_does_not_resolve() {
        let graph = graph(&two_projects());

        let deps = graph
            .deps("src/Navex.Api/OrderService.cs")
            .expect("the caller is in the graph");

        // Navex.Billing.Legacy also declares an Order, and nothing puts it in
        // scope. Textual matching would link it; scope must not.
        assert!(
            !deps
                .depends_on
                .contains(&"src/Navex.Billing/Order.cs".to_string()),
            "an out of scope namespace must not be reached: {:?}",
            deps.depends_on
        );
    }

    #[test]
    fn impact_on_a_type_names_its_callers_and_no_more() {
        let graph = graph(&two_projects());

        let reached = graph.impact("src/Navex.Orders/OrderRepository.cs");

        assert_eq!(
            reached,
            vec!["src/Navex.Api/OrderService.cs".to_string()],
            "only the caller depends on the repository"
        );
    }

    /// A using alias and a using static both name a type outright, so they are
    /// edges in their own right rather than scope.
    #[test]
    fn a_using_static_and_a_using_alias_resolve_to_the_type_they_name() {
        let graph = graph(&[
            (
                "src/Helpers.cs",
                "namespace Navex.Shared;\n\npublic static class Helpers\n{\n    public static int One() { return 1; }\n}\n",
            ),
            (
                "src/Invoice.cs",
                "namespace Navex.Billing;\n\npublic class Invoice\n{\n}\n",
            ),
            (
                "src/Caller.cs",
                "using static Navex.Shared.Helpers;\nusing Doc = Navex.Billing.Invoice;\n\nnamespace Navex.Api;\n\npublic class Caller\n{\n}\n",
            ),
        ]);

        let deps = graph
            .deps("src/Caller.cs")
            .expect("the caller is in the graph");

        assert!(deps.depends_on.contains(&"src/Helpers.cs".to_string()));
        assert!(deps.depends_on.contains(&"src/Invoice.cs".to_string()));
    }

    /// An ancestor namespace is in scope without a using, which is a C# rule
    /// that no other language here has.
    #[test]
    fn an_ancestor_namespace_is_in_scope_without_a_using() {
        let graph = graph(&[
            (
                "src/Clock.cs",
                "namespace Navex.Platform;\n\npublic interface IClock\n{\n}\n",
            ),
            (
                "src/Job.cs",
                "namespace Navex.Platform.Jobs;\n\npublic class Job\n{\n    private readonly IClock _clock;\n}\n",
            ),
        ]);

        let deps = graph.deps("src/Job.cs").expect("the file is in the graph");

        assert!(
            deps.depends_on.contains(&"src/Clock.cs".to_string()),
            "Navex.Platform is an ancestor of Navex.Platform.Jobs: {:?}",
            deps.depends_on
        );
    }

    /// A partial type lives in more than one file, so a caller depends on
    /// every part: changing either one reaches it.
    /// A dependency injection container names the concrete class only as a
    /// generic type argument. Miss those and a DI heavy codebase has almost no
    /// edges into its implementations, so `impact` on a service reports
    /// nothing and reads as safe to change.
    #[test]
    fn a_generic_type_argument_is_a_type_reference() {
        let graph = graph(&[
            (
                "src/StatisticsService.cs",
                "namespace App.Services;\n\npublic class StatisticsService\n{\n}\n",
            ),
            (
                "src/DependencyBuilder.cs",
                "using App.Services;\n\nnamespace App.Wiring;\n\npublic class DependencyBuilder\n{\n                 \x20   public void Register(ContainerBuilder b)\n    {\n                 \x20       b.RegisterType<StatisticsService>().As<IStatisticsService>();\n    }\n}\n",
            ),
        ]);

        let deps = graph
            .deps("src/DependencyBuilder.cs")
            .expect("the wiring file is in the graph");
        assert!(
            deps.depends_on
                .contains(&"src/StatisticsService.cs".to_string()),
            "a registration names the class it registers: {:?}",
            deps.depends_on
        );

        assert_eq!(
            graph.impact("src/StatisticsService.cs"),
            vec!["src/DependencyBuilder.cs".to_string()],
            "impact must reach the file that registers it"
        );
    }

    #[test]
    fn a_partial_type_resolves_to_every_part() {
        let graph = graph(&[
            (
                "src/Svc.Core.cs",
                "namespace Navex.Api;\n\npublic partial class Svc\n{\n}\n",
            ),
            (
                "src/Svc.Extra.cs",
                "namespace Navex.Api;\n\npublic partial class Svc\n{\n}\n",
            ),
            (
                "src/Caller.cs",
                "namespace Navex.Api;\n\npublic class Caller\n{\n    private readonly Svc _svc;\n}\n",
            ),
        ]);

        let deps = graph
            .deps("src/Caller.cs")
            .expect("the caller is in the graph");

        assert!(deps.depends_on.contains(&"src/Svc.Core.cs".to_string()));
        assert!(deps.depends_on.contains(&"src/Svc.Extra.cs".to_string()));
    }

    /// A namespace using is scope, not a dependency. Importing a namespace and
    /// naming nothing from it must produce no edge, or every file in a
    /// namespace would depend on every other.
    #[test]
    fn a_using_that_names_no_type_is_not_a_dependency() {
        let graph = graph(&[
            (
                "src/Order.cs",
                "namespace Navex.Orders.Domain;\n\npublic class Order\n{\n}\n",
            ),
            (
                "src/Unrelated.cs",
                "using Navex.Orders.Domain;\n\nnamespace Navex.Api;\n\npublic class Unrelated\n{\n}\n",
            ),
        ]);

        let deps = graph
            .deps("src/Unrelated.cs")
            .expect("the file is in the graph");

        assert!(
            deps.depends_on.is_empty(),
            "a using alone is not a dependency: {:?}",
            deps.depends_on
        );
    }

    #[test]
    fn symbols_are_found_inside_a_block_namespace() {
        let source = "namespace Navex.Api\n{\n    public class Svc\n    {\n        public void Run() { }\n        public int Count { get; set; }\n    }\n}\n";
        let mut graph = DependencyGraph::new();
        graph.add_file("src/Svc.cs", source, LANGUAGE);

        let node = graph.deps("src/Svc.cs").expect("the file is in the graph");
        let names: Vec<&str> = node.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"Svc"), "{names:?}");
        assert!(names.contains(&"Run"), "{names:?}");
        assert!(names.contains(&"Count"), "{names:?}");
        assert_eq!(node.package_name.as_deref(), Some("Navex.Api"));
    }

    #[test]
    fn a_file_scoped_namespace_is_read_the_same_as_a_block_one() {
        let source = "namespace Navex.Api;\n\npublic class Svc\n{\n}\n";
        let mut graph = DependencyGraph::new();
        graph.add_file("src/Svc.cs", source, LANGUAGE);

        let node = graph.deps("src/Svc.cs").expect("the file is in the graph");
        assert_eq!(node.package_name.as_deref(), Some("Navex.Api"));
    }
}
