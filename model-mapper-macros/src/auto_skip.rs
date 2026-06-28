//! Helpers to read the source file of a referenced type and extract its field
//! names. Used by the `auto_skip` feature to detect fields that exist on the
//! other type but not on self (or vice versa).

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use proc_macro2::Span;
use syn::{Fields, Ident, Item, ItemUse, TypePath, UseTree, spanned::Spanned};

/// Attempt to discover the named fields of the struct/enum referenced by `ty`
/// by reading the source file where the type was referenced and following any
/// `use` imports as needed.
///
/// Returns `None` if the type or its file cannot be resolved. In that case the
/// caller should fall back to default behavior (compile-time error) or
/// warn the user.
pub(super) fn discover_other_type_fields(ty: &TypePath) -> Option<HashSet<Ident>> {
    discover_other_type_field_info(ty).map(|info| info.into_keys().collect())
}

/// Attempt to discover the named fields of the struct/enum referenced by `ty`
/// along with their declared types, by reading the source file where the type
/// was referenced and following any `use` imports as needed.
///
/// The returned `Type` is the exact `syn::Type` declared on the field, so
/// callers can inspect the type to apply special conversion rules
/// (e.g. `HasOne<…>` / `HasMany<…>` from sea-orm).
pub(super) fn discover_other_type_field_info(ty: &TypePath) -> Option<HashMap<Ident, syn::Type>> {
    // Step 1: get the file where `ty` is referenced.
    let origin_file = source_file_of_span(&ty.span())?;

    // Step 2: read and parse the file.
    let origin = read_and_parse(&origin_file)?;

    // Step 3: get the type name.
    let type_name = ty.path.segments.last()?.ident.clone();

    // Step 4: first, look for the type in the current file (handles inline
    // modules and the trivial case where the type lives next to the
    // `#[mapper]` call).
    if let Some(info) = find_named_fields_with_types_in_file(&origin, &origin_file, "", &type_name, &mut Vec::new()) {
        return Some(info);
    }

    // Step 5: walk every `use` statement and try to resolve it to a file that
    // might contain the type. The original implementation only matched use
    // statements that explicitly imported the type name (e.g.
    // `use foo::Bar;`). That breaks for the very common pattern
    // `use crate::entity::foo;` followed by a reference to
    // `crate::entity::foo::Bar` – the use only imports the *module*, not the
    // type, so the macro can't find the source file.
    for item in &origin.items {
        if let Item::Use(use_stmt) = item {
            for resolved_file in collect_resolved_files(use_stmt, &origin_file) {
                if let Some(content) = read_and_parse(&resolved_file) {
                    if let Some(info) =
                        find_named_fields_with_types_in_file(&content, &resolved_file, "", &type_name, &mut Vec::new())
                    {
                        return Some(info);
                    }
                }
            }
        }
    }

    // Step 6 (sea-orm 2.0 fallback): if the type ends with `Ex` (e.g.
    // `ModelEx`) we couldn't find it because `#[sea_orm::model]` generates
    // it from the companion `Model` struct. The generated `ModelEx` mirrors
    // `Model` field-for-field, so we can fall back to `Model`'s definition.
    if let Some(stripped) = type_name.to_string().strip_suffix("Ex") {
        if let Ok(alt_ident) = syn::parse_str::<Ident>(stripped) {
            if let Some(info) =
                find_named_fields_with_types_in_file(&origin, &origin_file, "", &alt_ident, &mut Vec::new())
            {
                return Some(info);
            }
            for item in &origin.items {
                if let Item::Use(use_stmt) = item {
                    for resolved_file in collect_resolved_files(use_stmt, &origin_file) {
                        if let Some(content) = read_and_parse(&resolved_file) {
                            if let Some(info) = find_named_fields_with_types_in_file(
                                &content,
                                &resolved_file,
                                "",
                                &alt_ident,
                                &mut Vec::new(),
                            ) {
                                return Some(info);
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// Walk a `use` tree and resolve every leaf to a file path. For grouped uses
/// such as `use foo::{a, b};` this returns one file per leaf. Globs are
/// ignored (we can't know what's in them without parsing the target file).
fn collect_resolved_files(use_stmt: &ItemUse, current_file: &Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    collect_from_tree(&use_stmt.tree, &[], current_file, &mut results);
    results
}

fn collect_from_tree(tree: &UseTree, prefix: &[String], current_file: &Path, results: &mut Vec<PathBuf>) {
    match tree {
        UseTree::Path(p) => {
            let mut new_prefix = prefix.to_vec();
            new_prefix.push(p.ident.to_string());
            collect_from_tree(&p.tree, &new_prefix, current_file, results);
        }
        UseTree::Name(n) => {
            // The leaf is a type/value being imported. Try two strategies:
            //
            // 1. Resolve the full path as a module (e.g. `use crate::foo::bar;` re-exports a sub-module). This is
            //    mostly relevant for glob re-exports like `pub use self::foo::*;` (handled elsewhere) and grouped uses
            //    with explicit module paths.
            // 2. Resolve the *prefix* (everything before the leaf) as a module file, then look for the type inside that
            //    file. This handles the common pattern `use crate::entity::foo::Model;` where `Model` is a
            //    type/struct/enum/type-alias inside `foo.rs`, not a module of its own.
            let mut full_path = prefix.to_vec();
            full_path.push(n.ident.to_string());
            if let Some(file) = resolve_path_to_file(&full_path, current_file) {
                if file.is_file() {
                    results.push(file);
                }
            }
            if !prefix.is_empty() {
                if let Some(file) = resolve_path_to_file(prefix, current_file) {
                    if file.is_file() {
                        results.push(file);
                    }
                }
            }
        }
        UseTree::Rename(r) => {
            // `use foo::Bar as Baz;` — `Baz` is the local alias, `Bar` is the
            // original name. We resolve by the original name.
            let mut full_path = prefix.to_vec();
            full_path.push(r.ident.to_string());
            if let Some(file) = resolve_path_to_file(&full_path, current_file) {
                if file.is_file() {
                    results.push(file);
                }
            }
            if !prefix.is_empty() {
                if let Some(file) = resolve_path_to_file(prefix, current_file) {
                    if file.is_file() {
                        results.push(file);
                    }
                }
            }
        }
        UseTree::Glob(_) => {
            // Can't enumerate a glob without reading the target.
        }
        UseTree::Group(g) => {
            for item in &g.items {
                collect_from_tree(item, prefix, current_file, results);
            }
        }
    }
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
    // The visited key includes the type_name so that we can recurse into the
    // same file looking for a *different* type (e.g. when following a type
    // alias like `pub type MemberUser = Model;`). Without the type_name in
    // the key, the recursion would be cut short.
    let key = format!("{}::{}::{}", file_path.display(), mod_path, type_name);
    if visited.contains(&key) {
        return None;
    }
    visited.push(key);

    // First pass: look for a struct/enum/type-alias with the matching name in this file.
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
            Item::Type(t) if t.ident == *type_name => {
                // Type alias: e.g. `pub type MemberUser = Model;`. Follow the
                // alias to the underlying type. The underlying type is most
                // often a path that resolves to a struct in the same file
                // (e.g. `Model` or `Entity::Model`). We recurse on the alias
                // target's last segment; if that fails we fall through to the
                // next pass and the `..Default::default()` fallback in
                // `derive_struct_into` will save us.
                if let syn::Type::Path(target_path) = &*t.ty {
                    if let Some(last_seg) = target_path.path.segments.last() {
                        let target_ident = last_seg.ident.clone();
                        if target_ident != *type_name {
                            if let Some(fields) =
                                find_named_fields_in_file(file, file_path, mod_path, &target_ident, visited)
                            {
                                return Some(fields);
                            }
                        }
                    }
                }
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
                if let Some(fields) = find_named_fields_in_file(&nested, file_path, &nested_path, type_name, visited) {
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
                    if let Some(fields) = find_named_fields_in_file(&content, &resolved, "", type_name, visited) {
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

/// Same as [`find_named_fields_in_file`] but also returns the declared `syn::Type`
/// of every field, so callers can apply type-driven conversion rules (e.g.
/// sea-orm `HasOne<…>` / `HasMany<…>`).
fn find_named_fields_with_types_in_file(
    file: &syn::File,
    file_path: &Path,
    mod_path: &str,
    type_name: &Ident,
    visited: &mut Vec<String>,
) -> Option<HashMap<Ident, syn::Type>> {
    // The visited key includes the type_name so that we can recurse into the
    // same file looking for a *different* type (e.g. when following a type
    // alias like `pub type MemberUser = Model;`). Without the type_name in
    // the key, the recursion would be cut short.
    let key = format!("{}::{}::{}", file_path.display(), mod_path, type_name);
    if visited.contains(&key) {
        return None;
    }
    visited.push(key);

    // First pass: look for a struct/enum/type-alias with the matching name in this file.
    for item in &file.items {
        match item {
            Item::Struct(s) if s.ident == *type_name => {
                return Some(extract_named_fields_with_types(&s.fields));
            }
            Item::Enum(e) if e.ident == *type_name => {
                // Enums don't have typed fields; mimic `extract_named_fields`
                // but use the variant ident as both key and a unit type.
                let mut map = HashMap::new();
                for v in &e.variants {
                    map.insert(
                        v.ident.clone(),
                        syn::Type::Tuple(syn::TypeTuple {
                            paren_token: syn::token::Paren::default(),
                            elems: syn::punctuated::Punctuated::new(),
                        }),
                    );
                }
                return Some(map);
            }
            Item::Type(t) if t.ident == *type_name => {
                // Type alias: follow it to the underlying type.
                if let syn::Type::Path(target_path) = &*t.ty {
                    if let Some(last_seg) = target_path.path.segments.last() {
                        let target_ident = last_seg.ident.clone();
                        if target_ident != *type_name {
                            if let Some(fields) =
                                find_named_fields_with_types_in_file(file, file_path, mod_path, &target_ident, visited)
                            {
                                return Some(fields);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Second pass: walk `mod` items to find inline modules.
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
                if let Some(fields) =
                    find_named_fields_with_types_in_file(&nested, file_path, &nested_path, type_name, visited)
                {
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
                    if let Some(fields) =
                        find_named_fields_with_types_in_file(&content, &resolved, "", type_name, visited)
                    {
                        return Some(fields);
                    }
                }
            }
        }
    }

    None
}

fn extract_named_fields_with_types(fields: &Fields) -> HashMap<Ident, syn::Type> {
    let mut map = HashMap::new();
    if let Fields::Named(named) = fields {
        for f in &named.named {
            if let Some(ident) = &f.ident {
                map.insert(ident.clone(), f.ty.clone());
            }
        }
    }
    map
}

/// If `use_stmt` re-exports `type_name` (possibly as one of many in a group),
/// return the resolved file path that actually defines the type.
fn resolve_use_for_type(use_stmt: &ItemUse, type_name: &Ident, current_file: &Path) -> Option<PathBuf> {
    // Only consider absolute paths starting with `crate::`, `::crate::`,
    // `super::`, `self::`, or external crate names.
    let tree = &use_stmt.tree;
    resolve_tree(tree, &[], type_name, current_file)
}

fn resolve_tree(tree: &UseTree, prefix: &[String], type_name: &Ident, current_file: &Path) -> Option<PathBuf> {
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
