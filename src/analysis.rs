use crate::model::{CodeUnit, Diagnostic, Language};
use anyhow::{Context, Result, anyhow};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::Path;
use tree_sitter::{Node, Parser};

/// Every function found under a root, with counts of the files behind them.
#[derive(Debug, Default)]
pub struct Analysis {
    /// One per named function, sorted by file, then start line, then symbol.
    pub units: Vec<CodeUnit>,
    /// Files of an enabled language that survived the excludes.
    pub candidate_files: usize,
    /// Candidates whose syntax tree had no errors.
    pub parsed_files: usize,
    /// One entry per file that failed to read or parse.
    pub diagnostics: Vec<Diagnostic>,
    /// Files of an enabled language that the exclude globs dropped. The
    /// coverage scope accounts for them: a report that covers a test file
    /// the excludes skipped is not a mismatch.
    pub excluded_files: Vec<std::path::PathBuf>,
}

/// What a walk of the tree found: the files to analyze, and the ones the
/// exclude globs dropped.
struct Discovered {
    analyze: Vec<(std::path::PathBuf, Language)>,
    excluded: Vec<std::path::PathBuf>,
}

struct FileAnalysis {
    units: Vec<CodeUnit>,
    diagnostic: Option<Diagnostic>,
}

struct UnitSpec<'tree> {
    node: Node<'tree>,
    name: String,
}

/// Parse every file of the given languages under `root`, minus the exclude
/// globs, which match paths relative to `root`.
///
/// `.gitignore` and hidden directories are honored. No excludes are applied
/// beyond the ones passed in; see [`crate::config::DEFAULT_EXCLUDES`].
pub fn analyze_tree(root: &Path, languages: &[Language], excludes: &[String]) -> Result<Analysis> {
    validate_root(root)?;
    let exclude_set = build_globs(excludes)?;
    let enabled: HashSet<_> = languages.iter().copied().collect();
    let found = discover_paths(root, &enabled, &exclude_set);
    analyze_discovered(found.analyze, found.excluded)
}

/// [`analyze_tree`] limited to the `selected` paths, given relative to `root`.
/// A selected path still has to pass the language and exclude filters.
pub fn analyze_paths(
    root: &Path,
    languages: &[Language],
    excludes: &[String],
    selected: &[std::path::PathBuf],
) -> Result<Analysis> {
    validate_root(root)?;
    let exclude_set = build_globs(excludes)?;
    let enabled: HashSet<_> = languages.iter().copied().collect();
    let selected: HashSet<_> = selected.iter().cloned().collect();
    let found = discover_paths(root, &enabled, &exclude_set);
    let paths = found
        .analyze
        .into_iter()
        .filter(|(path, _)| {
            let relative = path.strip_prefix(root).unwrap_or(path);
            selected.contains(relative)
        })
        .collect();
    analyze_discovered(paths, found.excluded)
}

fn analyze_discovered(
    paths: Vec<(std::path::PathBuf, Language)>,
    excluded: Vec<std::path::PathBuf>,
) -> Result<Analysis> {
    let results: Vec<_> = paths
        .par_iter()
        .map(|(path, language)| analyze_file(path, *language))
        .collect();
    let mut analysis = Analysis {
        candidate_files: paths.len(),
        excluded_files: excluded,
        ..Analysis::default()
    };
    for result in results {
        add_file_result(&mut analysis, result);
    }
    sort_analysis(&mut analysis);
    Ok(analysis)
}

fn validate_root(root: &Path) -> Result<()> {
    if !root.exists() {
        return Err(anyhow!("analysis path does not exist: {}", root.display()));
    }
    if !root.is_dir() {
        return Err(anyhow!(
            "analysis path is not a directory: {}",
            root.display()
        ));
    }
    Ok(())
}

/// Every file of an enabled language under `root`, split by the exclude
/// globs, which match paths relative to `root`.
fn discover_paths(root: &Path, enabled: &HashSet<Language>, exclude_set: &GlobSet) -> Discovered {
    let (analyze, excluded): (Vec<_>, Vec<_>) = enabled_files(root, enabled)
        .into_iter()
        .partition(|(path, _)| !exclude_set.is_match(path.strip_prefix(root).unwrap_or(path)));
    Discovered {
        analyze,
        excluded: excluded.into_iter().map(|(path, _)| path).collect(),
    }
}

/// Every file of an enabled language under `root`, `.gitignore` and hidden
/// directories honored.
fn enabled_files(root: &Path, enabled: &HashSet<Language>) -> Vec<(std::path::PathBuf, Language)> {
    ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let path = entry.into_path();
            let language = Language::from_path(&path)?;
            enabled.contains(&language).then_some((path, language))
        })
        .collect()
}

fn add_file_result(analysis: &mut Analysis, result: Result<FileAnalysis>) {
    match result {
        Ok(file) => add_file_analysis(analysis, file),
        Err(error) => analysis.diagnostics.push(Diagnostic {
            kind: "parse".into(),
            message: error.to_string(),
            file: None,
        }),
    }
}

fn add_file_analysis(analysis: &mut Analysis, file: FileAnalysis) {
    if let Some(diagnostic) = file.diagnostic {
        analysis.diagnostics.push(diagnostic);
        return;
    }
    analysis.parsed_files += 1;
    analysis.units.extend(file.units);
}

fn sort_analysis(analysis: &mut Analysis) {
    analysis.units.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.symbol.cmp(&b.symbol))
    });
    analysis
        .diagnostics
        .sort_by(|a, b| a.file.cmp(&b.file).then(a.message.cmp(&b.message)));
}

fn build_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid exclude pattern: {pattern}"))?,
        );
    }
    builder.build().context("building exclude patterns")
}

type GrammarLoader = fn(&Path) -> tree_sitter::Language;

const GRAMMAR_LOADERS: [GrammarLoader; 10] = [
    javascript_grammar,
    typescript_grammar,
    python_grammar,
    go_grammar,
    rust_grammar,
    java_grammar,
    c_sharp_grammar,
    c_grammar,
    cpp_grammar,
    dart_grammar,
];

fn grammar(language: Language, path: &Path) -> tree_sitter::Language {
    GRAMMAR_LOADERS[language.index()](path)
}

fn javascript_grammar(_: &Path) -> tree_sitter::Language {
    tree_sitter_javascript::LANGUAGE.into()
}

fn typescript_grammar(path: &Path) -> tree_sitter::Language {
    if path.extension().is_some_and(|extension| extension == "tsx") {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    }
}

fn python_grammar(_: &Path) -> tree_sitter::Language {
    tree_sitter_python::LANGUAGE.into()
}

fn go_grammar(_: &Path) -> tree_sitter::Language {
    tree_sitter_go::LANGUAGE.into()
}

fn rust_grammar(_: &Path) -> tree_sitter::Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn java_grammar(_: &Path) -> tree_sitter::Language {
    tree_sitter_java::LANGUAGE.into()
}

fn c_sharp_grammar(_: &Path) -> tree_sitter::Language {
    tree_sitter_c_sharp::LANGUAGE.into()
}

fn cpp_grammar(_: &Path) -> tree_sitter::Language {
    tree_sitter_cpp::LANGUAGE.into()
}

fn c_grammar(_: &Path) -> tree_sitter::Language {
    tree_sitter_c::LANGUAGE.into()
}

fn dart_grammar(_: &Path) -> tree_sitter::Language {
    tree_sitter_dart::LANGUAGE.into()
}

fn analyze_file(path: &Path, language: Language) -> Result<FileAnalysis> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("reading source file {}", path.display()))?;
    let mut parser = Parser::new();
    parser
        .set_language(&grammar(language, path))
        .with_context(|| format!("loading {language} grammar"))?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow!("parser returned no tree for {}", path.display()))?;
    if tree.root_node().has_error() {
        return Ok(FileAnalysis {
            units: Vec::new(),
            diagnostic: Some(Diagnostic {
                kind: "parse".into(),
                message: format!(
                    "skipped {} because its syntax tree has errors",
                    path.display()
                ),
                file: Some(path.to_path_buf()),
            }),
        });
    }

    let bytes = source.as_bytes();
    let mut specs = Vec::new();
    collect_units(tree.root_node(), language, bytes, &mut specs);
    let units = specs
        .into_iter()
        .map(|spec| CodeUnit {
            language,
            file: path.to_path_buf(),
            symbol: qualify_symbol(spec.node, language, &spec.name, bytes),
            start_line: spec.node.start_position().row + 1,
            end_line: unit_end_line(spec.node, language),
            complexity: complexity(spec.node, bytes) as f64,
        })
        .collect();
    Ok(FileAnalysis {
        units,
        diagnostic: None,
    })
}

fn unit_end_line(node: Node<'_>, language: Language) -> usize {
    let end = if language == Language::Dart && is_dart_callable(node.kind()) {
        dart_body(node).unwrap_or(node)
    } else {
        node
    };
    end.end_position().row + 1
}

fn collect_units<'tree>(
    node: Node<'tree>,
    language: Language,
    source: &[u8],
    output: &mut Vec<UnitSpec<'tree>>,
) {
    if let Some((unit_node, name)) = named_unit(node, language, source) {
        output.push(UnitSpec {
            node: unit_node,
            name,
        });
    }

    let mut cursor = node.walk();
    let mut attributes = TestAttributes::new(language);
    for child in node.named_children(&mut cursor) {
        if attributes.skips(child, source) {
            continue;
        }
        collect_units(child, language, source, output);
    }
}

/// Tracks Rust test attributes while walking a list of sibling nodes.
///
/// Rust puts an attribute in its own node ahead of the item it annotates, so a
/// `#[cfg(test)] mod tests` block and its `#[test]` functions look like plain
/// units. Scoring them measures the test suite against itself, which says
/// nothing about production risk, and it buries the real entries. Other
/// languages keep their tests in separate files, which the default excludes
/// already drop.
struct TestAttributes {
    rust: bool,
    pending: bool,
}

impl TestAttributes {
    fn new(language: Language) -> Self {
        Self {
            rust: language == Language::Rust,
            pending: false,
        }
    }

    /// Report whether this sibling should be skipped along with its subtree.
    ///
    /// Attributes are always skipped themselves; they hold no units. A test
    /// attribute stays pending across any attributes that follow it, so
    /// `#[cfg(test)]` still suppresses the item under `#[allow(..)]`.
    fn skips(&mut self, node: Node<'_>, source: &[u8]) -> bool {
        if !self.rust {
            return false;
        }
        if node.kind() == "attribute_item" {
            self.pending |= is_test_attribute(node, source);
            return true;
        }
        std::mem::take(&mut self.pending)
    }
}

/// Match `#[cfg(test)]` and `#[test]`-style attributes, including `#[tokio::test]`.
///
/// Compound forms such as `cfg(all(test, feature = "x"))` are not matched; they
/// are rare, and treating them as production code errs toward reporting more.
fn is_test_attribute(node: Node<'_>, source: &[u8]) -> bool {
    let Some(raw) = text(node, source) else {
        return false;
    };
    let stripped: String = raw.chars().filter(|value| !value.is_whitespace()).collect();
    let inner = stripped.trim_start_matches("#[").trim_end_matches(']');
    inner == "cfg(test)" || is_test_marker(inner)
}

fn is_test_marker(inner: &str) -> bool {
    inner == "test" || inner.ends_with("::test")
}

const DECLARATION_KINDS: [&[&str]; 10] = [
    &[
        "function_declaration",
        "generator_function_declaration",
        "method_definition",
    ],
    &[
        "function_declaration",
        "generator_function_declaration",
        "method_definition",
    ],
    &["function_definition"],
    &["function_declaration", "method_declaration"],
    &["function_item"],
    &["method_declaration", "constructor_declaration"],
    &[
        "method_declaration",
        "constructor_declaration",
        "destructor_declaration",
        "property_declaration",
        "operator_declaration",
        "conversion_operator_declaration",
        "accessor_declaration",
        "local_function_statement",
    ],
    &["function_definition"],
    &[
        "function_definition",
        "method_declaration",
        "constructor",
        "destructor",
    ],
    &[
        "function_signature",
        "method_signature",
        "getter_signature",
        "setter_signature",
        "constructor_signature",
        "factory_constructor_signature",
    ],
];

fn named_unit<'tree>(
    node: Node<'tree>,
    language: Language,
    source: &[u8],
) -> Option<(Node<'tree>, String)> {
    if DECLARATION_KINDS[language.index()].contains(&node.kind()) {
        return declared_unit(node, language, source);
    }
    assigned_callable(node, language, source)
}

fn declared_unit<'tree>(
    node: Node<'tree>,
    language: Language,
    source: &[u8],
) -> Option<(Node<'tree>, String)> {
    if is_bodyless_declaration(node, language) {
        return None;
    }
    if language == Language::CSharp && is_non_callable_csharp(node) {
        return None;
    }

    let name = declared_name(node, source)?;
    Some((node, java_signature(node, language, name, source)))
}

fn is_accessor_property(node: Node<'_>) -> bool {
    node.kind() == "property_declaration" && node.child_by_field_name("accessors").is_some()
}

fn is_non_callable_csharp(node: Node<'_>) -> bool {
    is_accessor_property(node)
        || (node.kind() == "accessor_declaration" && node.child_by_field_name("body").is_none())
}

fn is_bodyless_java_declaration(node: Node<'_>, language: Language) -> bool {
    language == Language::Java && node.child_by_field_name("body").is_none()
}

fn is_bodyless_declaration(node: Node<'_>, language: Language) -> bool {
    is_bodyless_java_declaration(node, language)
        || (language == Language::CSharp
            && matches!(
                node.kind(),
                "method_declaration" | "local_function_statement"
            )
            && node.child_by_field_name("body").is_none())
}

fn declared_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if matches!(
        node.kind(),
        "constructor_signature" | "factory_constructor_signature"
    ) {
        return dart_constructor_name(node, source);
    }
    // For C# conversion operators (implicit/explicit operator), there is no 'name' field;
    // the method name is always "operator". Return that so the caller can build "ClassName.operator".
    if node.kind() == "conversion_operator_declaration" {
        return Some("operator".to_string());
    }
    if node.kind() == "operator_declaration" {
        return operator_name(node, source);
    }

    node.child_by_field_name("name")
        .or_else(|| node.child_by_field_name("identifier"))
        .or_else(|| node.child_by_field_name("name_identifier"))
        .or_else(|| {
            node.child_by_field_name("declarator")
                .and_then(declarator_name)
        })
        .or_else(|| {
            // Direct: function_declarator or similar with unnamed identifier child
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "identifier" || child.kind() == "field_identifier")
        })
        .and_then(|name| text(name, source))
        .map(str::to_string)
}

fn dart_constructor_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let names: Vec<_> = node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "identifier")
        .filter_map(|child| text(child, source))
        .collect();
    (!names.is_empty()).then(|| names.join("."))
}

fn operator_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let keyword = node
        .children(&mut cursor)
        .find(|child| child.kind() == "operator")?;
    let token = text(keyword.next_sibling()?, source)?;
    Some(format!("operator {token}"))
}

fn declarator_name(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(inner) = node.child_by_field_name("declarator") {
        return declarator_name(inner);
    }
    if node.kind() == "parenthesized_declarator" {
        return declarator_name(node.named_child(0)?);
    }
    Some(node)
}

/// Get the first named child that looks like a name or identifier.
struct AssignmentSpec {
    language: Language,
    node_kind: &'static str,
    name_field: &'static str,
    value_fields: &'static [&'static str],
    callable_kinds: &'static [&'static str],
}

const JS_CALLABLES: &[&str] = &[
    "arrow_function",
    "function_expression",
    "generator_function",
];
const CSHARP_CALLABLES: &[&str] = &[
    "simple_lambda_expression",
    "parenthesized_lambda_expression",
    "lambda_expression",
    "anonymous_method_expression",
];
const ASSIGNMENT_SPECS: &[AssignmentSpec] = &[
    AssignmentSpec {
        language: Language::JavaScript,
        node_kind: "variable_declarator",
        name_field: "name",
        value_fields: &["value"],
        callable_kinds: JS_CALLABLES,
    },
    AssignmentSpec {
        language: Language::JavaScript,
        node_kind: "public_field_definition",
        name_field: "name",
        value_fields: &["value"],
        callable_kinds: JS_CALLABLES,
    },
    AssignmentSpec {
        language: Language::JavaScript,
        node_kind: "pair",
        name_field: "key",
        value_fields: &["value"],
        callable_kinds: JS_CALLABLES,
    },
    AssignmentSpec {
        language: Language::JavaScript,
        node_kind: "assignment_expression",
        name_field: "left",
        value_fields: &["right"],
        callable_kinds: JS_CALLABLES,
    },
    AssignmentSpec {
        language: Language::TypeScript,
        node_kind: "variable_declarator",
        name_field: "name",
        value_fields: &["value"],
        callable_kinds: JS_CALLABLES,
    },
    AssignmentSpec {
        language: Language::TypeScript,
        node_kind: "public_field_definition",
        name_field: "name",
        value_fields: &["value"],
        callable_kinds: JS_CALLABLES,
    },
    AssignmentSpec {
        language: Language::TypeScript,
        node_kind: "pair",
        name_field: "key",
        value_fields: &["value"],
        callable_kinds: JS_CALLABLES,
    },
    AssignmentSpec {
        language: Language::TypeScript,
        node_kind: "assignment_expression",
        name_field: "left",
        value_fields: &["right"],
        callable_kinds: JS_CALLABLES,
    },
    AssignmentSpec {
        language: Language::Python,
        node_kind: "assignment",
        name_field: "left",
        value_fields: &["right"],
        callable_kinds: &["lambda"],
    },
    AssignmentSpec {
        language: Language::Go,
        node_kind: "short_var_declaration",
        name_field: "left",
        value_fields: &["right"],
        callable_kinds: &["func_literal"],
    },
    AssignmentSpec {
        language: Language::Go,
        node_kind: "var_spec",
        name_field: "name",
        value_fields: &["value"],
        callable_kinds: &["func_literal"],
    },
    AssignmentSpec {
        language: Language::Rust,
        node_kind: "let_declaration",
        name_field: "pattern",
        value_fields: &["value"],
        callable_kinds: &["closure_expression"],
    },
    AssignmentSpec {
        language: Language::Java,
        node_kind: "variable_declarator",
        name_field: "name",
        value_fields: &["value"],
        callable_kinds: &["lambda_expression"],
    },
    AssignmentSpec {
        language: Language::CSharp,
        node_kind: "assignment_expression",
        name_field: "left",
        value_fields: &["right"],
        callable_kinds: CSHARP_CALLABLES,
    },
    AssignmentSpec {
        language: Language::CSharp,
        node_kind: "variable_declarator",
        name_field: "name",
        value_fields: &["value", "initializer"],
        callable_kinds: CSHARP_CALLABLES,
    },
    AssignmentSpec {
        language: Language::Cpp,
        node_kind: "init_declarator",
        name_field: "declarator",
        value_fields: &["value"],
        callable_kinds: &["lambda_expression"],
    },
    AssignmentSpec {
        language: Language::Cpp,
        node_kind: "assignment_expression",
        name_field: "left",
        value_fields: &["right"],
        callable_kinds: &["lambda_expression"],
    },
    AssignmentSpec {
        language: Language::Dart,
        node_kind: "initialized_variable_definition",
        name_field: "name",
        value_fields: &["value"],
        callable_kinds: &["function_expression"],
    },
    AssignmentSpec {
        language: Language::Dart,
        node_kind: "static_final_declaration",
        name_field: "name",
        value_fields: &["value"],
        callable_kinds: &["function_expression"],
    },
    AssignmentSpec {
        language: Language::Dart,
        node_kind: "initialized_identifier",
        name_field: "name",
        value_fields: &["value"],
        callable_kinds: &["function_expression"],
    },
    AssignmentSpec {
        language: Language::Dart,
        node_kind: "assignment_expression",
        name_field: "left",
        value_fields: &["right"],
        callable_kinds: &["function_expression"],
    },
];

fn assigned_callable<'tree>(
    node: Node<'tree>,
    language: Language,
    source: &[u8],
) -> Option<(Node<'tree>, String)> {
    let spec = ASSIGNMENT_SPECS
        .iter()
        .find(|spec| spec.language == language && spec.node_kind == node.kind())?;
    // Try the primary name field; fall back to first named child with kind "identifier"
    let name_node = node
        .child_by_field_name(spec.name_field)
        .or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "identifier")
        })
        .map(unwrap_singleton)?;
    // Try each value field in order; fall back to named children for callable kinds
    let value = spec
        .value_fields
        .iter()
        .filter_map(|f| node.child_by_field_name(*f))
        .next()
        .or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| spec.callable_kinds.contains(&child.kind()))
        })
        .map(unwrap_singleton)?;
    spec.callable_kinds.contains(&value.kind()).then(|| {
        (
            value,
            normalize(text(name_node, source).unwrap_or("anonymous")),
        )
    })
}

fn unwrap_singleton(mut node: Node<'_>) -> Node<'_> {
    while matches!(node.kind(), "expression_list" | "pattern_list") && node.named_child_count() == 1
    {
        node = node.named_child(0).expect("singleton node has one child");
    }
    node
}

fn java_signature(node: Node<'_>, language: Language, name: String, source: &[u8]) -> String {
    if language != Language::Java {
        return name;
    }
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return name;
    };
    format!(
        "{name}({})",
        java_parameter_types(parameters, source).join(",")
    )
}

fn java_parameter_types(parameters: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut cursor = parameters.walk();
    parameters
        .named_children(&mut cursor)
        .filter_map(|parameter| parameter.child_by_field_name("type"))
        .map(|kind| normalize(text(kind, source).unwrap_or_default()))
        .collect()
}

fn qualify_symbol(node: Node<'_>, language: Language, name: &str, source: &[u8]) -> String {
    let mut parts = csharp_file_scoped_namespace(node, language, source)
        .into_iter()
        .collect::<Vec<_>>();
    parts.extend(container_parts(node, language, source));
    if let Some(receiver) = go_receiver_name(node, language, source) {
        parts.push(receiver);
    }
    parts.push(name.to_string());
    parts.join(SYMBOL_SEPARATORS[language.index()])
}

fn csharp_file_scoped_namespace(
    node: Node<'_>,
    language: Language,
    source: &[u8],
) -> Option<String> {
    if language != Language::CSharp {
        return None;
    }
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|child| child.kind() == "file_scoped_namespace_declaration")
        .and_then(|namespace| namespace.child_by_field_name("name"))
        .and_then(|name| text(name, source))
        .map(str::to_string)
}

const SYMBOL_SEPARATORS: [&str; 10] = [".", ".", ".", ".", "::", ".", ".", "::", "::", "."];

fn container_parts(node: Node<'_>, language: Language, source: &[u8]) -> Vec<String> {
    let mut parts = Vec::new();
    let mut parent = node.parent();
    while let Some(ancestor) = parent {
        if is_container(ancestor.kind()) {
            if let Some(value) = container_name(ancestor, language, source) {
                parts.push(value);
            }
        }
        parent = ancestor.parent();
    }
    parts.reverse();
    parts
}

fn is_container(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "class_specifier"
            | "struct_specifier"
            | "union_specifier"
            | "abstract_class_declaration"
            | "class"
            | "class_definition"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "struct_declaration"
            | "trait_item"
            | "impl_item"
            | "mod_item"
            | "mod_declaration"
            | "namespace_declaration"
            | "namespace_definition"
            | "function_declaration"
            | "function_definition"
            | "function_item"
            | "method_definition"
            | "method_declaration"
            | "constructor_declaration"
            | "accessor_declaration"
            | "property_declaration"
            | "file_scoped_namespace_declaration"
    )
}

fn go_receiver_name(node: Node<'_>, language: Language, source: &[u8]) -> Option<String> {
    if language != Language::Go {
        return None;
    }
    if node.kind() != "method_declaration" {
        return None;
    }
    let receiver = node.child_by_field_name("receiver")?;
    let parameter = receiver.named_child(0).unwrap_or(receiver);
    let receiver_type = parameter.child_by_field_name("type")?;
    Some(normalize(text(receiver_type, source).unwrap_or_default()))
}

fn container_name(node: Node<'_>, language: Language, source: &[u8]) -> Option<String> {
    if node.kind() == "impl_item" {
        return impl_name(node, source);
    }
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| {
            node.child_by_field_name("declarator")
                .and_then(declarator_name)
        })
        .or_else(|| assigned_class_name(node, language))
        .or_else(|| {
            // C/C++ class_specifier stores the class name as a type_identifier
            // child rather than a "name" field when the class is unqualified.
            if language == Language::Cpp && node.kind() == "class_specifier" {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find(|c| c.kind() == "type_identifier")
            } else {
                None
            }
        })
        .or_else(|| {
            // namespace_definition stores the namespace name as a namespace_identifier child.
            if node.kind() == "namespace_definition" {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find(|c| c.kind() == "namespace_identifier")
            } else {
                None
            }
        });
    let value = text(name_node?, source)?.to_string();
    if is_java_callable(node, language) {
        return Some(java_signature(node, language, value, source));
    }
    (!value.is_empty()).then_some(value)
}

/// The variable a class expression is assigned to, so the methods of
/// `const Widget = class { ... }` read as `Widget.method` rather than bare.
fn assigned_class_name<'tree>(node: Node<'tree>, language: Language) -> Option<Node<'tree>> {
    if node.kind() != "class" {
        return None;
    }
    let parent = node.parent()?;
    let spec = ASSIGNMENT_SPECS
        .iter()
        .find(|spec| spec.language == language && spec.node_kind == parent.kind())?;
    parent.child_by_field_name(spec.name_field)
}

fn impl_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let header = text(node, source)?.split('{').next()?.trim();
    header
        .strip_prefix("impl")
        .map(str::trim)
        .and_then(|value| value.split_whitespace().last())
        .map(str::to_string)
}

fn is_java_callable(node: Node<'_>, language: Language) -> bool {
    language == Language::Java
        && matches!(
            node.kind(),
            "method_declaration" | "constructor_declaration"
        )
}

fn complexity(root: Node<'_>, source: &[u8]) -> usize {
    let mut count = 1;
    count_decisions(root, root, source, &mut count);
    count
}

fn count_decisions(node: Node<'_>, root: Node<'_>, source: &[u8], count: &mut usize) {
    if is_dart_callable(node.kind()) {
        return count_dart_body(node, root, source, count);
    }
    walk_and_count(node, root, source, count);
}

/// Walk a node and all its descendants, counting decision points.
fn walk_and_count(node: Node<'_>, root: Node<'_>, source: &[u8], count: &mut usize) {
    if node != root && is_callable(node.kind()) {
        return;
    }
    if is_decision(node, source) {
        *count += 1;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_and_count(child, root, source, count);
    }
}

/// For a Dart callable node, walk its `function_body` instead of descending
/// into child nodes (tree-sitter-dart places the body inside a separate node).
fn count_dart_body(callable: Node<'_>, root: Node<'_>, source: &[u8], count: &mut usize) {
    if callable != root {
        return;
    }
    if let Some(body) = dart_body(callable) {
        walk_and_count(body, body, source, count);
    }
}

/// Dart local declarations have an unfielded body inside their own wrapper.
fn dart_body(callable: Node<'_>) -> Option<Node<'_>> {
    let owner = callable.parent()?;
    if owner.kind() == "method_signature" {
        return dart_body(owner);
    }
    owner
        .child_by_field_name("body")
        .or_else(|| {
            let mut cursor = owner.walk();
            owner
                .named_children(&mut cursor)
                .find(|child| child.kind() == "function_body")
        })
        .filter(|body| body.kind() == "function_body")
}

fn is_dart_callable(kind: &str) -> bool {
    matches!(
        kind,
        "function_signature"
            | "method_signature"
            | "getter_signature"
            | "setter_signature"
            | "constructor_signature"
            | "factory_constructor_signature"
    )
}

fn is_callable(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
            | "function_definition"
            | "lambda"
            | "method_declaration"
            | "func_literal"
            | "function_item"
            | "closure_expression"
            | "constructor_declaration"
            | "lambda_expression"
            | "destructor_declaration"
            // C/C++ constructors/destructors (no `_declaration` suffix)
            | "constructor"
            | "destructor"
            | "local_function_statement"
            | "local_function_declaration"
            | "parenthesized_lambda_expression"
            | "simple_lambda_expression"
            | "anonymous_method_expression"
            | "operator_declaration"
            | "conversion_operator_declaration"
            | "function_signature"
            | "method_signature"
            | "getter_signature"
            | "setter_signature"
            | "constructor_signature"
            | "factory_constructor_signature"
    )
}

fn is_decision(node: Node<'_>, source: &[u8]) -> bool {
    if is_simple_decision(node) {
        return true;
    }
    if is_arm_decision(node.kind()) {
        return is_non_default_arm(node, source);
    }
    is_boolean_decision(node, source)
}

fn is_simple_decision(node: Node<'_>) -> bool {
    is_let_else(node)
        || matches!(
            node.kind(),
            "if_statement"
                | "elif_clause"
                | "if_expression"
                | "for_statement"
                | "foreach_statement"
                | "for_in_statement"
                | "for_range_loop"
                | "while_statement"
                | "while_expression"
                | "do_statement"
                | "enhanced_for_statement"
                | "loop_expression"
                | "for_expression"
                | "catch_clause"
                | "except_clause"
                | "ternary_expression"
                | "conditional_expression"
                | "for_in_clause"
                | "if_clause"
                | "goto_statement"
                // Dart uses flat node kinds for control flow.
                | "if"
                | "for"
                | "while"
                | "do"
                | "switch"
        )
}

/// Rust's `let … else` diverges on its else branch, so it is a decision like
/// `if let`, even though the grammar files it under `let_declaration`.
fn is_let_else(node: Node<'_>) -> bool {
    node.kind() == "let_declaration" && node.child_by_field_name("alternative").is_some()
}

fn is_arm_decision(kind: &str) -> bool {
    matches!(
        kind,
        "switch_case"
            | "case_statement"
            | "expression_case"
            | "type_case"
            | "communication_case"
            | "case_clause"
                | "switch_expression_arm"
            | "match_arm"
            | "when_clause"
            // Java: one per label in both `case x:` and `case x ->` forms.
            // Counting the enclosing `switch_rule` instead would miss the
            // colon form and double-count the arrow form.
            | "switch_label"
    )
}

fn is_non_default_arm(node: Node<'_>, source: &[u8]) -> bool {
    let value = text(node, source).unwrap_or_default().trim_start();
    if node.kind() == "switch_expression_arm" {
        return !is_csharp_default_arm(value);
    }
    !value.starts_with("default") && !value.starts_with("_ =>") && !is_wildcard_case(value)
}

fn is_csharp_default_arm(value: &str) -> bool {
    value
        .strip_prefix('_')
        .is_some_and(|rest| rest.trim_start().starts_with("=>"))
}

/// Python's `case _:` is the default arm of a `match` and adds nothing, like
/// `default:` and `_ =>`. A guarded `case _ if x:` still branches.
fn is_wildcard_case(value: &str) -> bool {
    value
        .strip_prefix("case")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix('_'))
        .is_some_and(|after| after.trim_start().starts_with(':'))
}

/// `let_chain` is Rust's `if let … && …`: each node holds one `&&`, so it
/// counts like the `&&` of a plain condition.
fn is_boolean_decision(node: Node<'_>, source: &[u8]) -> bool {
    matches!(
        node.kind(),
        "binary_expression" | "boolean_operator" | "let_chain"
            // Dart: `logical_and_expression` / `logical_or_expression` embed the
            // operator in text rather than as a named child.
            | "logical_and_expression"
            | "logical_or_expression"
    ) && has_boolean_operator(node, source)
}

fn has_boolean_operator(node: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| {
        matches!(child.kind(), "&&" | "||" | "and" | "or")
            || text(child, source).is_some_and(|value| matches!(value, "&&" | "||" | "and" | "or"))
    })
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn analyze(extension: &str, source: &str) -> Vec<CodeUnit> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("sample.{extension}"));
        fs::write(&path, source).unwrap();
        let language = Language::from_path(&path).unwrap();
        analyze_file(&path, language).unwrap().units
    }

    #[test]
    fn javascript_named_arrow_and_function() {
        let units = analyze(
            "js",
            "function f(a) { if (a && a.ok) return 1; return 0; }\nconst g = (x) => x ? 1 : 0;",
        );
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].symbol, "f");
        assert_eq!(units[0].complexity, 3.0);
        assert_eq!(units[1].symbol, "g");
        assert_eq!(units[1].complexity, 2.0);
    }

    #[test]
    fn typescript_tsx_and_async_functions_parse() {
        let units = analyze(
            "tsx",
            "async function load<T>(x: T) { return x ? <div /> : null; }\nconst View = (p: {ok: boolean}) => p.ok && <span />;",
        );
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].symbol, "load");
        assert_eq!(units[0].complexity, 2.0);
        assert_eq!(units[1].complexity, 2.0);
    }

    #[test]
    fn assigned_callable_values_get_names() {
        let python = analyze("py", "handler = lambda x: 1 if x else 0\n");
        assert_eq!(python[0].symbol, "handler");
        assert_eq!(python[0].complexity, 2.0);

        let rust = analyze(
            "rs",
            "fn main() { let handler = |x| if x { 1 } else { 0 }; }",
        );
        assert!(rust.iter().any(|unit| unit.symbol == "main::handler"));

        let java = analyze(
            "java",
            "class A { void run() { Runnable handler = () -> { if (true) {} }; } }",
        );
        assert!(java.iter().any(|unit| unit.symbol == "A.run().handler"));

        let go = analyze(
            "go",
            "package p\nfunc main() { handler := func(x bool) { if x {} }; handler(true) }",
        );
        assert!(go.iter().any(|unit| unit.symbol == "main.handler"));
    }

    #[test]
    fn python_nested_units_do_not_inflate_parent() {
        let units = analyze(
            "py",
            "async def outer(x):\n    def inner(y):\n        return 1 if y else 0\n    if x:\n        return inner(x)\n",
        );
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].complexity, 2.0);
        assert_eq!(units[1].symbol, "outer.inner");
        assert_eq!(units[1].complexity, 2.0);
    }

    #[test]
    fn rust_impl_is_qualified() {
        let units = analyze(
            "rs",
            "impl Thing { fn run(&self, x: bool) { if x { loop {} } } }",
        );
        assert_eq!(units[0].symbol, "Thing::run");
        assert_eq!(units[0].complexity, 3.0);
    }

    #[test]
    fn c_accepts_c_identifier() {
        let units = analyze("c", "int new(int x) { if (x) return 1; return 0; }");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].language.as_str(), "c");
        assert_eq!(units[0].symbol, "new");
        assert_eq!(units[0].complexity, 2.0);
    }

    #[test]
    fn rust_match_ignores_default_arm() {
        let units = analyze(
            "rs",
            "fn choose(x: u8) -> u8 { match x { 0 => 1, _ => 2 } }",
        );
        assert_eq!(units[0].complexity, 2.0);
    }

    #[test]
    fn rust_test_code_is_not_scored() {
        let units = analyze(
            "rs",
            concat!(
                "fn keep(x: bool) { if x { loop {} } }\n",
                "#[cfg(test)]\n",
                "mod tests {\n",
                "    fn helper(x: bool) { if x { loop {} } }\n",
                "    #[test]\n",
                "    fn case() {}\n",
                "}\n",
                "#[cfg(test)]\n",
                "fn gated() {}\n",
                "#[tokio::test]\n",
                "async fn async_case() {}\n",
                "#[allow(dead_code)]\n",
                "fn attributed(x: bool) { if x { loop {} } }\n",
            ),
        );
        let symbols: Vec<_> = units.iter().map(|unit| unit.symbol.as_str()).collect();
        // A non-test attribute must not suppress the item it annotates.
        assert_eq!(symbols, ["keep", "attributed"]);
    }

    #[test]
    fn go_receiver_is_qualified() {
        let units = analyze(
            "go",
            "package p\ntype T struct{}\nfunc (t T) Run(x bool) { if x { return } }",
        );
        assert!(units[0].symbol.ends_with(".Run"));
        assert_eq!(units[0].complexity, 2.0);
    }

    #[test]
    fn java_overload_has_parameter_types() {
        let units = analyze(
            "java",
            "class A { A() {} int f(String x) { return x == null ? 0 : 1; } }",
        );
        assert_eq!(units.len(), 2);
        assert!(units.iter().any(|unit| unit.symbol == "A.f(String)"));
    }

    #[test]
    fn java_switch_forms_score_the_same() {
        let colon = analyze(
            "java",
            "class A { int f(int x) { switch (x) { case 1: return 1; case 2: return 2; case 3: return 3; default: return 0; } } }",
        );
        let arrow = analyze(
            "java",
            "class A { int f(int x) { return switch (x) { case 1 -> 1; case 2 -> 2; case 3 -> 3; default -> 0; }; } }",
        );
        assert_eq!(
            colon[0].complexity, 4.0,
            "colon-form switch was not counted"
        );
        assert_eq!(
            arrow[0].complexity, 4.0,
            "arrow-form switch was double counted"
        );
    }

    #[test]
    fn cpp_codeunits_produce_functions_methods_constructors_and_destructors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.cpp");
        fs::write(
            &path,
            concat!(
                "void free_fn(int *p) { if (p) {} }\n",
                "class Widget {\n",
                "    Widget() {}\n",
                "    ~Widget() {}\n",
                "    void run(bool x) { if (x) {} }\n",
                "};\n",
            ),
        )
        .unwrap();
        let language = Language::from_path(&path).unwrap();
        let result = analyze_file(&path, language).unwrap();
        let symbols: Vec<_> = result.units.iter().map(|u| u.symbol.as_str()).collect();
        // tree-sitter-cpp 0.23 produces class_specifier for `class Widget { ... }`.
        // Inside the class body, methods are `function_definition` nodes.
        // The constructor is also detected as `function_definition`.
        assert!(
            symbols.contains(&"free_fn"),
            "free_fn not found in {symbols:?}"
        );
        // C++ method detection requires class body traversal.
        assert!(
            symbols.contains(&"Widget::run") || symbols.contains(&"run"),
            "Widget::run not found in {symbols:?}"
        );
        // Constructor may appear as Widget::Widget.
        let has_widget = symbols.iter().any(|s| s.contains("Widget"));
        assert!(has_widget, "Widget-related unit not found in {symbols:?}");
    }

    #[test]
    fn cpp_debug_class_structure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.cpp");
        fs::write(
            &path,
            // Same as existing integration test — global function.
            "void run(bool x) { if (x) {} }\n",
        )
        .unwrap();
        let language = Language::from_path(&path).unwrap();
        let result = analyze_file(&path, language).unwrap();
        let symbols: Vec<_> = result.units.iter().map(|u| u.symbol.as_str()).collect();
        assert!(
            !result.units.is_empty(),
            "expected >=1 unit, got {}: {:?}",
            result.units.len(),
            symbols
        );
        assert!(symbols.contains(&"run"), "run not found in {symbols:?}");
    }

    #[test]
    fn cpp_class_in_namespace_produces_codeunits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.cpp");
        fs::write(
            &path,
            "namespace ns { class A { void foo(bool x) { if (x) {} } }; }\n",
        )
        .unwrap();
        let language = Language::from_path(&path).unwrap();

        // Verify language is Cpp.
        assert!(
            language == Language::Cpp,
            "expected Language::Cpp, got {language:?}",
        );
        eprintln!("language.index() = {}", language.index());
        eprintln!(
            "DECLARATION_KINDS[7] = {:?}",
            DECLARATION_KINDS[language.index()]
        );

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&grammar(language, &path)).unwrap();
        let source = std::fs::read_to_string(&path).unwrap();
        let tree = parser.parse(&source, None).unwrap();
        let root = tree.root_node();
        assert!(!root.has_error(), "parse error");
        let bytes = source.as_bytes();

        // Check if function_definition node kind matches.
        let mut found_node = None;
        fn find_fn_def(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
            if node.kind() == "function_definition" {
                return Some(node);
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if let Some(found) = find_fn_def(child) {
                    return Some(found);
                }
            }
            None
        }
        if let Some(fn_node) = find_fn_def(root) {
            found_node = Some((fn_node.kind().to_string(), fn_node.start_position().row));
            // Directly test named_unit on this node.
            let nu_result = named_unit(fn_node, language, bytes);
            if let Some((n, name)) = &nu_result {
                eprintln!("  returned name='{}' kind='{}'", name, n.kind());
            }
            // Test container_parts for qualification.
            let parts = container_parts(fn_node, language, bytes);
            eprintln!("  container_parts = {parts:?}");
        }
        assert!(
            found_node.is_some(),
            "function_definition not found in tree"
        );

        let mut specs = Vec::new();
        collect_units(root, language, bytes, &mut specs);
        let names: Vec<_> = specs.iter().map(|s| &s.name).collect();
        assert!(
            !specs.is_empty(),
            "collect_units found 0 units; names: {names:?}",
        );
        let result = analyze_file(&path, language).unwrap();
        let symbols: Vec<_> = result.units.iter().map(|u| u.symbol.as_str()).collect();
        assert!(
            !result.units.is_empty(),
            "expected >=1 unit, got {}: {:?}",
            result.units.len(),
            symbols
        );
        assert!(
            symbols.contains(&"ns::A::foo"),
            "ns::A::foo not found in {symbols:?}"
        );
    }

    #[test]
    fn cpp_goto_counts_as_decision() {
        let units = analyze(
            "c",
            "void process(int *p) { if (!p) goto error; cleanup: error: return; }\n",
        );
        assert_eq!(units[0].complexity, 3.0);
    }

    #[test]
    fn c_and_cpp_count_runtime_decisions_but_not_preprocessor_branches() {
        let source = concat!(
            "void process_data(int *data) {\n",
            "#if defined(DEBUG)\n",
            "  int value = 0;\n",
            "#elif defined(TRACE)\n",
            "  int value = 1;\n",
            "#else\n",
            "  int value = 2;\n",
            "#endif\n",
            "  if (data && data[0] || data[1]) goto cleanup;\n",
            "cleanup: return;\n",
            "}\n",
        );
        for extension in ["c", "cpp"] {
            let units = analyze(extension, source);
            assert_eq!(units.len(), 1);
            assert_eq!(units[0].symbol, "process_data");
            assert_eq!(units[0].complexity, 5.0);
            assert_eq!(units[0].language.as_str(), extension);
        }
    }

    #[test]
    fn vc3_cpp_lambdas_include_enclosing_function_names() {
        let units = analyze(
            "cpp",
            concat!(
                "namespace ns {\n",
                "void first() { auto handler = [](bool x) { if (x) {} }; }\n",
                "void second() { auto handler = [](bool x) { if (x) { while (x) {} } }; }\n",
                "}\n",
            ),
        );
        let actual: Vec<_> = units
            .iter()
            .map(|unit| (unit.symbol.as_str(), unit.complexity))
            .collect();
        assert_eq!(
            actual,
            [
                ("ns::first", 1.0),
                ("ns::first::handler", 2.0),
                ("ns::second", 1.0),
                ("ns::second::handler", 3.0),
            ]
        );
    }

    #[test]
    fn vc3_cpp_range_loop_counts_as_one_decision() {
        let units = analyze(
            "cpp",
            "int sum(int (&values)[3]) { int total = 0; for (int value : values) { total += value; } return total; }",
        );
        assert_eq!(symbols(&units), ["sum"]);
        assert_eq!(units[0].complexity, 2.0);
    }

    #[test]
    fn vc3_c_and_cpp_switch_cases_exclude_default_labels() {
        for language in ["c", "cpp"] {
            for (body, complexity) in [
                (
                    "case 1: return 1; case 2: return 2; default: return 0;",
                    3.0,
                ),
                ("case 1: case 2: return 1; default: return 0;", 3.0),
                ("default: return 0;", 1.0),
            ] {
                let units = analyze(
                    language,
                    &format!("int pick(int x) {{ switch (x) {{ {body} }} }}"),
                );
                assert_eq!(symbols(&units), ["pick"], "{language}");
                assert_eq!(units[0].complexity, complexity, "{language}: {body}");
            }
        }
    }

    #[test]
    fn cpp_preprocessor_is_not_counted_as_decision() {
        let units = analyze(
            "cpp",
            "#if defined(DEBUG)\nvoid dbg() {}\n#endif\nvoid run(bool x) { if (x) {} }\n",
        );
        assert!(units.iter().any(|u| u.symbol == "run"));
        let run = units.iter().find(|u| u.symbol == "run").unwrap();
        assert_eq!(
            run.complexity, 2.0,
            "preprocessor #if was counted as decision"
        );
    }

    #[test]
    fn cpp_template_specialization_is_separate_callable() {
        let units = analyze(
            "cpp",
            "template<typename T> void convert(T x) { if (x) {} }\ntemplate<> void convert<int>(int x) { if (x) {} }\n",
        );
        assert_eq!(symbols(&units), ["convert", "convert<int>"]);
        assert!(units.iter().all(|unit| unit.complexity == 2.0));
    }

    #[test]
    fn cpp_complete_declarator_names_and_type_scopes() {
        let units = analyze(
            "cpp",
            concat!(
                "namespace ns {\n",
                "struct Widget { Widget() {} ~Widget() {} void run() {} };\n",
                "union Value { int x; void run() {} };\n",
                "void Widget::outside() {}\n",
                "bool Widget::operator==(const Widget& other) { return true; }\n",
                "int (*factory())(int) { return nullptr; }\n",
                "}\n",
            ),
        );
        assert_eq!(
            symbols(&units),
            [
                "ns::Widget::Widget",
                "ns::Widget::~Widget",
                "ns::Widget::run",
                "ns::Value::run",
                "ns::Widget::outside",
                "ns::Widget::operator==",
                "ns::factory",
            ]
        );
    }

    #[test]
    fn cpp_initialized_lambda_has_own_unit_and_decisions() {
        let units = analyze(
            "cpp",
            concat!(
                "void outer(bool ready) {\n",
                "auto handler = [](bool x) { if (x) return 1; return 0; };\n",
                "if (ready) handler(ready);\n",
                "}\n",
            ),
        );
        assert_eq!(symbols(&units), ["outer", "outer::handler"]);
        assert_eq!(
            units.iter().map(|unit| unit.complexity).collect::<Vec<_>>(),
            [2.0, 2.0]
        );
    }

    #[test]
    fn cpp_qualified_symbols_include_namespace_and_class() {
        let units = analyze(
            "cpp",
            "namespace ns { class A { void run(bool x) { if (x) {} } }; }",
        );
        assert_eq!(units[0].symbol, "ns::A::run");
        assert_eq!(units[0].complexity, 2.0);
    }

    #[test]
    fn terraform_files_are_not_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.tf");
        fs::write(&path, "resource \"aws_x\" \"main\" { count = 1 }\n").unwrap();
        assert!(Language::from_path(&path).is_none());
    }

    fn symbols(units: &[CodeUnit]) -> Vec<&str> {
        units.iter().map(|unit| unit.symbol.as_str()).collect()
    }

    #[test]
    fn typescript_abstract_and_assigned_classes_qualify_methods() {
        let units = analyze(
            "ts",
            concat!(
                "export abstract class Base { run(x: boolean) { return x ? 1 : 0; } }\n",
                "const Widget = class { go(y: number) { return y > 1; } };\n",
                "const Alias = class Inner { stop() { return 0; } };\n",
            ),
        );
        // A class expression takes the variable's name unless it names itself.
        assert_eq!(symbols(&units), ["Base.run", "Widget.go", "Inner.stop"]);
    }

    #[test]
    fn rust_inline_modules_qualify_functions() {
        let units = analyze(
            "rs",
            "mod alpha { pub fn run() {} }\nmod beta { pub fn run() {} }\nfn top() {}",
        );
        assert_eq!(symbols(&units), ["alpha::run", "beta::run", "top"]);
    }

    #[test]
    fn python_match_ignores_the_wildcard_arm() {
        let units = analyze(
            "py",
            concat!(
                "def pick(x):\n",
                "    match x:\n",
                "        case 1:\n",
                "            return 1\n",
                "        case 2:\n",
                "            return 2\n",
                "        case _ if x > 9:\n",
                "            return 9\n",
                "        case _:\n",
                "            return 0\n",
            ),
        );
        // Two literal arms, the guarded wildcard arm, and its guard count.
        // The bare `case _:` is the default and adds nothing.
        assert_eq!(units[0].complexity, 5.0);
    }

    #[test]
    fn rust_let_else_and_let_chains_count() {
        let units = analyze(
            "rs",
            concat!(
                "fn first(x: Option<u8>) -> u8 { let Some(v) = x else { return 0 }; v }\n",
                "fn second(x: Option<u8>) -> u8 { if let Some(v) = x && v > 1 { v } else { 0 } }\n",
                "fn third(x: Option<u8>) -> u8 { let v = x.unwrap_or(0); v }\n",
            ),
        );
        assert_eq!(units[0].complexity, 2.0, "let-else diverges like if-let");
        assert_eq!(
            units[1].complexity, 3.0,
            "the && of a let chain is a decision"
        );
        assert_eq!(units[2].complexity, 1.0, "a plain let is not");
    }

    #[test]
    fn csharp_methods_and_constructors_produce_codeunits() {
        let units = analyze(
            "cs",
            concat!(
                "namespace MyApp {\n",
                "    class Service {\n",
                "        public void Register() {}\n",
                "        public Service() {}\n",
                "        ~Service() {}\n",
                "    }\n",
                "}\n",
            ),
        );
        assert_eq!(
            units
                .iter()
                .map(|unit| (
                    unit.symbol.as_str(),
                    unit.start_line,
                    unit.end_line,
                    unit.complexity,
                ))
                .collect::<Vec<_>>(),
            [
                ("MyApp.Service.Register", 3, 3, 1.0),
                ("MyApp.Service.Service", 4, 4, 1.0),
                ("MyApp.Service.Service", 5, 5, 1.0),
            ]
        );
    }

    #[test]
    fn vc2_csharp_bodyless_methods_are_not_codeunits() {
        let units = analyze(
            "cs",
            concat!(
                "interface I { void Contract(); }\n",
                "abstract class Base { public abstract void Abstract(); public partial void Partial(); }\n",
                "class C : Base { public override void Abstract() {} public partial void Partial() {} public void Expression() => Work(); }\n",
            ),
        );
        let symbols: Vec<_> = units.iter().map(|unit| unit.symbol.as_str()).collect();
        assert_eq!(
            symbols,
            ["C.Abstract", "C.Partial", "C.Expression"],
            "only implemented C# methods are executable units"
        );
    }

    #[test]
    fn vc2_csharp_file_scoped_namespace_qualifies_types_and_nested_callables() {
        let units = analyze(
            "cs",
            concat!(
                "namespace MyApp.Services;\n",
                "class Service {\n",
                "    void Register() { void Local() {} }\n",
                "}\n",
            ),
        );
        let symbols: Vec<_> = units.iter().map(|unit| unit.symbol.as_str()).collect();
        assert_eq!(
            symbols,
            [
                "MyApp.Services.Service.Register",
                "MyApp.Services.Service.Register.Local"
            ]
        );
    }

    #[test]
    fn vc2_csharp_foreach_and_await_foreach_count_as_decisions() {
        let units = analyze(
            "cs",
            concat!(
                "class C {\n",
                "    void Each(System.Collections.Generic.IEnumerable<int> items) { foreach (var item in items) { } }\n",
                "    async System.Threading.Tasks.Task AwaitEach(System.Collections.Generic.IAsyncEnumerable<int> items) { await foreach (var item in items) { } }\n",
                "}\n",
            ),
        );
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].complexity, 2.0);
        assert_eq!(units[1].complexity, 2.0);
    }

    #[test]
    fn csharp_operator_and_property_accessors_produce_codeunits() {
        let source = concat!(
            "class Vector {\n",
            "    public int X { get { return 0; } set { } }\n",
            "    public static implicit operator Vector(int a) { return new(); }\n",
            "}\n",
        );
        let units = analyze("cs", source);
        let symbols: Vec<_> = units.iter().map(|u| u.symbol.as_str()).collect();
        assert!(
            symbols.contains(&"Vector.operator"),
            "operator not found in {symbols:?}"
        );
        assert!(
            symbols.contains(&"Vector.X.get"),
            "get accessor not found in {symbols:?}"
        );
        assert!(
            symbols.contains(&"Vector.X.set"),
            "set accessor not found in {symbols:?}"
        );
    }

    #[test]
    fn csharp_complexity_counts_and_and_or_and_when() {
        let units = analyze(
            "cs",
            "class C { void M(bool a, bool b, bool c) { if (a && b || c) {} } }",
        );
        // 1 (base) + 1 (if_statement) + 1 (&&) + 1 (||) = 4
        assert_eq!(
            units[0].complexity, 4.0,
            "if, &&, and || should count as decisions"
        );
    }

    #[test]
    fn csharp_complexity_when_clause_counts() {
        let units = analyze(
            "cs",
            "class C { int M(int x) => x switch { 1 when x > 0 => 1, _ => 0 }; }",
        );
        // 1 (base) + 1 (switch arm) + 1 (when) = 3
        assert_eq!(
            units[0].complexity, 3.0,
            "switch arm and when clause should count as decisions"
        );
    }

    #[test]
    fn csharp_null_coalescing_not_counted() {
        let units = analyze(
            "cs",
            "class C { string M(string a, string b) { a ??= b; return a ?? b; } }",
        );
        // Only base complexity — ?? and ??= should NOT count
        assert_eq!(
            units[0].complexity, 1.0,
            "?? should not count as a decision"
        );
    }

    #[test]
    fn csharp_field_assigned_lambda_is_codeunit() {
        let source = concat!(
            "namespace N {\n",
            "    class C {\n",
            "        System.Func<int> _fn = () => 42;\n",
            "    }\n",
            "}\n",
        );
        let units = analyze("cs", source);
        let symbols: Vec<_> = units.iter().map(|u| u.symbol.as_str()).collect();
        assert!(
            !units.is_empty(),
            "field-assigned lambda should be a CodeUnit; got {symbols:?}"
        );
        assert!(
            units.iter().any(|u| u.symbol.contains("_fn")),
            "field _fn not found in {symbols:?}"
        );
    }

    #[test]
    fn csharp_local_functions_and_enums_have_exact_units_and_boundaries() {
        let units = analyze(
            "cs",
            concat!(
                "enum Color { Red, Blue }\n",
                "class C {\n",
                "    int Outer(bool value) {\n",
                "        int Inner(bool nested) { if (nested) return 1; return 0; }\n",
                "        if (value) return Inner(value);\n",
                "        return 0;\n",
                "    }\n",
                "}\n",
            ),
        );
        let mut symbols: Vec<_> = units.iter().map(|unit| unit.symbol.as_str()).collect();
        symbols.sort_unstable();
        assert_eq!(symbols, ["C.Outer", "C.Outer.Inner"]);
        assert_eq!(
            units
                .iter()
                .find(|unit| unit.symbol == "C.Outer.Inner")
                .unwrap()
                .complexity,
            2.0
        );
        assert_eq!(
            units
                .iter()
                .find(|unit| unit.symbol == "C.Outer")
                .unwrap()
                .complexity,
            2.0
        );
    }

    #[test]
    fn csharp_assigned_callables_are_emitted_once_with_own_complexity() {
        let units = analyze(
            "cs",
            concat!(
                "class C {\n",
                "    void M(bool outer) {\n",
                "        System.Func<bool, int> local = value => value && outer ? 1 : 0;\n",
                "        if (outer) local(outer);\n",
                "    }\n",
                "    System.Func<int, int> Field = (value) => value > 0 ? value : 0;\n",
                "    System.Func<int, int> Anonymous = delegate(int value) { if (value > 0) return value; return 0; };\n",
                "}\n",
            ),
        );
        let mut names: Vec<_> = units.iter().map(|unit| unit.symbol.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["C.Anonymous", "C.Field", "C.M", "C.M.local"]);
        assert_eq!(
            units
                .iter()
                .find(|unit| unit.symbol == "C.Field")
                .unwrap()
                .complexity,
            2.0
        );
        assert_eq!(
            units
                .iter()
                .find(|unit| unit.symbol == "C.Anonymous")
                .unwrap()
                .complexity,
            2.0
        );
        assert_eq!(
            units
                .iter()
                .find(|unit| unit.symbol == "C.M")
                .unwrap()
                .complexity,
            2.0
        );
    }

    #[test]
    fn csharp_reassignments_support_all_callable_forms() {
        let units = analyze(
            "cs",
            concat!(
                "class C { void M() {\n",
                "a = x => x ? 1 : 0;\n",
                "b = (bool x) => { if (x) return 1; return 0; };\n",
                "c = delegate(bool x) { if (x) return 1; return 0; };\n",
                "} int Auto { get; set; } }\n",
            ),
        );
        let actual: Vec<_> = units
            .iter()
            .map(|unit| (unit.symbol.as_str(), unit.complexity))
            .collect();
        assert_eq!(
            actual,
            [("C.M", 1.0), ("C.M.a", 2.0), ("C.M.b", 2.0), ("C.M.c", 2.0)]
        );
    }

    #[test]
    fn csharp_operator_token_and_conversion_operator_names_are_stable() {
        let units = analyze(
            "cs",
            "class Item { public static bool operator ==(Item left, Item right) => true; public static implicit operator Item(int value) => new(); }",
        );
        let symbols: Vec<_> = units.iter().map(|unit| unit.symbol.as_str()).collect();
        assert!(
            symbols.contains(&"Item.operator =="),
            "symbols: {symbols:?}"
        );
        assert!(symbols.contains(&"Item.operator"), "symbols: {symbols:?}");
    }

    #[test]
    fn csharp_properties_emit_accessors_or_expression_property_only() {
        let units = analyze(
            "cs",
            concat!(
                "class C {\n",
                "    int Value { get { if (ready) return 1; return 0; } set { if (value > 0) field = value; } }\n",
                "    int Expression => ready ? 1 : 0;\n",
                "}\n",
            ),
        );
        let mut symbols: Vec<_> = units.iter().map(|unit| unit.symbol.as_str()).collect();
        symbols.sort_unstable();
        assert_eq!(symbols, ["C.Expression", "C.Value.get", "C.Value.set"]);
        assert_eq!(
            units
                .iter()
                .find(|unit| unit.symbol == "C.Value.get")
                .unwrap()
                .complexity,
            2.0
        );
        assert_eq!(
            units
                .iter()
                .find(|unit| unit.symbol == "C.Value.set")
                .unwrap()
                .complexity,
            2.0
        );
        assert_eq!(
            units
                .iter()
                .find(|unit| unit.symbol == "C.Expression")
                .unwrap()
                .complexity,
            2.0
        );
    }

    #[test]
    fn csharp_switch_expression_counts_non_default_arms_and_guards() {
        let units = analyze(
            "cs",
            "class C { int M(int x) => x switch { 0 => 10, 1 when x > 0 => 20, _ => 30 }; }",
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].complexity, 4.0);
    }

    #[test]
    fn csharp_unguarded_switch_default_ignores_whitespace() {
        for source in [
            "class C { int M(int x) => x switch { 0 => 10, 1 => 20, _ => 30 }; }",
            "class C { int M(int x) => x switch { 0=>10, 1=>20, _=>30 }; }",
        ] {
            let units = analyze("cs", source);
            assert_eq!(units.len(), 1);
            assert_eq!(units[0].complexity, 3.0);
        }
    }

    #[test]
    fn dart_top_level_functions_produce_codeunits() {
        let units = analyze(
            "dart",
            "int add(int a, int b) { return a + b; }\nvoid greet(String name) { if (name != null) { print(name); } }\n",
        );
        let symbols: Vec<_> = units.iter().map(|u| u.symbol.as_str()).collect();
        assert!(symbols.contains(&"add"), "add not found in {symbols:?}");
        assert!(symbols.contains(&"greet"), "greet not found in {symbols:?}");
    }

    #[test]
    fn dart_adjacent_nested_and_initialized_callables_own_their_bodies() {
        let source = concat!(
            "void first(bool x) {\n",
            "  void nested(bool y) { if (y) { while (y) {} } }\n",
            "  final handler = (bool y) { if (y) {} };\n",
            "  if (x) {}\n",
            "}\n",
            "void second(bool x) {\n",
            "  if (x) { while (x) {} }\n",
            "}\n",
        );
        let units = analyze("dart", source);
        let actual: Vec<_> = units
            .iter()
            .map(|unit| {
                (
                    unit.symbol.as_str(),
                    unit.complexity,
                    unit.start_line,
                    unit.end_line,
                )
            })
            .collect();
        assert_eq!(
            actual,
            [
                ("first", 2.0, 1, 5),
                ("nested", 3.0, 2, 2),
                ("handler", 2.0, 3, 3),
                ("second", 3.0, 6, 8),
            ]
        );
    }

    #[test]
    fn dart_class_bodies_and_top_level_initializer_have_exact_units() {
        let source = concat!(
            "final handler = (bool x) { if (x) {} };\n",
            "class Widget {\n",
            "  Widget() { if (true) {} }\n",
            "  factory Widget.make() { if (true) {} return Widget(); }\n",
            "  int get value { if (true) return 1; return 0; }\n",
            "  set value(int x) { if (x > 0) {} }\n",
            "  void run(bool x) { if (x) { while (x) {} } }\n",
            "}\n",
        );
        let units = analyze("dart", source);
        let actual: Vec<_> = units
            .iter()
            .map(|unit| {
                (
                    unit.symbol.as_str(),
                    unit.complexity,
                    unit.start_line,
                    unit.end_line,
                )
            })
            .collect();
        assert_eq!(
            actual,
            [
                ("handler", 2.0, 1, 1),
                ("Widget.Widget", 2.0, 3, 3),
                ("Widget.Widget.make", 2.0, 4, 4),
                ("Widget.value", 2.0, 5, 5),
                ("Widget.value", 2.0, 6, 6),
                ("Widget.run", 3.0, 7, 7),
            ]
        );
    }

    #[test]
    fn dart_declaration_initializer_forms_emit_one_callable() {
        for (source, symbol) in [
            ("var handler = (bool x) { if (x) {} };", "handler"),
            ("Function handler = (bool x) { if (x) {} };", "handler"),
            (
                "class Widget { final handler = (bool x) { if (x) {} }; }",
                "Widget.handler",
            ),
        ] {
            let units = analyze("dart", source);
            assert_eq!(units.len(), 1, "{source}");
            assert_eq!(units[0].symbol, symbol);
            assert_eq!(units[0].complexity, 2.0);
        }
    }

    #[test]
    fn vc4_dart_named_constructors_have_distinct_symbols() {
        let units = analyze(
            "dart",
            concat!(
                "class Widget {\n",
                "  Widget() {}\n",
                "  Widget.named(bool ready) { if (ready) {} }\n",
                "  factory Widget.make(bool ready) { if (ready) { while (ready) {} } return Widget(); }\n",
                "}\n",
            ),
        );
        let actual: Vec<_> = units
            .iter()
            .map(|unit| (unit.symbol.as_str(), unit.complexity))
            .collect();
        assert_eq!(
            actual,
            [
                ("Widget.Widget", 1.0),
                ("Widget.Widget.named", 2.0),
                ("Widget.Widget.make", 3.0),
            ]
        );
    }

    #[test]
    fn dart_methods_produce_qualified_codeunits() {
        let units = analyze(
            "dart",
            concat!(
                "class Calculator {\n",
                "  int add(int a, int b) { return a + b; }\n",
                "  int multiply(int a, int b) { return a * b; }\n",
                "}\n",
            ),
        );
        let symbols: Vec<_> = units.iter().map(|u| u.symbol.as_str()).collect();
        assert!(
            symbols.contains(&"Calculator.add"),
            "Calculator.add not found in {symbols:?}"
        );
        assert!(
            symbols.contains(&"Calculator.multiply"),
            "Calculator.multiply not found in {symbols:?}"
        );
    }

    #[test]
    fn dart_getter_and_setter_produce_codeunits() {
        let units = analyze(
            "dart",
            concat!(
                "class Counter {\n",
                "  int _value = 0;\n",
                "  int get value => _value;\n",
                "  set value(int v) { _value = v; }\n",
                "}\n",
            ),
        );
        let symbols: Vec<_> = units.iter().map(|u| u.symbol.as_str()).collect();
        assert!(
            symbols.contains(&"Counter.value"),
            "Counter.value not found in {symbols:?}"
        );
    }

    #[test]
    fn dart_constructor_produces_codeunit() {
        let units = analyze(
            "dart",
            concat!(
                "class Point {\n",
                "  int x, y;\n",
                "  Point(this.x, this.y) {}\n",
                "}\n",
            ),
        );
        let symbols: Vec<_> = units.iter().map(|u| u.symbol.as_str()).collect();
        assert!(
            symbols.contains(&"Point.Point"),
            "Point.Point not found in {symbols:?}"
        );
    }

    #[test]
    fn dart_factory_constructor_produces_codeunit() {
        let units = analyze(
            "dart",
            concat!(
                "class Logger {\n",
                "  static Logger factory(String name) { return Logger(); }\n",
                "}\n",
            ),
        );
        let symbols: Vec<_> = units.iter().map(|u| u.symbol.as_str()).collect();
        assert!(
            symbols.contains(&"Logger.factory"),
            "Logger.factory not found in {symbols:?}"
        );
    }

    #[test]
    fn dart_complexity_counts_correctly() {
        let units = analyze(
            "dart",
            concat!(
                "class Validator {\n",
                "  bool check(String input, bool strict) {\n",
                "    if (input == null) return false;\n",
                "    if (strict && input.isEmpty) return false;\n",
                "    return true;\n",
                "  }\n",
                "}\n",
            ),
        );
        let check = units.iter().find(|u| u.symbol == "Validator.check");
        assert!(
            check.is_some(),
            "Validator.check not found; got {:?}",
            units.iter().map(|u| u.symbol.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(check.unwrap().complexity, 4.0);
    }
}
