//! What the index cannot say, read from source syntax with `syn`: attributes (`#[test]`),
//! visibility, `use` declarations, and a stable identity for each item so that the same item can be
//! recognised in the base and head versions of a file.

use proc_macro2::Span as PSpan;
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Fn,
    Mod,
    Type,
    Const,
    Macro,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub kind: ItemKind,
    pub name: String,
    /// Path inside the file: modules, impl owner, name. Equal in base and head for "the same item".
    pub path: String,
    /// `kind` plus `path`: unique enough to match items across versions of a file.
    pub key: String,
    pub name_line: u32,
    /// First and last line of the item, attributes and doc comments included.
    pub start: u32,
    pub end: u32,
    pub is_test: bool,
    /// Declared `pub` (plain `pub`, not `pub(crate)`).
    pub is_pub: bool,
    /// Defined directly in the file, outside any module, impl or trait.
    pub top_level: bool,
}

#[derive(Clone, Debug, Default)]
pub struct FileSyntax {
    pub items: Vec<Item>,
    /// Line ranges of `use` declarations: names mentioned there are imports, not uses.
    pub use_ranges: Vec<(u32, u32)>,
}

impl FileSyntax {
    pub fn item_at_name_line(&self, line: u32) -> Option<&Item> {
        self.items.iter().find(|i| i.name_line == line)
    }
    pub fn in_use(&self, line: u32) -> bool {
        self.use_ranges.iter().any(|&(a, b)| a <= line && line <= b)
    }
    pub fn has_key(&self, key: &str) -> bool {
        self.items.iter().any(|i| i.key == key)
    }
}

/// Parse a file. `None` when it does not parse (the caller then falls back to index-only facts).
pub fn analyze(src: &str) -> Option<FileSyntax> {
    let file = syn::parse_file(src).ok()?;
    let mut v = Collector { out: FileSyntax::default(), scope: Vec::new(), owner: None };
    v.visit_file(&file);
    Some(v.out)
}

fn lines(s: PSpan) -> (u32, u32) {
    (s.start().line as u32, s.end().line as u32)
}

fn is_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| a.path().segments.last().is_some_and(|s| s.ident == "test" || s.ident == "bench"))
}

fn is_pub(v: &syn::Visibility) -> bool {
    matches!(v, syn::Visibility::Public(_))
}

fn squash(t: impl ToTokens) -> String {
    // Token streams print `a :: b < T >`; undo the spaces around path and generic punctuation.
    t.to_token_stream().to_string().replace(" :: ", "::").replace(" < ", "<").replace(" >", ">").replace(" , ", ", ").replace("& ", "&")
}

struct Collector {
    out: FileSyntax,
    /// Modules and trait names above the current item.
    scope: Vec<String>,
    /// Header of the enclosing impl, e.g. `Frame` or `Display for Frame`.
    owner: Option<String>,
}

impl Collector {
    fn path(&self, name: &str) -> String {
        let mut parts = self.scope.clone();
        if let Some(o) = &self.owner {
            parts.push(format!("<{o}>"));
        }
        parts.push(name.to_string());
        parts.join("::")
    }

    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, kind: ItemKind, name: &str, ident: PSpan, whole: PSpan, attrs: &[syn::Attribute], vis: Option<&syn::Visibility>) {
        let path = self.path(name);
        let (start, end) = lines(whole);
        self.out.items.push(Item {
            kind,
            name: name.to_string(),
            key: format!("{kind:?} {path}"),
            path,
            name_line: ident.start().line as u32,
            start,
            end,
            is_test: kind == ItemKind::Fn && is_test_attr(attrs),
            is_pub: vis.is_some_and(is_pub),
            top_level: self.scope.is_empty() && self.owner.is_none(),
        });
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.push(ItemKind::Fn, &i.sig.ident.to_string(), i.sig.ident.span(), i.span(), &i.attrs, Some(&i.vis));
        // Items nested in a function body belong to its scope.
        self.scope.push(i.sig.ident.to_string());
        let owner = self.owner.take();
        visit::visit_block(self, &i.block);
        self.owner = owner;
        self.scope.pop();
    }

    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.push(ItemKind::Mod, &i.ident.to_string(), i.ident.span(), i.span(), &i.attrs, Some(&i.vis));
        if let Some((_, items)) = &i.content {
            self.scope.push(i.ident.to_string());
            let owner = self.owner.take();
            for it in items {
                self.visit_item(it);
            }
            self.owner = owner;
            self.scope.pop();
        }
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let header = match &i.trait_ {
            Some((_, tr, _)) => format!("{} for {}", squash(tr), squash(&i.self_ty)),
            None => squash(&i.self_ty),
        };
        let outer = self.owner.replace(header);
        for it in &i.items {
            self.visit_impl_item(it);
        }
        self.owner = outer;
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.push(ItemKind::Fn, &i.sig.ident.to_string(), i.sig.ident.span(), i.span(), &i.attrs, Some(&i.vis));
    }

    fn visit_impl_item_const(&mut self, i: &'ast syn::ImplItemConst) {
        self.push(ItemKind::Const, &i.ident.to_string(), i.ident.span(), i.span(), &i.attrs, Some(&i.vis));
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.push(ItemKind::Type, &i.ident.to_string(), i.ident.span(), i.span(), &i.attrs, Some(&i.vis));
        self.scope.push(i.ident.to_string());
        for it in &i.items {
            match it {
                syn::TraitItem::Fn(f) => self.push(ItemKind::Fn, &f.sig.ident.to_string(), f.sig.ident.span(), f.span(), &f.attrs, None),
                syn::TraitItem::Const(c) => self.push(ItemKind::Const, &c.ident.to_string(), c.ident.span(), c.span(), &c.attrs, None),
                _ => {}
            }
        }
        self.scope.pop();
    }

    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.push(ItemKind::Type, &i.ident.to_string(), i.ident.span(), i.span(), &i.attrs, Some(&i.vis));
    }

    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.push(ItemKind::Type, &i.ident.to_string(), i.ident.span(), i.span(), &i.attrs, Some(&i.vis));
    }

    fn visit_item_union(&mut self, i: &'ast syn::ItemUnion) {
        self.push(ItemKind::Type, &i.ident.to_string(), i.ident.span(), i.span(), &i.attrs, Some(&i.vis));
    }

    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        self.push(ItemKind::Type, &i.ident.to_string(), i.ident.span(), i.span(), &i.attrs, Some(&i.vis));
    }

    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.push(ItemKind::Const, &i.ident.to_string(), i.ident.span(), i.span(), &i.attrs, Some(&i.vis));
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.push(ItemKind::Const, &i.ident.to_string(), i.ident.span(), i.span(), &i.attrs, Some(&i.vis));
    }

    fn visit_item_macro(&mut self, i: &'ast syn::ItemMacro) {
        if let Some(id) = &i.ident {
            self.push(ItemKind::Macro, &id.to_string(), id.span(), i.span(), &i.attrs, None);
        }
    }

    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        self.out.use_ranges.push(lines(i.span()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"use std::fmt;
use a::{
    b,
    c,
};

/// doc
#[derive(Debug)]
pub struct Frame {
    x: u8,
}

pub const MAX: u32 = 4;

impl Frame {
    pub fn new() -> Self { Frame { x: 0 } }
    fn helper(&self) {}
}

impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { Ok(()) }
}

pub(crate) fn crate_only() {}

pub fn outer() {
    fn inner() {}
    inner();
}

pub trait T {
    fn req(&self);
    fn provided(&self) {}
}

fn main() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {}

    #[tokio::test]
    async fn async_one() {}
}

macro_rules! m { () => {} }
"#;

    fn item<'a>(s: &'a FileSyntax, path: &str) -> &'a Item {
        s.items.iter().find(|i| i.path == path).unwrap_or_else(|| panic!("no {path} in {:?}", s.items.iter().map(|i| &i.path).collect::<Vec<_>>()))
    }

    #[test]
    fn items_paths_spans_and_flags() {
        let s = analyze(SRC).unwrap();
        let frame = item(&s, "Frame");
        assert_eq!((frame.kind, frame.start, frame.end, frame.name_line, frame.is_pub), (ItemKind::Type, 7, 11, 9, true), "attributes and docs are inside the span");
        let new = item(&s, "<Frame>::new");
        assert!(new.is_pub && !new.is_test && !new.top_level);
        assert_eq!(new.name_line, 16);
        assert!(!item(&s, "<Frame>::helper").is_pub);
        assert_eq!(item(&s, "<fmt::Display for Frame>::fmt").kind, ItemKind::Fn);
        assert!(!item(&s, "crate_only").is_pub, "pub(crate) is not public");
        assert!(item(&s, "MAX").is_pub);
        assert!(item(&s, "outer::inner").name_line == 27 && !item(&s, "outer::inner").top_level);
        assert!(item(&s, "T::req").kind == ItemKind::Fn && item(&s, "T::provided").kind == ItemKind::Fn);
        let main = item(&s, "main");
        assert!(main.top_level && !main.is_pub);
        assert!(item(&s, "tests::it_works").is_test);
        assert!(item(&s, "tests::async_one").is_test, "path ending in `test` counts");
        assert_eq!(item(&s, "tests").kind, ItemKind::Mod);
        assert_eq!(item(&s, "m").kind, ItemKind::Macro);
        assert_eq!(s.item_at_name_line(9).unwrap().path, "Frame");
        assert!(s.item_at_name_line(1000).is_none());
    }

    #[test]
    fn use_ranges_cover_multiline_imports() {
        let s = analyze(SRC).unwrap();
        assert_eq!(s.use_ranges[..2], [(1, 1), (2, 5)]);
        assert!(s.in_use(3) && s.in_use(1) && !s.in_use(7));
    }

    #[test]
    fn keys_identify_items_across_versions() {
        let a = analyze("mod m { fn f() {} }\nimpl X { fn g() {} }").unwrap();
        let b = analyze("\n\nmod m { fn f() { 1; } }\n\nimpl X { fn g() {} fn h() {} }").unwrap();
        assert!(b.has_key(&a.items[1].key) && b.has_key(&a.items[2].key));
        assert!(!a.has_key(&b.items.last().unwrap().key));
    }

    #[test]
    fn unparsable_source_is_none() {
        assert!(analyze("fn broken( {").is_none());
        assert!(analyze("").unwrap().items.is_empty());
    }
}
