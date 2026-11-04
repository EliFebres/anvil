//! Tree-sitter outlines: which node kinds are items, imports or containers in
//! each language, and where their doc comments live.

use std::path::Path;

use tree_sitter::{Node, Parser};

use super::Item;

/// A language parsed with tree-sitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    /// `.rs`
    Rust,
    /// `.py`, `.pyi`
    Python,
    /// `.ts`, `.mts`, `.cts`
    TypeScript,
    /// `.tsx`
    Tsx,
    /// `.js`, `.jsx`, `.mjs`, `.cjs`
    JavaScript,
    /// `.toml`
    Toml,
    /// `.md`, `.markdown`
    Markdown,
}

impl Language {
    /// The language of a path, by extension.
    ///
    /// # Arguments
    ///
    /// * `path` - The file's path.
    ///
    /// # Returns
    ///
    /// The language, or `None` when no grammar here covers it.
    pub fn of(path: &Path) -> Option<Language> {
        let ext = path.extension()?.to_str()?;
        Some(match ext {
            "rs" => Language::Rust,
            "py" | "pyi" => Language::Python,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "toml" => Language::Toml,
            "md" | "markdown" => Language::Markdown,
            _ => return None,
        })
    }

    /// The tree-sitter grammar.
    fn grammar(self) -> tree_sitter::Language {
        match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Language::Markdown => tree_sitter_md::LANGUAGE.into(),
        }
    }

    /// What a node of this kind is in an outline.
    fn role(self, kind: &str) -> Role {
        match (self, kind) {
            (Language::Rust, "use_declaration" | "extern_crate_declaration") => Role::Import,
            (Language::Rust, "impl_item" | "trait_item" | "mod_item") => Role::Container,
            (
                Language::Rust,
                "function_item"
                | "function_signature_item"
                | "struct_item"
                | "enum_item"
                | "union_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "macro_definition",
            ) => Role::Leaf,

            (
                Language::Python,
                "import_statement" | "import_from_statement" | "future_import_statement",
            ) => Role::Import,
            (Language::Python, "class_definition") => Role::Container,
            (Language::Python, "function_definition") => Role::Leaf,
            (Language::Python, "decorated_definition") => Role::Wrapper,

            (Language::TypeScript | Language::Tsx | Language::JavaScript, kind) => match kind {
                "import_statement" => Role::Import,
                "class_declaration"
                | "abstract_class_declaration"
                | "class"
                | "internal_module"
                | "module" => Role::Container,
                "function_declaration"
                | "generator_function_declaration"
                | "method_definition"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "abstract_method_signature"
                | "function_signature" => Role::Leaf,
                "lexical_declaration" | "variable_declaration" => Role::Binding,
                "export_statement" | "expression_statement" => Role::Wrapper,
                _ => Role::Skip,
            },

            (Language::Toml, "table" | "table_array_element") => Role::Leaf,
            (Language::Markdown, "section") => Role::Section,
            _ => Role::Skip,
        }
    }
}

/// What a syntax node contributes to an outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// An import, collapsed with its neighbours into one line.
    Import,
    /// An item whose body holds more items.
    Container,
    /// An item with no nested items.
    Leaf,
    /// A node around an item (an export, a decorator) that is not one itself.
    Wrapper,
    /// `const f = () => …`: an item only when it binds a function.
    Binding,
    /// A Markdown heading's section.
    Section,
    /// Anything else.
    Skip,
}

/// Parses `text` and outlines it, or `None` when the grammar cannot load.
pub(super) fn outline(language: Language, text: &str) -> Option<Vec<Item>> {
    let mut parser = Parser::new();
    parser.set_language(&language.grammar()).ok()?;
    let tree = parser.parse(text, None)?;
    let walker = Walker { language, text };
    Some(walker.items(tree.root_node()))
}

/// Walks one parsed file.
struct Walker<'t> {
    language: Language,
    text: &'t str,
}

impl Walker<'_> {
    /// The items among `parent`'s named children, imports collapsed.
    fn items(&self, parent: Node<'_>) -> Vec<Item> {
        let mut items = Vec::new();
        let mut imports: Option<(usize, usize, usize)> = None;
        let mut cursor = parent.walk();
        for child in parent.named_children(&mut cursor) {
            if self.language.role(child.kind()) == Role::Import {
                let (start, _, count) = imports.unwrap_or((first_line(child), 0, 0));
                imports = Some((start, last_line(child), count + 1));
                continue;
            }
            if let Some((start, end, count)) = imports.take() {
                items.push(Item::imports(start, end, count));
            }
            items.extend(self.item(child, child));
        }
        if let Some((start, end, count)) = imports {
            items.push(Item::imports(start, end, count));
        }
        items
    }

    /// The item `node` stands for, if any; `anchor` is the outermost node
    /// around it, whose position and preceding comments count.
    fn item(&self, node: Node<'_>, anchor: Node<'_>) -> Option<Item> {
        match self.language.role(node.kind()) {
            Role::Leaf => Some(self.leaf(node, anchor)),
            Role::Container => {
                let mut item = self.leaf(node, anchor);
                if let Some(body) = node.child_by_field_name("body") {
                    item.children = self.items(body);
                }
                Some(item)
            }
            Role::Wrapper => {
                let mut cursor = node.walk();
                let inner = node
                    .named_children(&mut cursor)
                    .find(|child| self.language.role(child.kind()) != Role::Skip)?;
                self.item(inner, anchor)
            }
            Role::Binding => self.binds_function(node).then(|| self.leaf(node, anchor)),
            Role::Section => Some(self.section(node)),
            Role::Import | Role::Skip => None,
        }
    }

    /// An item with no children: signature, range and doc.
    fn leaf(&self, node: Node<'_>, anchor: Node<'_>) -> Item {
        let signature = self.slice(node).lines().next().unwrap_or_default();
        Item::new(
            signature,
            first_line(anchor),
            last_line(anchor),
            self.doc(node, anchor),
        )
    }

    /// A Markdown section: its heading, with nested sections as children.
    fn section(&self, node: Node<'_>) -> Item {
        let heading = node
            .named_child(0)
            .filter(|child| child.kind().ends_with("heading"))
            .map(|child| {
                self.slice(child)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .unwrap_or_default();
        let mut item = Item::new(&heading, first_line(node), last_line(node), None);
        let mut cursor = node.walk();
        item.children = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "section")
            .map(|child| self.section(child))
            .collect();
        item
    }

    /// Whether a `const`/`let`/`var` declaration binds a function.
    fn binds_function(&self, node: Node<'_>) -> bool {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).any(|declarator| {
            declarator
                .child_by_field_name("value")
                .is_some_and(|value| {
                    matches!(
                        value.kind(),
                        "arrow_function" | "function_expression" | "function" | "class"
                    )
                })
        })
    }

    /// The first line of the item's doc comment or docstring.
    fn doc(&self, node: Node<'_>, anchor: Node<'_>) -> Option<String> {
        if self.language == Language::Python {
            return self.docstring(node);
        }
        let mut comments = Vec::new();
        let mut previous = anchor.prev_named_sibling();
        while let Some(sibling) = previous {
            if sibling.kind() == "attribute_item" || sibling.kind() == "decorator" {
                previous = sibling.prev_named_sibling();
                continue;
            }
            if !sibling.kind().contains("comment") {
                break;
            }
            comments.push(sibling);
            previous = sibling.prev_named_sibling();
        }
        let first = comments.into_iter().rev().find_map(|comment| {
            let text = self.slice(comment).trim();
            let is_doc = text.starts_with("///") && !text.starts_with("////")
                || text.starts_with("/**")
                || self.language == Language::Toml && text.starts_with('#');
            is_doc.then(|| doc_text(text))
        })?;
        Some(first)
    }

    /// The first line of a Python docstring: the body's leading string.
    fn docstring(&self, node: Node<'_>) -> Option<String> {
        let body = node.child_by_field_name("body")?;
        let statement = body.named_child(0)?;
        let string = statement
            .named_child(0)
            .filter(|_| statement.kind() == "expression_statement")
            .filter(|child| child.kind() == "string")?;
        let text = self
            .slice(string)
            .trim_start_matches(['r', 'u', 'b', 'R', 'U', 'B']);
        let text = text.trim_matches(|c| c == '"' || c == '\'');
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned)
    }

    /// The source text of a node.
    fn slice(&self, node: Node<'_>) -> &str {
        self.text.get(node.byte_range()).unwrap_or_default()
    }
}

/// The first content line of a comment, markers removed.
fn doc_text(comment: &str) -> String {
    comment
        .lines()
        .map(|line| {
            line.trim()
                .trim_start_matches("/**")
                .trim_start_matches("///")
                .trim_start_matches("//!")
                .trim_start_matches('#')
                .trim_end_matches("*/")
                .trim_start_matches('*')
                .trim()
        })
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// A node's first line, 1-based.
fn first_line(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

/// A node's last line, 1-based; a node ending at column 0 ends on the line before.
fn last_line(node: Node<'_>) -> usize {
    let end = node.end_position();
    let row = if end.column == 0 && end.row > node.start_position().row {
        end.row
    } else {
        end.row + 1
    };
    row.max(first_line(node))
}
