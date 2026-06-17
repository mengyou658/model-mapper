//! Helpers to read the source file of a referenced type and extract its field
//! names. Used by the `auto_skip` feature to detect fields that exist on the
//! other type but not on self (or vice versa).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::{Fields, Ident, Item, ItemUse, TypePath, UseTree};

/// Attempt to discover the named fields of the struct/enum referenced by `ty`
/// by reading the source file where the type was referenced and following any
/// `use` imports as needed.
///
/// Returns `None` if the type or its file cannot be resolved. In that case the
/// caller should fall back to default behavior (compile-time error) or
/// warn the user.
pub(super) fn discover_other_type_fields(ty: &TypePath) -> Option<HashSet<Ident>> {
    // Step 1: get the file where `ty` is referenced.
    let origin_file = source_file_of_span(&ty.span())?;

    // Step 2: read and parse the file.
    let origin = read_and_parse(&origin_file)?;

    // Step 3: get the type name.
    let type_name = ty.path.segments.last()?.ident.clone();

    // Step 4: try to find the struct/enum in this file, possibly following `use`.
    find_named_fields_in_file(&origin, &origin_file, "", &type_name, &mut Vec::new())
}

fn source_file_of_span(span: &Span) -> Option<PathBuf> {
    // Convert proc_macro2::Span -> proc_macro::Span (stable in proc-macro context).
    let proc_span: proc_macro::Span = span.unwrap();
    // In recent rustc versions, `Span::file()` returns a `String` directly.
    // Synthetic spans (e.g. from `macro_rules!` expansions) yield names like
    // "<command line>" or "<input>" — reading those will simply fail in
    // `read_and_parse`, and the caller falls back to the default behavior.
    let path = proc_span.file();
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

fn read_and_parse(path: &Path) -> Option<syn::File> {
    let content = std::fs::read_to_string(path).ok()?;
    syn::parse_file(&content).ok()
}

/// Find the fields of a struct/enum named `type_name` in the given file, or in
/// any file reachable through `use` statements. To avoid infinite recursion on
/// cyclic imports, we keep a stack of visited file paths. Note: nested
/// `mod foo { ... }` items share the same file path, so we differentiate by
/// mod path.
fn find_named_fields_in_file(
    file: &syn::File,
    file_path: &Path,
    mod_path: &str,
    type_name: &Ident,
    visited: &mut Vec<String>,
) -> Option<HashSet<Ident>> {
    let key = format!("{}::{}", file_path.display(), mod_path);
    if visited.contains(&key) {
        return None;
    }
    visited.push(key);

    // First pass: look for a struct/enum with the matching name in this file.
    for item in &file.items {
        match item {
            Item::Struct(s) if s.ident == *type_name => {
                return Some(extract_named_fields(&s.fields));
            }
            Item::Enum(e) if e.ident == *type_name => {
                // Enums: collect variant names. The user asked for "fields" but
                // for enums, fields aren't applicable; we just bail out so
                // default error behavior applies.
                let mut set = HashSet::new();
                for v in &e.variants {
                    set.insert(v.ident.clone());
                }
                return Some(set);
            }
            _ => {}
        }
    }

    // Second pass: walk `mod` items to find inline modules that define
    // the type in the same file. The type might be nested in a `mod foo { ... }`.
    for item in &file.items {
        if let Item::Mod(m) = item {
            if let Some((_, items)) = &m.content {
                let nested = syn::File {
                    shebang: None,
                    attrs: vec![],
                    items: items.clone(),
                };
                let nested_path = if mod_path.is_empty() {
                    m.ident.to_string()
                } else {
                    format!("{}::{}", mod_path, m.ident)
                };
                if let Some(fields) = find_named_fields_in_file(
                    &nested,
                    file_path,
                    &nested_path,
                    type_name,
                    visited,
                ) {
                    return Some(fields);
                }
            }
        }
    }

    // Third pass: walk `use` statements that re-export `type_name`.
    for item in &file.items {
        if let Item::Use(use_stmt) = item {
            if let Some(resolved) = resolve_use_for_type(use_stmt, type_name, file_path) {
                if let Some(content) = read_and_parse(&resolved) {
                    if let Some(fields) = find_named_fields_in_file(
                        &content,
                        &resolved,
                        "",
                        type_name,
                        visited,
                    ) {
                        return Some(fields);
                    }
                }
            }
        }
    }

    None
}

fn extract_named_fields(fields: &Fields) -> HashSet<Ident> {
    let mut set = HashSet::new();
    if let Fields::Named(named) = fields {
        for f in &named.named {
            if let Some(ident) = &f.ident {
                set.insert(ident.clone());
            }
        }
    }
    set
}

/// If `use_stmt` re-exports `type_name` (possibly as one of many in a group),
/// return the resolved file path that actually defines the type.
fn resolve_use_for_type(
    use_stmt: &ItemUse,
    type_name: &Ident,
    current_file: &Path,
) -> Option<PathBuf> {
    // Only consider absolute paths starting with `crate::`, `::crate::`,
    // `super::`, `self::`, or external crate names.
    let tree = &use_stmt.tree;
    resolve_tree(tree, &[], type_name, current_file)
}

fn resolve_tree(
    tree: &UseTree,
    prefix: &[String],
    type_name: &Ident,
    current_file: &Path,
) -> Option<PathBuf> {
    match tree {
        UseTree::Path(p) => {
            let mut new_prefix = prefix.to_vec();
            new_prefix.push(p.ident.to_string());
            resolve_tree(&p.tree, &new_prefix, type_name, current_file)
        }
        UseTree::Name(n) => {
            if n.ident == *type_name {
                resolve_path_to_file(prefix, current_file)
            } else {
                None
            }
        }
        UseTree::Rename(r) => {
            // `use foo::Bar as Baz;` — the original is `Bar`, the alias is `Baz`.
            if r.rename == *type_name {
                let mut new_prefix = prefix.to_vec();
                new_prefix.push(r.ident.to_string());
                resolve_path_to_file(&new_prefix, current_file)
            } else {
                None
            }
        }
        UseTree::Glob(_) => None,
        UseTree::Group(g) => {
            for item in &g.items {
                if let Some(resolved) = resolve_tree(item, prefix, type_name, current_file) {
                    return Some(resolved);
                }
            }
            None
        }
    }
}

/// Given a path like `["crate", "model", "admin", "admin_member_order"]` and the
/// current source file, try to resolve it to a real file on disk.
///
/// - `crate::...` resolves relative to the current crate's `src/` directory.
/// - `super::...` resolves relative to the current file's parent.
/// - Otherwise (external crate, etc.) we bail out and return None.
fn resolve_path_to_file(path: &[String], current_file: &Path) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }

    let first = &path[0];
    if first == "crate" {
        // Resolve from the crate's `src/` directory.
        let src_dir = find_crate_src_dir(current_file)?;
        let mut p = src_dir.to_path_buf();
        for segment in &path[1..] {
            p.push(segment);
        }
        Some(try_module_file(&p))
    } else if first == "self" {
        let dir = current_file.parent()?;
        let mut p = dir.to_path_buf();
        for segment in &path[1..] {
            p.push(segment);
        }
        Some(try_module_file(&p))
    } else if first == "super" {
        // Walk up `n` levels for each `super` we encounter consecutively.
        let mut p = current_file.parent()?.to_path_buf();
        let mut i = 0;
        while i < path.len() && path[i] == "super" {
            p = p.parent()?.to_path_buf();
            i += 1;
        }
        for segment in &path[i..] {
            p.push(segment);
        }
        Some(try_module_file(&p))
    } else {
        // External crate; we can't read the source.
        None
    }
}

/// Find the crate's `src/` directory by walking up from the current file
/// looking for a `Cargo.toml`. We do this rather than using `CARGO_MANIFEST_DIR`
/// because the macro runs at compile time without guaranteed env access from
/// every Cargo invocation; walking up is reliable.
fn find_crate_src_dir(file: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = file.parent();
    while let Some(dir) = cur {
        if dir.join("Cargo.toml").is_file() {
            return Some(dir.join("src"));
        }
        cur = dir.parent();
    }
    None
}

/// Try to find the actual file for a module path. Rust modules can be either
/// `name.rs` or `name/mod.rs`.
fn try_module_file(p: &Path) -> PathBuf {
    let as_rs = p.with_extension("rs");
    if as_rs.is_file() {
        return as_rs;
    }
    let mod_rs = p.join("mod.rs");
    if mod_rs.is_file() {
        return mod_rs;
    }
    as_rs
}
