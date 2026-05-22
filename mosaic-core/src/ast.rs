//! AST layer (M5 kernel).
//!
//! Parses source code via tree-sitter into an `AstTree`, exposes
//! content-addressable subtree hashes (so identical-shaped subtrees in two
//! versions are recognizable as "the same"), and surfaces top-level named
//! definitions (functions, methods, types). Higher layers use this for
//! semantic-aware merge — for example, recognizing that one branch renamed
//! `chargeCard` to `processCharge` and propagating that rename through
//! another branch's edits.

use crate::error::{Error, Result};
use crate::hash::{Hash, Hasher};
use std::collections::BTreeMap;
use tree_sitter::{Language, Node, Parser, Tree};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Go,
    Java,
    C,
    Ruby,
    JavaScript,
}

impl Lang {
    pub fn from_path(path: &str) -> Option<Self> {
        let ext = path.rsplit_once('.').map(|x| x.1.to_ascii_lowercase());
        match ext.as_deref() {
            Some("rs") => Some(Self::Rust),
            Some("py") => Some(Self::Python),
            Some("ts") | Some("tsx") => Some(Self::TypeScript),
            Some("go") => Some(Self::Go),
            Some("java") => Some(Self::Java),
            Some("c") | Some("h") => Some(Self::C),
            Some("rb") => Some(Self::Ruby),
            Some("js") | Some("jsx") | Some("mjs") => Some(Self::JavaScript),
            _ => None,
        }
    }

    fn ts_language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::language(),
            Self::Python => tree_sitter_python::language(),
            Self::TypeScript => tree_sitter_typescript::language_typescript(),
            Self::Go => tree_sitter_go::language(),
            Self::Java => tree_sitter_java::language(),
            Self::C => tree_sitter_c::language(),
            Self::Ruby => tree_sitter_ruby::language(),
            Self::JavaScript => tree_sitter_javascript::language(),
        }
    }

    /// Node kind names that identify "top-level definitions" — the unit at
    /// which we recognize renames and structural moves.
    fn def_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["function_item", "struct_item", "enum_item", "impl_item", "trait_item"],
            Self::Python => &["function_definition", "class_definition"],
            Self::TypeScript => &[
                "function_declaration",
                "class_declaration",
                "interface_declaration",
                "method_definition",
            ],
            Self::Go => &["function_declaration", "method_declaration", "type_declaration"],
            Self::Java => &[
                "class_declaration",
                "method_declaration",
                "interface_declaration",
                "enum_declaration",
            ],
            Self::C => &["function_definition", "struct_specifier", "enum_specifier"],
            Self::Ruby => &["method", "class", "module"],
            Self::JavaScript => &["function_declaration", "class_declaration", "method_definition"],
        }
    }
}

pub struct AstTree {
    lang: Lang,
    tree: Tree,
    source: Vec<u8>,
}

impl AstTree {
    pub fn parse(lang: Lang, source: &[u8]) -> Result<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&lang.ts_language())
            .map_err(|e| Error::Serialization(format!("set_language failed: {e}")))?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| Error::Serialization("tree-sitter returned no tree".into()))?;
        Ok(Self {
            lang,
            tree,
            source: source.to_vec(),
        })
    }

    pub fn lang(&self) -> Lang {
        self.lang
    }

    pub fn source(&self) -> &[u8] {
        &self.source
    }

    /// The root tree-sitter node. Exposed so structural-diff layers (e.g.
    /// `gumtree`) can walk the parse tree directly.
    pub fn root_node(&self) -> Node<'_> {
        self.tree.root_node()
    }

    pub fn root_hash(&self) -> Hash {
        node_hash(self.tree.root_node(), &self.source)
    }

    pub fn has_errors(&self) -> bool {
        self.tree.root_node().has_error()
    }

    /// Map from top-level definition name to its body hash. Two trees share
    /// an entry when a definition has the same name; comparing the hashes
    /// tells us whether the body is identical or has been edited.
    pub fn definitions(&self) -> BTreeMap<String, DefInfo> {
        let mut out = BTreeMap::new();
        let mut cursor = self.tree.walk();
        let root = self.tree.root_node();
        for child in root.children(&mut cursor) {
            if !self.lang.def_kinds().contains(&child.kind()) {
                continue;
            }
            if let Some((name, info)) = self.def_info(child) {
                out.insert(name, info);
            }
        }
        out
    }

    fn def_info(&self, node: Node<'_>) -> Option<(String, DefInfo)> {
        // Every supported grammar exposes the definition name under the
        // `name` field, except a few (C `function_definition`, Go
        // `type_declaration`) where the name is nested inside a declarator or
        // spec. For those we fall back to scanning for the first
        // identifier-like descendant. If nothing is found we skip the def.
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| first_identifier(node))?;
        let name_bytes = &self.source[name_node.start_byte()..name_node.end_byte()];
        let name = String::from_utf8_lossy(name_bytes).to_string();
        Some((
            name,
            DefInfo {
                kind: node.kind().to_string(),
                body_hash: node_hash_excluding(node, name_node.id(), &self.source),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            },
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefInfo {
    pub kind: String,
    pub body_hash: Hash,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Find the first identifier-like descendant of `node` in pre-order. Used as a
/// fallback for grammars where a definition's name is not exposed via a `name`
/// field (e.g. C `function_definition`, whose name is nested in the
/// declarator, or Go `type_declaration`, whose name lives in a `type_spec`).
fn first_identifier<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" | "field_identifier" | "type_identifier" => return Some(child),
            _ => {
                if let Some(found) = first_identifier(child) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn node_hash(node: Node<'_>, source: &[u8]) -> Hash {
    let mut h = Hasher::new();
    h.update(b"mosaic.ast.v1");
    write_node(node, source, None, &mut h);
    h.finalize()
}

fn node_hash_excluding(node: Node<'_>, exclude_id: usize, source: &[u8]) -> Hash {
    let mut h = Hasher::new();
    h.update(b"mosaic.ast.v1.bodyonly");
    write_node(node, source, Some(exclude_id), &mut h);
    h.finalize()
}

fn write_node(node: Node<'_>, source: &[u8], exclude_id: Option<usize>, h: &mut Hasher) {
    if Some(node.id()) == exclude_id {
        h.update(b"<excluded>");
        return;
    }
    h.update(node.kind().as_bytes());
    h.update(b"\x00");
    if node.child_count() == 0 {
        let bytes = &source[node.start_byte()..node.end_byte()];
        h.update(&(bytes.len() as u32).to_le_bytes());
        h.update(bytes);
        h.update(b"\x00");
        return;
    }
    h.update(b"(");
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        write_node(child, source, exclude_id, h);
    }
    h.update(b")");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rust_no_errors() {
        let ast = AstTree::parse(Lang::Rust, b"fn main() { println!(\"hi\"); }").unwrap();
        assert!(!ast.has_errors());
    }

    #[test]
    fn extracts_top_level_definitions() {
        let src = b"
            fn alpha() { 1 + 1; }
            fn beta() -> u32 { 42 }
            struct Point { x: f64, y: f64 }
        ";
        let ast = AstTree::parse(Lang::Rust, src).unwrap();
        let defs = ast.definitions();
        let names: Vec<&String> = defs.keys().collect();
        assert!(names.iter().any(|n| n.as_str() == "alpha"));
        assert!(names.iter().any(|n| n.as_str() == "beta"));
        assert!(names.iter().any(|n| n.as_str() == "Point"));
    }

    #[test]
    fn identical_source_gives_identical_root_hash() {
        let src = b"fn x() -> u32 { 7 }";
        let a = AstTree::parse(Lang::Rust, src).unwrap().root_hash();
        let b = AstTree::parse(Lang::Rust, src).unwrap().root_hash();
        assert_eq!(a, b);
    }

    #[test]
    fn whitespace_only_change_keeps_same_hashes() {
        let src_a = b"fn x()->u32{7}";
        let src_b = b"fn  x ( )  ->  u32  {  7  }";
        let a = AstTree::parse(Lang::Rust, src_a).unwrap();
        let b = AstTree::parse(Lang::Rust, src_b).unwrap();
        let da = a.definitions();
        let db = b.definitions();
        assert_eq!(
            da.get("x").map(|d| &d.body_hash),
            db.get("x").map(|d| &d.body_hash),
            "tree-sitter parse trees should be identical modulo whitespace"
        );
    }

    #[test]
    fn body_edit_changes_body_hash_only() {
        let src_a = b"fn x() -> u32 { 7 }\nfn y() -> u32 { 8 }";
        let src_b = b"fn x() -> u32 { 9 }\nfn y() -> u32 { 8 }";
        let a = AstTree::parse(Lang::Rust, src_a).unwrap();
        let b = AstTree::parse(Lang::Rust, src_b).unwrap();
        let da = a.definitions();
        let db = b.definitions();
        assert_ne!(da["x"].body_hash, db["x"].body_hash);
        assert_eq!(da["y"].body_hash, db["y"].body_hash);
    }

    #[test]
    fn python_definitions() {
        let src = b"
def alpha(x):
    return x + 1

class Beta:
    def greet(self):
        return 'hi'
";
        let ast = AstTree::parse(Lang::Python, src).unwrap();
        let defs = ast.definitions();
        assert!(defs.contains_key("alpha"));
        assert!(defs.contains_key("Beta"));
    }

    #[test]
    fn typescript_definitions() {
        let src = b"
            function greet(name: string): string { return 'hi ' + name; }
            class Box { open() {} }
            interface Shape { area(): number; }
        ";
        let ast = AstTree::parse(Lang::TypeScript, src).unwrap();
        let defs = ast.definitions();
        assert!(defs.contains_key("greet"));
        assert!(defs.contains_key("Box"));
        assert!(defs.contains_key("Shape"));
    }

    #[test]
    fn go_definitions() {
        let src = b"package main\nfunc Alpha() {}\nfunc Beta() int { return 1 }\ntype Gamma struct { x int }\n";
        let ast = AstTree::parse(Lang::Go, src).unwrap();
        let defs = ast.definitions();
        assert!(defs.contains_key("Alpha"), "got: {:?}", defs.keys().collect::<Vec<_>>());
        assert!(defs.contains_key("Beta"));
        assert!(defs.contains_key("Gamma"));
    }

    #[test]
    fn java_definitions() {
        let src = b"class Foo { void bar() {} }\ninterface Shape { int area(); }\n";
        let ast = AstTree::parse(Lang::Java, src).unwrap();
        let defs = ast.definitions();
        assert!(defs.contains_key("Foo"), "got: {:?}", defs.keys().collect::<Vec<_>>());
        assert!(defs.contains_key("Shape"));
    }

    #[test]
    fn c_definitions() {
        let src = b"int add(int a, int b) { return a + b; }\nstruct Point { double x; double y; };\n";
        let ast = AstTree::parse(Lang::C, src).unwrap();
        let defs = ast.definitions();
        assert!(defs.contains_key("add"), "got: {:?}", defs.keys().collect::<Vec<_>>());
        assert!(defs.contains_key("Point"));
    }

    #[test]
    fn ruby_definitions() {
        let src = b"def greet\n  1\nend\nclass Box\nend\nmodule Util\nend\n";
        let ast = AstTree::parse(Lang::Ruby, src).unwrap();
        let defs = ast.definitions();
        assert!(defs.contains_key("greet"), "got: {:?}", defs.keys().collect::<Vec<_>>());
        assert!(defs.contains_key("Box"));
        assert!(defs.contains_key("Util"));
    }

    #[test]
    fn javascript_definitions() {
        let src = b"function greet(){ return 1; }\nclass Box { open() {} }\n";
        let ast = AstTree::parse(Lang::JavaScript, src).unwrap();
        let defs = ast.definitions();
        assert!(defs.contains_key("greet"), "got: {:?}", defs.keys().collect::<Vec<_>>());
        assert!(defs.contains_key("Box"));
    }

    #[test]
    fn lang_from_path() {
        assert_eq!(Lang::from_path("src/main.rs"), Some(Lang::Rust));
        assert_eq!(Lang::from_path("script.py"), Some(Lang::Python));
        assert_eq!(Lang::from_path("ui/App.tsx"), Some(Lang::TypeScript));
        assert_eq!(Lang::from_path("server/main.go"), Some(Lang::Go));
        assert_eq!(Lang::from_path("src/Main.java"), Some(Lang::Java));
        assert_eq!(Lang::from_path("lib/util.c"), Some(Lang::C));
        assert_eq!(Lang::from_path("lib/util.h"), Some(Lang::C));
        assert_eq!(Lang::from_path("app/box.rb"), Some(Lang::Ruby));
        assert_eq!(Lang::from_path("ui/app.js"), Some(Lang::JavaScript));
        assert_eq!(Lang::from_path("ui/App.jsx"), Some(Lang::JavaScript));
        assert_eq!(Lang::from_path("ui/app.mjs"), Some(Lang::JavaScript));
        assert_eq!(Lang::from_path("README.md"), None);
    }
}
