//! Instant `syn`-powered structural skeletons (`alsp outline`) and reusable item-declaration
//! extraction (`format_item_declaration`).
//!
//! Local-path outlining parses Rust files directly with `syn` — zero `rust-analyzer`, zero
//! `cargo`, zero build scripts or proc macros. All filesystem traversal checks `symlink_metadata`
//! and refuses symlinks or paths that escape the canonical root.

use alsp_evidence::{escape, Provenance};
use alsp_index::Span;
use proc_macro2::Span as PSpan;
use quote::ToTokens;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use syn::spanned::Spanned;

/// A single structural item in a Rust file (struct, enum, trait, impl, fn, field, variant, ...).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutlineItem {
    pub kind: String,
    pub vis: String,
    pub name: String,
    pub signature: String,
    pub span: Span,
    pub children: Vec<OutlineItem>,
}

/// Structural skeleton of one Rust source file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileOutline {
    pub file: String,
    pub lines: usize,
    pub items: Vec<OutlineItem>,
}

/// Complete answer returned by `alsp outline`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutlineAnswer {
    pub schema: u32,
    pub command: String,
    pub provenance: Provenance,
    pub files: Vec<FileOutline>,
    pub notes: Vec<String>,
}

impl OutlineAnswer {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("outline answer serializes") + "\n"
    }

    pub fn to_text(&self) -> String {
        let mut out = Vec::new();
        let label = match self.files.as_slice() {
            [] => "(0 files)".to_string(),
            [only] => escape(&only.file).into_owned(),
            files if files.len() <= 3 => {
                files.iter().map(|f| escape(&f.file).into_owned()).collect::<Vec<_>>().join(", ")
            }
            files => format!("{} files", files.len()),
        };
        out.push(format!("outline {label} [{}]", self.provenance.as_str()));
        for note in &self.notes {
            out.push(format!("! {}", escape(note)));
        }
        for f in &self.files {
            out.push(format!("=== {} ({} lines) ===", escape(&f.file), f.lines));
            for item in &f.items {
                render_item(&mut out, item, 0);
            }
        }
        out.join("\n") + "\n"
    }
}

fn render_item(out: &mut Vec<String>, item: &OutlineItem, depth: usize) {
    let indent = "  ".repeat(depth);
    let span = format_span(item.span);
    out.push(format!("{indent}{} {span}", escape(&item.signature)));
    for child in &item.children {
        render_item(out, child, depth + 1);
    }
}

fn format_span(s: Span) -> String {
    if s.start == s.end {
        format!("L{}", s.start)
    } else {
        format!("L{}-{}", s.start, s.end)
    }
}

fn span_of(s: PSpan) -> Span {
    Span { start: s.start().line as u32, end: s.end().line as u32 }
}

fn is_pub(v: &syn::Visibility) -> bool {
    matches!(v, syn::Visibility::Public(_))
}

fn is_macro_export(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("macro_export"))
}

fn vis_str(v: &syn::Visibility) -> String {
    match v {
        syn::Visibility::Public(_) => "pub".into(),
        syn::Visibility::Restricted(r) => clean_tokens(r),
        syn::Visibility::Inherited => String::new(),
    }
}

fn vis_prefix(v: &syn::Visibility) -> String {
    let s = vis_str(v);
    if s.is_empty() { String::new() } else { format!("{s} ") }
}

/// Normalize `quote::ToTokens` output into compact, single-line idiomatic Rust syntax.
fn clean_tokens(t: impl ToTokens) -> String {
    let raw = t.to_token_stream().to_string();
    normalize_rust_tokens(&raw)
}

fn normalize_rust_tokens(raw: &str) -> String {
    // Collapse all whitespace (including embedded newlines/tabs) to single spaces.
    let mut s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    // Punctuation spacing cleanups.
    for (from, to) in [
        (" :: ", "::"),
        (":: <", "::<"),
        (" , ", ", "),
        (" ,", ","),
        (" ;", ";"),
        (" : ", ": "),
        (" :", ":"),
        ("( ", "("),
        (" )", ")"),
        ("[ ", "["),
        (" ]", "]"),
        (" !", "!"),
        ("& mut ", "&mut "),
        ("& '", "&'"),
        ("& ", "&"),
        ("* const ", "*const "),
        ("* mut ", "*mut "),
        ("< ", "<"),
        (" < ", "<"),
        (" <", "<"),
    ] {
        s = s.replace(from, to);
    }
    // Close spaces before `>` without disturbing `->` or `=>`.
    loop {
        let next = strip_space_before_gt(&s);
        if next == s {
            break;
        }
        s = next;
    }
    // Remove space before `(` after identifiers, `>`, `)`, or `pub` (but not after keywords/arrows).
    s = tighten_open_paren(&s);
    // Restore space in `impl<`? No, `impl<T>` is idiomatic! Restore space after `where` if needed,
    // and ensure `->` has spaces around it.
    s = s.replace("->", " -> ");
    // Collapse any double spaces introduced above.
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_space_before_gt(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b' ' && bytes[i + 1] == b'>' {
            // Keep space before `>` if preceded by `-` or `=` (already `->`/`=>` have no space)
            // or if it looks like a binary operator? In type/fn signatures ` >` is always closing generic.
            i += 1;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn tighten_open_paren(s: &str) -> String {
    const KEEP_SPACE_BEFORE_PAREN: &[&str] = &[
        "for", "in", "where", "mut", "dyn", "impl", "ref", "const", "return", "as", "else", "if",
        "while", "match", "async", "unsafe", "extern",
    ];
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == ' ' && chars[i + 1] == '(' {
            let prev = out.chars().last();
            let prev_is_arrow = out.ends_with("->") || out.ends_with("=>");
            let prev_word = last_word(&out);
            let should_tighten = !prev_is_arrow
                && prev.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '>' || c == ')')
                && !KEEP_SPACE_BEFORE_PAREN.contains(&prev_word);
            if should_tighten {
                i += 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn last_word(s: &str) -> &str {
    s.rsplit(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap_or("")
}

fn format_where(w: &Option<syn::WhereClause>) -> String {
    match w {
        Some(wc) => format!(" {}", clean_tokens(wc)),
        None => String::new(),
    }
}

fn format_generics_params(g: &syn::Generics) -> String {
    if g.params.is_empty() {
        String::new()
    } else {
        clean_tokens(&g.params).trim().to_string().pipe(|p| format!("<{p}>"))
    }
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

fn format_fn_sig(vis: &syn::Visibility, sig: &syn::Signature) -> String {
    let vp = vis_prefix(vis);
    let mut parts = Vec::new();
    if sig.constness.is_some() {
        parts.push("const".to_string());
    }
    if sig.asyncness.is_some() {
        parts.push("async".to_string());
    }
    if sig.unsafety.is_some() {
        parts.push("unsafe".to_string());
    }
    if let Some(abi) = &sig.abi {
        parts.push(clean_tokens(abi));
    }
    let gens = format_generics_params(&sig.generics);
    let inputs = sig.inputs.iter().map(clean_tokens).collect::<Vec<_>>().join(", ");
    let variadic = if sig.variadic.is_some() {
        if inputs.is_empty() { "...".to_string() } else { ", ...".to_string() }
    } else {
        String::new()
    };
    let ret = match &sig.output {
        syn::ReturnType::Default => String::new(),
        syn::ReturnType::Type(_, ty) => format!(" -> {}", clean_tokens(ty)),
    };
    let wc = format_where(&sig.generics.where_clause);
    let prefix = if parts.is_empty() { String::new() } else { format!("{} ", parts.join(" ")) };
    format!("{vp}{prefix}fn {}{gens}({inputs}{variadic}){ret}{wc}", sig.ident)
}

fn format_field(f: &syn::Field) -> String {
    let vp = vis_prefix(&f.vis);
    let ty = clean_tokens(&f.ty);
    match &f.ident {
        Some(id) => format!("{vp}{id}: {ty}"),
        None => format!("{vp}{ty}"),
    }
}

fn format_variant(v: &syn::Variant) -> String {
    let base = match &v.fields {
        syn::Fields::Unit => v.ident.to_string(),
        syn::Fields::Unnamed(u) => {
            let fs = u.unnamed.iter().map(format_field).collect::<Vec<_>>().join(", ");
            format!("{}({fs})", v.ident)
        }
        syn::Fields::Named(n) => {
            let fs = n.named.iter().map(format_field).collect::<Vec<_>>().join(", ");
            format!("{} {{ {fs} }}", v.ident)
        }
    };
    match &v.discriminant {
        Some((_, expr)) => format!("{base} = {}", clean_tokens(expr)),
        None => base,
    }
}

fn format_struct_sig(s: &syn::ItemStruct) -> String {
    let vp = vis_prefix(&s.vis);
    let gens = format_generics_params(&s.generics);
    let wc = format_where(&s.generics.where_clause);
    match &s.fields {
        syn::Fields::Unnamed(u) => {
            let fs = u.unnamed.iter().map(format_field).collect::<Vec<_>>().join(", ");
            format!("{vp}struct {}{gens}({fs}){wc}", s.ident)
        }
        syn::Fields::Named(_) | syn::Fields::Unit => {
            format!("{vp}struct {}{gens}{wc}", s.ident)
        }
    }
}

fn format_enum_sig(e: &syn::ItemEnum) -> String {
    let vp = vis_prefix(&e.vis);
    let gens = format_generics_params(&e.generics);
    let wc = format_where(&e.generics.where_clause);
    format!("{vp}enum {}{gens}{wc}", e.ident)
}

fn format_trait_sig(t: &syn::ItemTrait) -> String {
    let vp = vis_prefix(&t.vis);
    let uns = if t.unsafety.is_some() { "unsafe " } else { "" };
    let auto = if t.auto_token.is_some() { "auto " } else { "" };
    let gens = format_generics_params(&t.generics);
    let supers = if t.supertraits.is_empty() {
        String::new()
    } else {
        format!(": {}", t.supertraits.iter().map(clean_tokens).collect::<Vec<_>>().join(" + "))
    };
    let wc = format_where(&t.generics.where_clause);
    format!("{vp}{uns}{auto}trait {}{gens}{supers}{wc}", t.ident)
}

fn format_impl_header(imp: &syn::ItemImpl) -> (String, String) {
    let self_ty = clean_tokens(&imp.self_ty);
    let name = match &imp.trait_ {
        Some((bang, tr, _)) => {
            let neg = if bang.is_some() { "!" } else { "" };
            format!("{neg}{} for {self_ty}", clean_tokens(tr))
        }
        None => self_ty,
    };
    let uns = if imp.unsafety.is_some() { "unsafe " } else { "" };
    let gens = format_generics_params(&imp.generics);
    let wc = format_where(&imp.generics.where_clause);
    let sig = format!("{uns}impl{gens} {name}{wc}");
    (name, sig)
}

fn format_const_sig(vis: &syn::Visibility, ident: &syn::Ident, ty: &syn::Type) -> String {
    format!("{}const {ident}: {}", vis_prefix(vis), clean_tokens(ty))
}

fn format_static_sig(s: &syn::ItemStatic) -> String {
    let m = match s.mutability {
        syn::StaticMutability::Mut(_) => "mut ",
        _ => "",
    };
    format!("{}static {m}{}: {}", vis_prefix(&s.vis), s.ident, clean_tokens(&s.ty))
}

fn format_type_sig(
    vis: &syn::Visibility,
    ident: &syn::Ident,
    generics: &syn::Generics,
    ty: &syn::Type,
) -> String {
    let gens = format_generics_params(generics);
    let wc = format_where(&generics.where_clause);
    format!("{}type {ident}{gens}{wc} = {}", vis_prefix(vis), clean_tokens(ty))
}

fn format_trait_type(t: &syn::TraitItemType) -> String {
    let gens = format_generics_params(&t.generics);
    let bounds = if t.bounds.is_empty() {
        String::new()
    } else {
        format!(": {}", t.bounds.iter().map(clean_tokens).collect::<Vec<_>>().join(" + "))
    };
    let def = match &t.default {
        Some((_, ty)) => format!(" = {}", clean_tokens(ty)),
        None => String::new(),
    };
    let wc = format_where(&t.generics.where_clause);
    format!("type {}{gens}{bounds}{wc}{def}", t.ident)
}

fn format_trait_const(c: &syn::TraitItemConst) -> String {
    format!("const {}: {}", c.ident, clean_tokens(&c.ty))
}

/// Parse a single Rust source string into a [`FileOutline`].
///
/// When `pub_only` is true, private items, private struct fields, and private inherent `impl`
/// methods are omitted; trait `impl` blocks and their methods are kept.
pub fn outline_source(file: &str, src: &str, pub_only: bool) -> Result<FileOutline, String> {
    let parsed = syn::parse_file(src).map_err(|e| format!("syntax error in {file}: {e}"))?;
    let items = collect_items(&parsed.items, pub_only);
    Ok(FileOutline { file: file.to_string(), lines: src.lines().count(), items })
}

fn collect_items(items: &[syn::Item], pub_only: bool) -> Vec<OutlineItem> {
    let mut out = Vec::new();
    for item in items {
        if let Some(oi) = outline_item(item, pub_only) {
            out.push(oi);
        }
    }
    out
}

fn outline_item(item: &syn::Item, pub_only: bool) -> Option<OutlineItem> {
    match item {
        syn::Item::Struct(s) => {
            if pub_only && !is_pub(&s.vis) {
                return None;
            }
            let children = match &s.fields {
                syn::Fields::Named(named) => named
                    .named
                    .iter()
                    .filter(|f| !pub_only || is_pub(&f.vis))
                    .map(|f| OutlineItem {
                        kind: "field".into(),
                        vis: vis_str(&f.vis),
                        name: f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default(),
                        signature: format_field(f),
                        span: span_of(f.span()),
                        children: Vec::new(),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            Some(OutlineItem {
                kind: "struct".into(),
                vis: vis_str(&s.vis),
                name: s.ident.to_string(),
                signature: format_struct_sig(s),
                span: span_of(s.span()),
                children,
            })
        }
        syn::Item::Enum(e) => {
            if pub_only && !is_pub(&e.vis) {
                return None;
            }
            let children = e
                .variants
                .iter()
                .map(|v| OutlineItem {
                    kind: "variant".into(),
                    vis: String::new(),
                    name: v.ident.to_string(),
                    signature: format_variant(v),
                    span: span_of(v.span()),
                    children: Vec::new(),
                })
                .collect();
            Some(OutlineItem {
                kind: "enum".into(),
                vis: vis_str(&e.vis),
                name: e.ident.to_string(),
                signature: format_enum_sig(e),
                span: span_of(e.span()),
                children,
            })
        }
        syn::Item::Trait(t) => {
            if pub_only && !is_pub(&t.vis) {
                return None;
            }
            let mut children = Vec::new();
            for ti in &t.items {
                match ti {
                    syn::TraitItem::Fn(f) => children.push(OutlineItem {
                        kind: "fn".into(),
                        vis: String::new(),
                        name: f.sig.ident.to_string(),
                        signature: format_fn_sig(&syn::Visibility::Inherited, &f.sig),
                        span: span_of(f.span()),
                        children: Vec::new(),
                    }),
                    syn::TraitItem::Type(ty) => children.push(OutlineItem {
                        kind: "type".into(),
                        vis: String::new(),
                        name: ty.ident.to_string(),
                        signature: format_trait_type(ty),
                        span: span_of(ty.span()),
                        children: Vec::new(),
                    }),
                    syn::TraitItem::Const(c) => children.push(OutlineItem {
                        kind: "const".into(),
                        vis: String::new(),
                        name: c.ident.to_string(),
                        signature: format_trait_const(c),
                        span: span_of(c.span()),
                        children: Vec::new(),
                    }),
                    _ => {}
                }
            }
            Some(OutlineItem {
                kind: "trait".into(),
                vis: vis_str(&t.vis),
                name: t.ident.to_string(),
                signature: format_trait_sig(t),
                span: span_of(t.span()),
                children,
            })
        }
        syn::Item::Impl(imp) => {
            let is_trait_impl = imp.trait_.is_some();
            let (name, signature) = format_impl_header(imp);
            let mut children = Vec::new();
            for ii in &imp.items {
                match ii {
                    syn::ImplItem::Fn(f) => {
                        if pub_only && !is_trait_impl && !is_pub(&f.vis) {
                            continue;
                        }
                        children.push(OutlineItem {
                            kind: "fn".into(),
                            vis: vis_str(&f.vis),
                            name: f.sig.ident.to_string(),
                            signature: format_fn_sig(&f.vis, &f.sig),
                            span: span_of(f.span()),
                            children: Vec::new(),
                        });
                    }
                    syn::ImplItem::Const(c) => {
                        if pub_only && !is_trait_impl && !is_pub(&c.vis) {
                            continue;
                        }
                        children.push(OutlineItem {
                            kind: "const".into(),
                            vis: vis_str(&c.vis),
                            name: c.ident.to_string(),
                            signature: format_const_sig(&c.vis, &c.ident, &c.ty),
                            span: span_of(c.span()),
                            children: Vec::new(),
                        });
                    }
                    syn::ImplItem::Type(ty) => {
                        if pub_only && !is_trait_impl && !is_pub(&ty.vis) {
                            continue;
                        }
                        children.push(OutlineItem {
                            kind: "type".into(),
                            vis: vis_str(&ty.vis),
                            name: ty.ident.to_string(),
                            signature: format_type_sig(&ty.vis, &ty.ident, &ty.generics, &ty.ty),
                            span: span_of(ty.span()),
                            children: Vec::new(),
                        });
                    }
                    _ => {}
                }
            }
            if pub_only && !is_trait_impl && children.is_empty() {
                return None;
            }
            Some(OutlineItem {
                kind: "impl".into(),
                vis: String::new(),
                name,
                signature,
                span: span_of(imp.span()),
                children,
            })
        }
        syn::Item::Fn(f) => {
            if pub_only && !is_pub(&f.vis) {
                return None;
            }
            Some(OutlineItem {
                kind: "fn".into(),
                vis: vis_str(&f.vis),
                name: f.sig.ident.to_string(),
                signature: format_fn_sig(&f.vis, &f.sig),
                span: span_of(f.span()),
                children: Vec::new(),
            })
        }
        syn::Item::Const(c) => {
            if pub_only && !is_pub(&c.vis) {
                return None;
            }
            Some(OutlineItem {
                kind: "const".into(),
                vis: vis_str(&c.vis),
                name: c.ident.to_string(),
                signature: format_const_sig(&c.vis, &c.ident, &c.ty),
                span: span_of(c.span()),
                children: Vec::new(),
            })
        }
        syn::Item::Static(s) => {
            if pub_only && !is_pub(&s.vis) {
                return None;
            }
            Some(OutlineItem {
                kind: "static".into(),
                vis: vis_str(&s.vis),
                name: s.ident.to_string(),
                signature: format_static_sig(s),
                span: span_of(s.span()),
                children: Vec::new(),
            })
        }
        syn::Item::Type(t) => {
            if pub_only && !is_pub(&t.vis) {
                return None;
            }
            Some(OutlineItem {
                kind: "type".into(),
                vis: vis_str(&t.vis),
                name: t.ident.to_string(),
                signature: format_type_sig(&t.vis, &t.ident, &t.generics, &t.ty),
                span: span_of(t.span()),
                children: Vec::new(),
            })
        }
        syn::Item::Union(u) => {
            if pub_only && !is_pub(&u.vis) {
                return None;
            }
            let gens = format_generics_params(&u.generics);
            let wc = format_where(&u.generics.where_clause);
            let children = u
                .fields
                .named
                .iter()
                .filter(|f| !pub_only || is_pub(&f.vis))
                .map(|f| OutlineItem {
                    kind: "field".into(),
                    vis: vis_str(&f.vis),
                    name: f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default(),
                    signature: format_field(f),
                    span: span_of(f.span()),
                    children: Vec::new(),
                })
                .collect();
            Some(OutlineItem {
                kind: "union".into(),
                vis: vis_str(&u.vis),
                name: u.ident.to_string(),
                signature: format!("{}union {}{gens}{wc}", vis_prefix(&u.vis), u.ident),
                span: span_of(u.span()),
                children,
            })
        }
        syn::Item::Mod(m) => {
            if pub_only && !is_pub(&m.vis) {
                return None;
            }
            let children = match &m.content {
                Some((_, items)) => collect_items(items, pub_only),
                None => Vec::new(),
            };
            Some(OutlineItem {
                kind: "mod".into(),
                vis: vis_str(&m.vis),
                name: m.ident.to_string(),
                signature: format!("{}mod {}", vis_prefix(&m.vis), m.ident),
                span: span_of(m.span()),
                children,
            })
        }
        syn::Item::Macro(m) => {
            let ident = m.ident.as_ref()?;
            let exported = is_macro_export(&m.attrs);
            if pub_only && !exported {
                return None;
            }
            Some(OutlineItem {
                kind: "macro".into(),
                vis: if exported { "pub".into() } else { String::new() },
                name: ident.to_string(),
                signature: format!("macro_rules! {ident}"),
                span: span_of(m.span()),
                children: Vec::new(),
            })
        }
        _ => None,
    }
}

/// Extract a clean, body-free Rust declaration for the item at `span` in `src`.
///
/// Reused by `alsp prep` (Phase 3):
/// - functions/methods: full signature without `{ ... }` body
/// - structs: header + fields
/// - enums: header + variants
/// - traits: header + associated types, consts, and method signatures (without default bodies)
/// - const/static/type alias/union/mod/impl/macro: clean declaration
pub fn format_item_declaration(src: &str, span: Span) -> Option<String> {
    let parsed = syn::parse_file(src).ok()?;
    let mut best: Option<(u32, u32, String)> = None;
    find_decl_in_items(&parsed.items, span, &mut best);
    best.map(|(_, _, decl)| decl)
}

fn consider(
    best: &mut Option<(u32, u32, String)>,
    target: Span,
    item_span: Span,
    ident_line: u32,
    decl: impl FnOnce() -> String,
) {
    // Must overlap the requested span, and either the item's identifier is inside the requested
    // span or the requested span starts inside the item.
    if item_span.start > target.end || target.start > item_span.end {
        return;
    }
    if !target.contains(ident_line) && !item_span.contains(target.start) {
        return;
    }
    let dist = item_span.start.abs_diff(target.start) + item_span.end.abs_diff(target.end);
    let len = item_span.end.saturating_sub(item_span.start);
    let replace = match best {
        None => true,
        Some((best_dist, best_len, _)) => (dist, len) < (*best_dist, *best_len),
    };
    if replace {
        *best = Some((dist, len, decl()));
    }
}

fn find_decl_in_items(items: &[syn::Item], target: Span, best: &mut Option<(u32, u32, String)>) {
    for item in items {
        match item {
            syn::Item::Fn(f) => {
                consider(best, target, span_of(f.span()), f.sig.ident.span().start().line as u32, || {
                    format_fn_sig(&f.vis, &f.sig)
                });
            }
            syn::Item::Struct(s) => {
                consider(best, target, span_of(s.span()), s.ident.span().start().line as u32, || {
                    format_struct_decl(s)
                });
            }
            syn::Item::Enum(e) => {
                consider(best, target, span_of(e.span()), e.ident.span().start().line as u32, || {
                    format_enum_decl(e)
                });
            }
            syn::Item::Trait(t) => {
                consider(best, target, span_of(t.span()), t.ident.span().start().line as u32, || {
                    format_trait_decl(t)
                });
                for ti in &t.items {
                    match ti {
                        syn::TraitItem::Fn(f) => {
                            consider(best, target, span_of(f.span()), f.sig.ident.span().start().line as u32, || {
                                format_fn_sig(&syn::Visibility::Inherited, &f.sig)
                            });
                        }
                        syn::TraitItem::Type(ty) => {
                            consider(best, target, span_of(ty.span()), ty.ident.span().start().line as u32, || {
                                format_trait_type(ty)
                            });
                        }
                        syn::TraitItem::Const(c) => {
                            consider(best, target, span_of(c.span()), c.ident.span().start().line as u32, || {
                                format_trait_const(c)
                            });
                        }
                        _ => {}
                    }
                }
            }
            syn::Item::Impl(imp) => {
                let imp_span = span_of(imp.span());
                consider(best, target, imp_span, imp.impl_token.span.start().line as u32, || {
                    format_impl_decl(imp)
                });
                for ii in &imp.items {
                    match ii {
                        syn::ImplItem::Fn(f) => {
                            consider(best, target, span_of(f.span()), f.sig.ident.span().start().line as u32, || {
                                format_fn_sig(&f.vis, &f.sig)
                            });
                        }
                        syn::ImplItem::Const(c) => {
                            consider(best, target, span_of(c.span()), c.ident.span().start().line as u32, || {
                                format_const_sig(&c.vis, &c.ident, &c.ty)
                            });
                        }
                        syn::ImplItem::Type(ty) => {
                            consider(best, target, span_of(ty.span()), ty.ident.span().start().line as u32, || {
                                format_type_sig(&ty.vis, &ty.ident, &ty.generics, &ty.ty)
                            });
                        }
                        _ => {}
                    }
                }
            }
            syn::Item::Const(c) => {
                consider(best, target, span_of(c.span()), c.ident.span().start().line as u32, || {
                    format_const_sig(&c.vis, &c.ident, &c.ty)
                });
            }
            syn::Item::Static(s) => {
                consider(best, target, span_of(s.span()), s.ident.span().start().line as u32, || {
                    format_static_sig(s)
                });
            }
            syn::Item::Type(t) => {
                consider(best, target, span_of(t.span()), t.ident.span().start().line as u32, || {
                    format_type_sig(&t.vis, &t.ident, &t.generics, &t.ty)
                });
            }
            syn::Item::Union(u) => {
                consider(best, target, span_of(u.span()), u.ident.span().start().line as u32, || {
                    format_union_decl(u)
                });
            }
            syn::Item::Mod(m) => {
                consider(best, target, span_of(m.span()), m.ident.span().start().line as u32, || {
                    format!("{}mod {}", vis_prefix(&m.vis), m.ident)
                });
                if let Some((_, sub)) = &m.content {
                    find_decl_in_items(sub, target, best);
                }
            }
            syn::Item::Macro(m) => {
                if let Some(ident) = &m.ident {
                    consider(best, target, span_of(m.span()), ident.span().start().line as u32, || {
                        format!("macro_rules! {ident}")
                    });
                }
            }
            _ => {}
        }
    }
}

fn format_struct_decl(s: &syn::ItemStruct) -> String {
    let head = format_struct_sig(s);
    match &s.fields {
        syn::Fields::Named(named) => {
            if named.named.is_empty() {
                format!("{head} {{}}")
            } else {
                let mut lines = vec![format!("{head} {{")];
                for f in &named.named {
                    lines.push(format!("    {},", format_field(f)));
                }
                lines.push("}".into());
                lines.join("\n")
            }
        }
        syn::Fields::Unnamed(_) | syn::Fields::Unit => format!("{head};"),
    }
}

fn format_enum_decl(e: &syn::ItemEnum) -> String {
    let head = format_enum_sig(e);
    if e.variants.is_empty() {
        return format!("{head} {{}}");
    }
    let mut lines = vec![format!("{head} {{")];
    for v in &e.variants {
        lines.push(format!("    {},", format_variant(v)));
    }
    lines.push("}".into());
    lines.join("\n")
}

fn format_trait_decl(t: &syn::ItemTrait) -> String {
    let head = format_trait_sig(t);
    if t.items.is_empty() {
        return format!("{head} {{}}");
    }
    let mut lines = vec![format!("{head} {{")];
    for ti in &t.items {
        match ti {
            syn::TraitItem::Fn(f) => {
                lines.push(format!("    {};", format_fn_sig(&syn::Visibility::Inherited, &f.sig)));
            }
            syn::TraitItem::Type(ty) => lines.push(format!("    {};", format_trait_type(ty))),
            syn::TraitItem::Const(c) => lines.push(format!("    {};", format_trait_const(c))),
            _ => {}
        }
    }
    lines.push("}".into());
    lines.join("\n")
}

fn format_impl_decl(imp: &syn::ItemImpl) -> String {
    let (_, head) = format_impl_header(imp);
    if imp.items.is_empty() {
        return format!("{head} {{}}");
    }
    let mut lines = vec![format!("{head} {{")];
    for ii in &imp.items {
        match ii {
            syn::ImplItem::Fn(f) => lines.push(format!("    {};", format_fn_sig(&f.vis, &f.sig))),
            syn::ImplItem::Const(c) => {
                lines.push(format!("    {};", format_const_sig(&c.vis, &c.ident, &c.ty)))
            }
            syn::ImplItem::Type(ty) => lines.push(format!(
                "    {};",
                format_type_sig(&ty.vis, &ty.ident, &ty.generics, &ty.ty)
            )),
            _ => {}
        }
    }
    lines.push("}".into());
    lines.join("\n")
}

fn format_union_decl(u: &syn::ItemUnion) -> String {
    let gens = format_generics_params(&u.generics);
    let wc = format_where(&u.generics.where_clause);
    let head = format!("{}union {}{gens}{wc}", vis_prefix(&u.vis), u.ident);
    let mut lines = vec![format!("{head} {{")];
    for f in &u.fields.named {
        lines.push(format!("    {},", format_field(f)));
    }
    lines.push("}".into());
    lines.join("\n")
}

/// Outline one or more local files or directories under `root` using `syn` only.
///
/// Security invariants:
/// - `root` is canonicalized once.
/// - Every path and directory entry is inspected with `symlink_metadata`.
/// - Symlinks or paths whose canonical target escapes `root` are refused with a note and never read.
/// - Directory symlink loops are detected via a canonical visited-dir set and skipped with a note.
/// - `target/` and `.git/` directories are skipped.
/// - Syntax errors or unreadable files record a note without aborting other files.
pub fn outline_paths(root: &Path, paths: &[PathBuf], pub_only: bool) -> Result<OutlineAnswer, String> {
    let canon_root = std::fs::canonicalize(root)
        .map_err(|e| format!("cannot resolve root {}: {e}", escape(&root.display().to_string())))?;
    let mut files = Vec::new();
    let mut notes = Vec::new();
    let mut visited_dirs: HashSet<PathBuf> = HashSet::new();
    let mut visited_files: HashSet<PathBuf> = HashSet::new();

    for req in paths {
        let full = if req.is_relative() { canon_root.join(req) } else { req.clone() };
        let disp = display_rel(&canon_root, &full);
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(e) => {
                return Err(format!("cannot access {}: {e}", escape(&disp)));
            }
        };
        let is_sym = meta.file_type().is_symlink();
        let canon = match std::fs::canonicalize(&full) {
            Ok(c) => c,
            Err(e) => {
                if is_sym {
                    notes.push(format!("skipped symlink {disp}: {e}"));
                    continue;
                }
                return Err(format!("cannot resolve {}: {e}", escape(&disp)));
            }
        };
        if !canon.starts_with(&canon_root) {
            let kind = if is_sym { "symlink " } else { "" };
            notes.push(format!("skipped {kind}{disp}: target escapes root"));
            continue;
        }
        if canon.is_dir() {
            if !visited_dirs.insert(canon.clone()) {
                if is_sym {
                    notes.push(format!("skipped symlink loop {disp}"));
                }
                continue;
            }
            walk_dir(&canon_root, &canon, pub_only, &mut visited_dirs, &mut visited_files, &mut files, &mut notes);
        } else if canon.is_file() {
            outline_one_file(&canon_root, &canon, pub_only, &mut visited_files, &mut files, &mut notes);
        }
    }

    Ok(OutlineAnswer {
        schema: crate::SCHEMA,
        command: "outline".into(),
        provenance: Provenance::Resolved,
        files,
        notes,
    })
}

fn walk_dir(
    canon_root: &Path,
    dir: &Path,
    pub_only: bool,
    visited_dirs: &mut HashSet<PathBuf>,
    visited_files: &mut HashSet<PathBuf>,
    files: &mut Vec<FileOutline>,
    notes: &mut Vec<String>,
) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            notes.push(format!("cannot read directory {}: {e}", display_rel(canon_root, dir)));
            return;
        }
    };
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in rd {
        match entry {
            Ok(e) => entries.push(e.path()),
            Err(e) => notes.push(format!("directory entry error in {}: {e}", display_rel(canon_root, dir))),
        }
    }
    entries.sort();

    for path in entries {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name == "target" || name == ".git" {
                continue;
            }
        }
        let disp = display_rel(canon_root, &path);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                notes.push(format!("cannot stat {disp}: {e}"));
                continue;
            }
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            let target = match std::fs::canonicalize(&path) {
                Ok(t) => t,
                Err(e) => {
                    notes.push(format!("skipped symlink {disp}: {e}"));
                    continue;
                }
            };
            if !target.starts_with(canon_root) {
                notes.push(format!("skipped symlink {disp}: target escapes root"));
                continue;
            }
            if target.is_dir() {
                if !visited_dirs.insert(target.clone()) {
                    notes.push(format!("skipped symlink loop {disp}"));
                    continue;
                }
                walk_dir(canon_root, &target, pub_only, visited_dirs, visited_files, files, notes);
            } else if target.is_file() && is_rs(&path) {
                outline_one_file(canon_root, &target, pub_only, visited_files, files, notes);
            }
        } else if ft.is_dir() {
            let canon = match std::fs::canonicalize(&path) {
                Ok(c) => c,
                Err(e) => {
                    notes.push(format!("cannot resolve {disp}: {e}"));
                    continue;
                }
            };
            if !canon.starts_with(canon_root) {
                notes.push(format!("skipped {disp}: target escapes root"));
                continue;
            }
            if !visited_dirs.insert(canon.clone()) {
                continue;
            }
            walk_dir(canon_root, &canon, pub_only, visited_dirs, visited_files, files, notes);
        } else if ft.is_file() && is_rs(&path) {
            outline_one_file(canon_root, &path, pub_only, visited_files, files, notes);
        }
    }
}

fn outline_one_file(
    canon_root: &Path,
    path: &Path,
    pub_only: bool,
    visited_files: &mut HashSet<PathBuf>,
    files: &mut Vec<FileOutline>,
    notes: &mut Vec<String>,
) {
    if !visited_files.insert(path.to_path_buf()) {
        return;
    }
    let rel = display_rel(canon_root, path);
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            notes.push(format!("cannot read {rel}: {e}"));
            return;
        }
    };
    match outline_source(&rel, &src, pub_only) {
        Ok(fo) => files.push(fo),
        Err(e) => notes.push(e),
    }
}

fn is_rs(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()) == Some("rs")
}

fn display_rel(canon_root: &Path, p: &Path) -> String {
    let rel = p.strip_prefix(canon_root).unwrap_or(p);
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() { ".".into() } else { s }
}

/// Locate a dependency crate's canonical root directory via `cargo metadata --format-version 1`.
///
/// Matches exact package name first, then `-`/`_` normalized name. If not found, returns a
/// helpful error listing close or available package names.
pub fn resolve_dep_root(repo: &Path, dep_name: &str, cargo: &Path) -> Result<PathBuf, String> {
    let canon_repo = std::fs::canonicalize(repo)
        .map_err(|e| format!("cannot resolve repo {}: {e}", escape(&repo.display().to_string())))?;
    let mut cmd = Command::new(cargo);
    cmd.args(["metadata", "--format-version", "1"]).current_dir(&canon_repo);
    if canon_repo.join("Cargo.toml").is_file() {
        cmd.args(["--manifest-path", "Cargo.toml"]);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run {}: {e}", escape(&cargo.display().to_string())))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.trim();
        return Err(if detail.is_empty() {
            format!("cargo metadata failed with {}", out.status)
        } else {
            format!("cargo metadata failed: {}", escape(detail))
        });
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_dep_root(&stdout, dep_name)
}

fn parse_dep_root(json: &str, dep_name: &str) -> Result<PathBuf, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("cargo metadata output is not JSON: {e}"))?;
    let pkgs = v
        .get("packages")
        .and_then(|p| p.as_array())
        .ok_or_else(|| "cargo metadata has no packages array".to_string())?;

    let norm = |s: &str| s.replace('-', "_");
    let dep_norm = norm(dep_name);
    let mut exact: Option<PathBuf> = None;
    let mut fuzzy: Option<PathBuf> = None;
    let mut names: Vec<String> = Vec::new();

    for pkg in pkgs {
        let Some(name) = pkg.get("name").and_then(|n| n.as_str()) else { continue };
        names.push(name.to_string());
        let Some(manifest) = pkg.get("manifest_path").and_then(|m| m.as_str()) else { continue };
        let Some(root) = Path::new(manifest).parent() else { continue };
        if name == dep_name && exact.is_none() {
            exact = Some(root.to_path_buf());
        } else if norm(name) == dep_norm && fuzzy.is_none() {
            fuzzy = Some(root.to_path_buf());
        }
    }

    if let Some(root) = exact.or(fuzzy) {
        return std::fs::canonicalize(&root).map_err(|e| {
            format!(
                "dependency `{}` root {} is not accessible: {e}",
                escape(dep_name),
                escape(&root.display().to_string())
            )
        });
    }

    names.sort();
    names.dedup();
    let needle = dep_norm.to_lowercase();
    let close: Vec<String> = names
        .iter()
        .filter(|n| {
            let nl = norm(n).to_lowercase();
            nl.contains(&needle) || needle.contains(&nl)
        })
        .take(10)
        .map(|n| escape(n).into_owned())
        .collect();
    if !close.is_empty() {
        Err(format!(
            "dependency `{}` not found in cargo metadata; close matches: {}",
            escape(dep_name),
            close.join(", ")
        ))
    } else if !names.is_empty() {
        let shown: Vec<String> = names.iter().take(15).map(|n| escape(n).into_owned()).collect();
        let more = if names.len() > 15 { format!(" (+{} more)", names.len() - 15) } else { String::new() };
        Err(format!(
            "dependency `{}` not found in cargo metadata; available packages: {}{more}",
            escape(dep_name),
            shown.join(", ")
        ))
    } else {
        Err(format!("dependency `{}` not found in cargo metadata (no packages listed)", escape(dep_name)))
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use std::fs;

    const SAMPLE: &str = r#"use std::fmt;

/// A widget.
pub struct Widget<T: Clone> {
    pub id: u32,
    secret: T,
}

struct PrivateStruct(u8, String);

pub enum Status {
    Ok,
    Err(String),
    Custom { code: u16, msg: String },
}

pub trait Runner: Send {
    type Output;
    const LIMIT: usize;
    fn run(&mut self, input: &str) -> Result<Self::Output, String>;
    fn default_ok(&self) -> bool {
        true
    }
}

impl<T: Clone> Widget<T> {
    pub fn new(id: u32, secret: T) -> Self {
        Self { id, secret }
    }
    fn helper(&self) -> u32 {
        self.id
    }
}

impl<T: Clone> fmt::Display for Widget<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.id)
    }
}

impl PrivateStruct {
    fn internal(&self) {}
}

pub async fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let _ = url;
    Ok(Vec::new())
}

fn private_fn() {}

pub const MAX_ITEMS: usize = 64;
const PRIV_CONST: u8 = 1;
pub static mut COUNTER: usize = 0;
pub type BoxErr = Box<dyn std::error::Error>;
"#;

    #[test]
    fn test_DW_2_2_outline_extracts_all_items_and_spans() {
        let fo = outline_source("src/lib.rs", SAMPLE, false).unwrap();
        assert_eq!(fo.file, "src/lib.rs");
        assert_eq!(fo.lines, SAMPLE.lines().count());

        let names: Vec<(&str, &str)> = fo.items.iter().map(|i| (i.kind.as_str(), i.name.as_str())).collect();
        assert!(names.contains(&("struct", "Widget")), "{names:?}");
        assert!(names.contains(&("struct", "PrivateStruct")), "{names:?}");
        assert!(names.contains(&("enum", "Status")), "{names:?}");
        assert!(names.contains(&("trait", "Runner")), "{names:?}");
        assert!(names.contains(&("impl", "Widget<T>")), "{names:?}");
        assert!(names.contains(&("impl", "fmt::Display for Widget<T>")), "{names:?}");
        assert!(names.contains(&("fn", "fetch")), "{names:?}");
        assert!(names.contains(&("fn", "private_fn")), "{names:?}");
        assert!(names.contains(&("const", "MAX_ITEMS")), "{names:?}");
        assert!(names.contains(&("const", "PRIV_CONST")), "{names:?}");
        assert!(names.contains(&("static", "COUNTER")), "{names:?}");
        assert!(names.contains(&("type", "BoxErr")), "{names:?}");

        let widget = fo.items.iter().find(|i| i.name == "Widget").unwrap();
        assert_eq!(widget.vis, "pub");
        assert_eq!(widget.children.len(), 2);
        assert_eq!(widget.children[0].signature, "pub id: u32");
        assert_eq!(widget.children[1].signature, "secret: T");
        assert!(widget.span.start >= 3 && widget.span.end >= 7);

        let status = fo.items.iter().find(|i| i.name == "Status").unwrap();
        assert_eq!(status.children.len(), 3);
        assert_eq!(status.children[0].signature, "Ok");
        assert_eq!(status.children[1].signature, "Err(String)");
        assert_eq!(status.children[2].signature, "Custom { code: u16, msg: String }");

        let runner = fo.items.iter().find(|i| i.name == "Runner").unwrap();
        assert_eq!(runner.children.len(), 4);
        assert_eq!(runner.children[0].signature, "type Output");
        assert_eq!(runner.children[1].signature, "const LIMIT: usize");
        assert_eq!(runner.children[2].signature, "fn run(&mut self, input: &str) -> Result<Self::Output, String>");
        assert_eq!(runner.children[3].signature, "fn default_ok(&self) -> bool");

        let fetch = fo.items.iter().find(|i| i.name == "fetch").unwrap();
        assert_eq!(fetch.signature, "pub async fn fetch(url: &str) -> Result<Vec<u8>, String>");
    }

    #[test]
    fn test_DW_2_2_pub_only_hides_private_items_and_keeps_trait_impls() {
        let fo = outline_source("src/lib.rs", SAMPLE, true).unwrap();
        let names: Vec<(&str, &str)> = fo.items.iter().map(|i| (i.kind.as_str(), i.name.as_str())).collect();
        assert!(names.contains(&("struct", "Widget")));
        assert!(!names.contains(&("struct", "PrivateStruct")));
        assert!(!names.contains(&("fn", "private_fn")));
        assert!(!names.contains(&("const", "PRIV_CONST")));
        // Private-only inherent impl is omitted.
        assert!(!names.contains(&("impl", "PrivateStruct")));
        // Inherent impl on Widget keeps only `pub fn new`, hides `fn helper`.
        let imp = fo.items.iter().find(|i| i.kind == "impl" && i.name == "Widget<T>").unwrap();
        assert_eq!(imp.children.len(), 1);
        assert_eq!(imp.children[0].name, "new");
        // Trait impl is kept along with its methods.
        let disp = fo.items.iter().find(|i| i.kind == "impl" && i.name == "fmt::Display for Widget<T>").unwrap();
        assert_eq!(disp.children.len(), 1);
        assert_eq!(disp.children[0].name, "fmt");
        // Widget struct under --pub keeps pub field `id`, hides private field `secret`.
        let widget = fo.items.iter().find(|i| i.name == "Widget").unwrap();
        assert_eq!(widget.children.len(), 1);
        assert_eq!(widget.children[0].name, "id");
    }

    #[test]
    fn test_DW_2_4_format_item_declaration_extracts_clean_declarations() {
        let fo = outline_source("src/lib.rs", SAMPLE, false).unwrap();

        let fetch = fo.items.iter().find(|i| i.name == "fetch").unwrap();
        let fn_decl = format_item_declaration(SAMPLE, fetch.span).unwrap();
        assert_eq!(fn_decl, "pub async fn fetch(url: &str) -> Result<Vec<u8>, String>");
        assert!(!fn_decl.contains("Ok(Vec::new())"), "body must be omitted: {fn_decl}");

        let widget = fo.items.iter().find(|i| i.name == "Widget").unwrap();
        let struct_decl = format_item_declaration(SAMPLE, widget.span).unwrap();
        assert!(struct_decl.contains("pub struct Widget<T: Clone> {") && struct_decl.contains("pub id: u32,") && struct_decl.contains("secret: T,"), "{struct_decl}");

        let status = fo.items.iter().find(|i| i.name == "Status").unwrap();
        let enum_decl = format_item_declaration(SAMPLE, status.span).unwrap();
        assert!(enum_decl.contains("pub enum Status {") && enum_decl.contains("Ok,") && enum_decl.contains("Err(String),"), "{enum_decl}");

        let runner = fo.items.iter().find(|i| i.name == "Runner").unwrap();
        let trait_decl = format_item_declaration(SAMPLE, runner.span).unwrap();
        assert!(trait_decl.contains("pub trait Runner: Send {") && trait_decl.contains("fn run(&mut self, input: &str) -> Result<Self::Output, String>;") && !trait_decl.contains("true"), "{trait_decl}");

        // Impl method inside impl block
        let imp = fo.items.iter().find(|i| i.kind == "impl" && i.name == "Widget<T>").unwrap();
        let new_m = &imp.children[0];
        let method_decl = format_item_declaration(SAMPLE, new_m.span).unwrap();
        assert_eq!(method_decl, "pub fn new(id: u32, secret: T) -> Self");

        // Const, static, type alias
        let c = fo.items.iter().find(|i| i.name == "MAX_ITEMS").unwrap();
        assert_eq!(format_item_declaration(SAMPLE, c.span).unwrap(), "pub const MAX_ITEMS: usize");
        let st = fo.items.iter().find(|i| i.name == "COUNTER").unwrap();
        assert_eq!(format_item_declaration(SAMPLE, st.span).unwrap(), "pub static mut COUNTER: usize");
        let ty = fo.items.iter().find(|i| i.name == "BoxErr").unwrap();
        assert_eq!(format_item_declaration(SAMPLE, ty.span).unwrap(), "pub type BoxErr = Box<dyn std::error::Error>");
    }

    #[test]
    fn test_DW_2_5_symlink_escape_loop_syntax_error_and_control_chars() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.rs");
        fs::write(&secret, "pub fn TOP_SECRET_DO_NOT_LEAK() {}\n").unwrap();

        let repo = tempfile::tempdir().unwrap();
        let src = repo.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(repo.path().join("target")).unwrap();
        fs::write(repo.path().join("target/gen.rs"), "pub fn ignored_target() {}\n").unwrap();
        fs::write(src.join("good.rs"), "pub fn ok() {}\n").unwrap();
        fs::write(src.join("broken.rs"), "pub fn broken( { }\n").unwrap();
        // Symlink file escaping repo root
        symlink(&secret, src.join("escape.rs")).unwrap();
        // Symlink dir escaping repo root
        symlink(outside.path(), src.join("escape_dir")).unwrap();
        // Symlink dir loop inside repo
        symlink(&src, src.join("loop_back")).unwrap();
        // Control character in filename
        fs::write(src.join("ctrl\x1bname.rs"), "pub fn ctrl_fn() {}\n").unwrap();

        let ans = outline_paths(repo.path(), &[PathBuf::from(".")], false).unwrap();
        let text = ans.to_text();
        let json = ans.to_json();

        // Never leaks external file content
        assert!(!text.contains("TOP_SECRET_DO_NOT_LEAK"), "{text}");
        assert!(!json.contains("TOP_SECRET_DO_NOT_LEAK"), "{json}");
        // target/ skipped
        assert!(!text.contains("ignored_target"), "{text}");
        // Good file parsed despite broken file and hostile symlinks
        assert!(ans.files.iter().any(|f| f.file == "src/good.rs"), "{:?}", ans.files);
        // Notes record symlink escapes, symlink loop, and syntax error
        assert!(ans.notes.iter().any(|n| n.contains("escape.rs") && n.contains("escapes root")), "{:?}", ans.notes);
        assert!(ans.notes.iter().any(|n| n.contains("escape_dir") && n.contains("escapes root")), "{:?}", ans.notes);
        assert!(ans.notes.iter().any(|n| n.contains("loop_back") && n.contains("loop")), "{:?}", ans.notes);
        assert!(ans.notes.iter().any(|n| n.contains("broken.rs") && n.contains("syntax error")), "{:?}", ans.notes);
        // Control char escaped in text output (no raw ESC 0x1b)
        assert!(!text.contains('\x1b'), "raw ESC leaked: {text:?}");
        assert!(text.contains("\\u{1b}"), "ESC should be escaped: {text}");
        // JSON is valid single-line JSON
        assert_eq!(json.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema"], 1);
        assert_eq!(parsed["command"], "outline");
        assert_eq!(parsed["provenance"], "resolved");
    }

    #[test]
    fn test_DW_2_5_direct_symlink_escape_path_is_refused() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.rs");
        fs::write(&secret, "pub fn LEAKED() {}\n").unwrap();

        let repo = tempfile::tempdir().unwrap();
        symlink(&secret, repo.path().join("link.rs")).unwrap();

        let ans = outline_paths(repo.path(), &[PathBuf::from("link.rs")], false).unwrap();
        assert!(ans.files.is_empty());
        assert!(ans.notes.iter().any(|n| n.contains("escapes root")), "{:?}", ans.notes);
    }

    #[test]
    fn test_DW_2_3_parse_dep_root_exact_normalized_and_not_found() {
        let dep_dir = tempfile::tempdir().unwrap();
        let canon = fs::canonicalize(dep_dir.path()).unwrap();
        let meta = serde_json::json!({
            "packages": [
                { "name": "proc-macro2", "manifest_path": format!("{}/Cargo.toml", canon.display()) },
                { "name": "serde", "manifest_path": format!("{}/Cargo.toml", canon.display()) }
            ]
        })
        .to_string();

        assert_eq!(parse_dep_root(&meta, "proc-macro2").unwrap(), canon);
        assert_eq!(parse_dep_root(&meta, "proc_macro2").unwrap(), canon);
        let err = parse_dep_root(&meta, "proc-macro").unwrap_err();
        assert!(err.contains("not found") && err.contains("proc-macro2"), "{err}");
        let err2 = parse_dep_root(&meta, "zzz_missing").unwrap_err();
        assert!(err2.contains("not found") && err2.contains("serde"), "{err2}");
    }

    #[test]
    fn test_DW_2_4_format_item_declaration_matches_span_without_doc_comments_and_nested_mod() {
        let src = "/// doc\n#[derive(Debug)]\npub struct S {\n    pub x: u32,\n}\nmod inner {\n    pub fn nested(a: *const u8) -> (bool, u8) {\n        (a.is_null(), 0)\n    }\n}\n";
        // Span starting at `pub struct S` (line 3..=5), omitting doc/attr lines 1..=2.
        let s_decl = format_item_declaration(src, Span { start: 3, end: 5 }).unwrap();
        assert_eq!(s_decl, "pub struct S {\n    pub x: u32,\n}");
        // Nested fn inside inline mod.
        let f_decl = format_item_declaration(src, Span { start: 7, end: 9 }).unwrap();
        assert_eq!(f_decl, "pub fn nested(a: *const u8) -> (bool, u8)");
        // Unparsable source returns None.
        assert_eq!(format_item_declaration("fn broken( {", Span { start: 1, end: 1 }), None);
    }

    #[test]
    fn test_DW_2_5_parent_traversal_dangling_and_self_loop_symlinks() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("outside.rs"), "pub fn OUTSIDE_SECRET() {}\n").unwrap();

        let repo = tempfile::tempdir().unwrap();
        let src = repo.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("ok.rs"), "pub(crate) fn c() {}\n").unwrap();
        // Dangling symlink
        symlink("/nonexistent/does_not_exist.rs", src.join("dangling.rs")).unwrap();
        // Self-referential symlink loop
        symlink(src.join("self_loop.rs"), src.join("self_loop.rs")).unwrap();
        // Newline in filename (cannot forge output lines)
        fs::write(src.join("bad\nforged.rs"), "pub fn f() {}\n").unwrap();

        let ans = outline_paths(
            repo.path(),
            &[PathBuf::from("src"), outside.path().join("outside.rs")],
            false,
        )
        .unwrap();
        let text = ans.to_text();
        assert!(!text.contains("OUTSIDE_SECRET"), "{text}");
        assert!(ans.notes.iter().any(|n| n.contains("dangling.rs")), "{:?}", ans.notes);
        assert!(ans.notes.iter().any(|n| n.contains("self_loop.rs")), "{:?}", ans.notes);
        assert!(ans.notes.iter().any(|n| n.contains("escapes root")), "{:?}", ans.notes);
        assert!(text.contains("bad\\nforged.rs"), "newline in filename must be escaped: {text}");
        assert!(!text.lines().any(|l| l.starts_with("forged.rs")), "forged line detected: {text}");
    }

    #[test]
    fn test_utf8_multibyte_char_before_open_paren_does_not_panic() {
        let src = r#"extern "🦀 (a)" fn f() {}
enum E {
    A = { "— (x)".len() as isize },
}
"#;
        let fo = outline_source("src/utf8.rs", src, false).unwrap();
        assert_eq!(fo.items.len(), 2);
        assert_eq!(fo.items[0].name, "f");
        assert_eq!(fo.items[1].name, "E");
        assert_eq!(last_word("🦀"), "");
        assert_eq!(last_word("—"), "");
        assert_eq!(last_word("—foo"), "foo");
    }

    #[test]
    fn test_resolve_dep_root_works_with_relative_repo_path() {
        use std::os::unix::fs::PermissionsExt;

        let dep = tempfile::tempdir().unwrap();
        let dep_canon = fs::canonicalize(dep.path()).unwrap();
        let tools = tempfile::tempdir().unwrap();
        let meta_path = tools.path().join("meta.json");
        fs::write(
            &meta_path,
            serde_json::json!({
                "packages": [
                    { "name": "rel-dep", "manifest_path": format!("{}/Cargo.toml", dep_canon.display()) }
                ]
            })
            .to_string(),
        )
        .unwrap();
        // Stub cargo verifies that any --manifest-path argument resolves to an existing file in cwd.
        let stub = tools.path().join("cargo");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = \"--manifest-path\" ]; then\n    shift\n    if [ ! -f \"$1\" ]; then echo \"missing manifest: $1\" >&2; exit 1; fi\n  fi\n  shift\ndone\ncat \"{}\"\n",
                meta_path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();

        // `cargo test -p alsp-query` runs with cwd = crates/alsp-query, so `../alsp-cli` is a real
        // non-`.` relative path containing Cargo.toml.
        let rel_repo = Path::new("../alsp-cli");
        assert!(rel_repo.join("Cargo.toml").is_file(), "expected ../alsp-cli/Cargo.toml from cwd");
        let resolved = resolve_dep_root(rel_repo, "rel-dep", &stub).unwrap();
        assert_eq!(resolved, dep_canon);
    }

    #[test]
    fn test_syntax_error_note_escapes_control_chars_once_not_twice() {
        let repo = tempfile::tempdir().unwrap();
        let src = repo.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("err\nfile.rs"), "pub fn broken( {\n").unwrap();

        let ans = outline_paths(repo.path(), &[PathBuf::from("src")], false).unwrap();
        let text = ans.to_text();
        assert!(text.contains("err\\nfile.rs"), "should be escaped once: {text}");
        assert!(!text.contains("err\\\\nfile.rs"), "must not be double-escaped: {text}");
    }
}
