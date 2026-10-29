use anyhow::{Context, Result};
use proc_macro2::{Group, TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::{HashMap, HashSet};
use syn::{
    parse_file, parse_quote, punctuated::Punctuated, visit_mut::VisitMut, Expr, ExprCall, Item,
    Path, Token, UseTree,
};

#[derive(Default)]
struct Scope {
    imports: HashMap<String, String>,
}

impl Scope {
    fn new(items: &[Item]) -> Self {
        let mut scope = Self::default();
        for item in items {
            if let Item::Use(item) = item {
                scope.add_use(&item.tree, String::new());
            }
        }
        scope
    }

    fn add_use(&mut self, tree: &UseTree, prefix: String) {
        match tree {
            UseTree::Path(p) => self.add_use(&p.tree, format!("{prefix}{}::", p.ident)),
            UseTree::Name(n) if n.ident == "self" => {
                let path = prefix.trim_end_matches("::");
                if let Some(name) = path.rsplit("::").next() {
                    self.imports.insert(name.to_owned(), path.to_owned());
                }
            }
            UseTree::Name(n) => {
                self.imports
                    .insert(n.ident.to_string(), format!("{prefix}{}", n.ident));
            }
            UseTree::Rename(r) => {
                self.imports
                    .insert(r.rename.to_string(), format!("{prefix}{}", r.ident));
            }
            UseTree::Group(g) => {
                for item in &g.items {
                    self.add_use(item, prefix.clone());
                }
            }
            UseTree::Glob(_) if prefix == "std::env::" => {
                self.imports.insert("args".into(), "std::env::args".into());
                self.imports
                    .insert("args_os".into(), "std::env::args_os".into());
            }
            _ => (),
        }
    }

    fn resolve(&self, path: &Path, module: &[String]) -> String {
        let mut parts: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        if let Some(import) = parts.first().and_then(|n| self.imports.get(n)) {
            let mut expanded: Vec<_> = import.split("::").map(str::to_owned).collect();
            expanded.extend(parts.into_iter().skip(1));
            parts = expanded;
        }
        let mut base = module.to_vec();
        match parts.first().map(String::as_str) {
            Some("std" | "clap" | "wild") => return parts.join("::"),
            Some("crate") => {
                base.clear();
                parts.remove(0);
            }
            Some("self") => {
                parts.remove(0);
            }
            Some("super") => {
                while parts.first().is_some_and(|p| p == "super") {
                    base.pop();
                    parts.remove(0);
                }
            }
            _ => (),
        }
        base.extend(parts);
        base.join("::")
    }
}

/// Index only types with a verified clap Parser derive, not every type named Args.
pub fn collect_parsers(
    source: &str,
    module: &[String],
    parsers: &mut HashSet<String>,
) -> Result<()> {
    fn collect(items: &[Item], module: &[String], parsers: &mut HashSet<String>) {
        let scope = Scope::new(items);
        for item in items {
            let declaration = match item {
                Item::Struct(s) => Some((&s.ident, &s.attrs)),
                Item::Enum(e) => Some((&e.ident, &e.attrs)),
                _ => None,
            };
            if let Some((ident, attrs)) = declaration {
                let is_parser = attrs
                    .iter()
                    .filter(|a| a.path().is_ident("derive"))
                    .any(|a| {
                        a.parse_args_with(Punctuated::<Path, Token![,]>::parse_terminated)
                            .is_ok_and(|paths| {
                                paths
                                    .iter()
                                    .any(|p| scope.resolve(p, &[]) == "clap::Parser")
                            })
                    });
                if is_parser {
                    let mut path = module.to_vec();
                    path.push(ident.to_string());
                    parsers.insert(path.join("::"));
                }
            }
            if let Item::Mod(m) = item {
                if let Some((_, items)) = &m.content {
                    let mut path = module.to_vec();
                    path.push(m.ident.to_string());
                    collect(items, &path, parsers);
                }
            }
        }
    }
    collect(&parse_file(source)?.items, module, parsers);
    Ok(())
}

/// Preserve main's signature and runtime attribute verbatim. Tokio/async-std
/// perform their own expansion, including runtime flavor and worker settings.
/// Termination handles (), Result<T, E>, ExitCode, and custom implementations.
pub fn transform_source(
    source: &str,
    module: &[String],
    parsers: &HashSet<String>,
    entry: bool,
) -> Result<String> {
    let mut file = parse_file(source).context("Cannot parse command source")?;
    if entry {
        anyhow::ensure!(
            !file
                .items
                .iter()
                .any(|i| matches!(i, Item::Fn(f) if f.sig.ident == "rustbb_entry")),
            "Input defines reserved rustbb_entry function"
        );
        if !file
            .items
            .iter()
            .any(|i| matches!(i, Item::Fn(f) if f.sig.ident == "main"))
        {
            // uucore's bin! expands to an argv-reading main. Replace only this
            // documented macro, using its library's explicit argument interface.
            let mut found = false;
            for item in &mut file.items {
                if let Item::Macro(m) = item {
                    if m.mac.path.to_token_stream().to_string().replace(' ', "") == "uucore::bin" {
                        let library: Path = syn::parse2(m.mac.tokens.clone())?;
                        *item = parse_quote! {
                            fn main() -> std::process::ExitCode {
                                std::process::ExitCode::from(#library::uumain(rustbb_runtime::args_os()) as u8)
                            }
                        };
                        found = true;
                    }
                }
            }
            anyhow::ensure!(found, "No main() function found to transform");
        }
    }
    let mut transformer = ArgumentTransformer {
        parsers,
        module: module.to_vec(),
        scope: Scope::new(&file.items),
        error: None,
    };
    transformer.visit_file_mut(&mut file);
    if let Some(error) = transformer.error {
        anyhow::bail!("{error}");
    }
    if entry {
        file.items.push(parse_quote! {
            pub fn rustbb_entry() -> std::process::ExitCode {
                std::process::Termination::report(main())
            }
        });
    }
    Ok(prettyplease::unparse(&file))
}

struct ArgumentTransformer<'a> {
    parsers: &'a HashSet<String>,
    module: Vec<String>,
    scope: Scope,
    error: Option<String>,
}

impl ArgumentTransformer<'_> {
    fn rewrite_call(&self, call: &ExprCall) -> Option<Expr> {
        if !call.args.is_empty() {
            return None;
        }
        let Expr::Path(func) = &*call.func else {
            return None;
        };
        let resolved = self.scope.resolve(&func.path, &self.module);
        match resolved.as_str() {
            "std::env::args" => return Some(parse_quote!(rustbb_runtime::args())),
            "std::env::args_os" => return Some(parse_quote!(rustbb_runtime::args_os())),
            _ => (),
        }
        let mut path = func.path.clone();
        let method = path.segments.pop()?.into_value().ident.to_string();
        if !matches!(method.as_str(), "parse" | "try_parse") {
            return None;
        }
        path.segments.pop_punct();
        if path.segments.is_empty() {
            return None;
        }
        let qualified_trait = self.scope.resolve(&path, &[]) == "clap::Parser";
        if !qualified_trait
            && !self
                .parsers
                .contains(&self.scope.resolve(&path, &self.module))
        {
            return None;
        }
        let mut replacement = call.clone();
        let Expr::Path(func) = &mut *replacement.func else {
            unreachable!()
        };
        func.path.segments.last_mut().unwrap().ident = syn::Ident::new(
            if method == "parse" {
                "parse_from"
            } else {
                "try_parse_from"
            },
            proc_macro2::Span::call_site(),
        );
        replacement
            .args
            .push(parse_quote!(rustbb_runtime::args_os()));
        Some(Expr::Call(replacement))
    }

    /// Rewrite syntactic calls in supported macros without touching literals or
    /// comments. Token groups cover format arguments, vec![x; n], and nested calls.
    fn rewrite_tokens(&self, tokens: TokenStream) -> (TokenStream, bool) {
        let input: Vec<_> = tokens.into_iter().collect();
        let mut output = TokenStream::new();
        let mut changed = false;
        let mut i = 0;
        while i < input.len() {
            let mut end = i;
            while end < input.len()
                && matches!(&input[end], TokenTree::Ident(_) | TokenTree::Punct(_))
            {
                end += 1;
            }
            if end > i && input.get(end).is_some_and(|t| matches!(t, TokenTree::Group(g) if g.delimiter() == proc_macro2::Delimiter::Parenthesis)) {
                let candidate: TokenStream = input[i..=end].iter().cloned().collect();
                if let Ok(call) = syn::parse2::<ExprCall>(candidate) {
                    if let Some(replacement) = self.rewrite_call(&call) {
                        output.extend(replacement.into_token_stream()); changed = true; i = end + 1; continue;
                    }
                }
            }
            match &input[i] {
                TokenTree::Group(g) => {
                    let (tokens, inner_changed) = self.rewrite_tokens(g.stream());
                    let mut group = Group::new(g.delimiter(), tokens);
                    group.set_span(g.span());
                    output.extend([TokenTree::Group(group)]);
                    changed |= inner_changed;
                }
                token => output.extend([token.clone()]),
            }
            i += 1;
        }
        (output, changed)
    }
}

impl VisitMut for ArgumentTransformer<'_> {
    fn visit_item_mod_mut(&mut self, module: &mut syn::ItemMod) {
        if let Some((_, items)) = &module.content {
            self.module.push(module.ident.to_string());
            let old = std::mem::replace(&mut self.scope, Scope::new(items));
            syn::visit_mut::visit_item_mod_mut(self, module);
            self.scope = old;
            self.module.pop();
        }
    }

    fn visit_block_mut(&mut self, block: &mut syn::Block) {
        let old = self.scope.imports.clone();
        for stmt in &block.stmts {
            if let syn::Stmt::Item(Item::Use(item)) = stmt {
                self.scope.add_use(&item.tree, String::new());
            }
        }
        syn::visit_mut::visit_block_mut(self, block);
        self.scope.imports = old;
    }

    fn visit_expr_mut(&mut self, expr: &mut Expr) {
        syn::visit_mut::visit_expr_mut(self, expr);
        if let Expr::Call(call) = expr {
            if let Some(replacement) = self.rewrite_call(call) {
                *expr = replacement;
            }
        }
    }

    fn visit_macro_mut(&mut self, mac: &mut syn::Macro) {
        let (tokens, changed) = self.rewrite_tokens(mac.tokens.clone());
        if !changed {
            return;
        }
        let name = mac.path.to_token_stream().to_string().replace(' ', "");
        let plain = name.strip_prefix("std::").unwrap_or(&name);
        if matches!(
            plain,
            "println"
                | "print"
                | "eprintln"
                | "eprint"
                | "format"
                | "format_args"
                | "write"
                | "writeln"
                | "dbg"
                | "vec"
                | "assert"
                | "assert_eq"
                | "assert_ne"
                | "debug_assert"
                | "debug_assert_eq"
                | "debug_assert_ne"
                | "panic"
                | "matches"
        ) {
            mac.tokens = tokens;
        } else {
            self.error = Some(format!("Argument access inside unsupported macro {name}!; move the expression outside the macro"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn transform(source: &str) -> String {
        let mut parsers = HashSet::new();
        collect_parsers(source, &[], &mut parsers).unwrap();
        transform_source(source, &[], &parsers, true).unwrap()
    }
    #[test]
    fn preserves_main_return_type_and_async_options() {
        let result = transform("#[tokio::main(flavor = \"current_thread\")] async fn main() -> std::io::Result<()> { Ok(()) }");
        assert!(result.contains("flavor = \"current_thread\""));
        assert!(result.contains("async fn main() -> std::io::Result<()>"));
        assert!(result.contains("Termination::report(main())"));
        assert!(!result.contains("Runtime::new"));
    }
    #[test]
    fn only_rewrites_verified_clap_types_and_keeps_generics() {
        let result = transform("use clap::Parser; #[derive(Parser)] struct Cli<T> { x: T } struct Config; fn main() { let _ = Config::parse(); let _ = Cli::<String>::parse(); }");
        assert!(result.contains("Config::parse()"));
        assert!(result.contains("Cli::<String>::parse_from(rustbb_runtime::args_os())"));
    }
    #[test]
    fn rewrites_macro_arguments_not_literals_or_local_args_functions() {
        let result = transform("fn args() {} fn main() { args(); println!(\"std::env::args() {:?}\", std::env::args().collect::<Vec<_>>()); }");
        assert!(result.contains("args();"));
        assert!(result.contains("\"std::env::args() {:?}\""));
        assert!(result.contains("rustbb_runtime::args().collect"));
    }
    #[test]
    fn rejects_argument_access_in_unknown_macro() {
        assert!(transform_source(
            "fn main() { custom!(std::env::args()); }",
            &[],
            &HashSet::new(),
            true
        )
        .is_err());
    }
    #[test]
    fn handles_explicit_clap_trait_calls_and_environment_imports() {
        let result = transform("use std::env::{self, args_os as argv}; fn main() { let _ = env::args(); let _ = argv(); let _ = <Options as clap::Parser>::parse(); let _: Options = clap::Parser::parse(); }");
        assert!(result.contains("rustbb_runtime::args()"));
        assert!(result.contains("rustbb_runtime::args_os()"));
        assert!(result.contains("<Options as clap::Parser>::parse_from"));
        assert!(result.contains("clap::Parser::parse_from"));
    }
    #[test]
    fn verifies_module_and_import_aliases() {
        let result = transform("mod cli { use clap::Parser as DeriveParser; #[derive(DeriveParser)] pub struct Args {} } use cli::Args as Options; fn main() { let _ = Options::parse(); }");
        assert!(result.contains("Options::parse_from"));
    }
}
