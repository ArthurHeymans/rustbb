use anyhow::{Context, Result};
use proc_macro2::Span;
use quote::quote;
use syn::{
    parse_file, parse_quote, visit_mut::VisitMut, Expr, ExprCall, ExprPath, Ident, ItemFn,
    Visibility,
};

/// Transform a crate's main.rs to expose main() as a public function
pub fn transform_main(source: &str, cmd_name: &str) -> Result<String> {
    let mut file = parse_file(source).context("Failed to parse source")?;

    let mut transformer = MainTransformer {
        cmd_name: cmd_name.to_string(),
        found_main: false,
    };

    transformer.visit_file_mut(&mut file);

    if !transformer.found_main {
        anyhow::bail!("No main() function found to transform");
    }

    // Generate the transformed source
    let output = quote!(#file);
    Ok(prettyplease::unparse(&syn::parse2(output)?))
}

struct MainTransformer {
    cmd_name: String,
    found_main: bool,
}

impl VisitMut for MainTransformer {
    fn visit_item_fn_mut(&mut self, func: &mut ItemFn) {
        // Check if this is main() and transform it
        if func.sig.ident == "main" {
            self.transform_main_fn(func);
        }

        // Continue visiting children (including function body)
        syn::visit_mut::visit_item_fn_mut(self, func);
    }

    fn visit_expr_mut(&mut self, expr: &mut Expr) {
        // First, recursively visit children
        syn::visit_mut::visit_expr_mut(self, expr);

        // Then check if this is a call to std::env::args() or std::env::args_os()
        if let Expr::Call(call) = expr {
            if is_env_args_call(call) {
                // Replace with rustbb_runtime::args()
                *expr = parse_quote!(rustbb_runtime::args());
            } else if is_env_args_os_call(call) {
                // Replace with rustbb_runtime::args_os()
                *expr = parse_quote!(rustbb_runtime::args_os());
            } else if let Some(new_expr) = transform_clap_parse(call) {
                // Replace clap::Parser::parse() with parse_from(args())
                *expr = new_expr;
            }
        }
    }
}

/// Check if this is a call to SomeType::parse() (clap Parser) and transform it
fn transform_clap_parse(call: &ExprCall) -> Option<Expr> {
    // Match pattern: Type::parse() with no arguments
    if !call.args.is_empty() {
        return None;
    }

    if let Expr::Path(ExprPath { path, .. }) = &*call.func {
        let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();

        // Match patterns like Args::parse(), Cli::parse(), etc.
        // We check if it ends with "parse" and has a type qualifier
        if segments.len() >= 2 && segments.last() == Some(&"parse".to_string()) {
            // Get the type path (everything except the last segment)
            let type_segments = &segments[..segments.len() - 1];
            let type_path = type_segments.join("::");

            // Generate: TypePath::parse_from(rustbb_runtime::args_os())
            let new_call: Expr = syn::parse_str(&format!(
                "{}::parse_from(rustbb_runtime::args_os())",
                type_path
            ))
            .ok()?;

            return Some(new_call);
        }
    }

    None
}

fn is_env_args_call(call: &ExprCall) -> bool {
    if let Expr::Path(ExprPath { path, .. }) = &*call.func {
        let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        // Match std::env::args or env::args
        let path_str = segments.join("::");
        matches!(path_str.as_str(), "std::env::args" | "env::args" | "args")
    } else {
        false
    }
}

fn is_env_args_os_call(call: &ExprCall) -> bool {
    if let Expr::Path(ExprPath { path, .. }) = &*call.func {
        let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        // Match std::env::args_os or env::args_os
        let path_str = segments.join("::");
        matches!(
            path_str.as_str(),
            "std::env::args_os" | "env::args_os" | "args_os"
        )
    } else {
        false
    }
}

impl MainTransformer {
    fn transform_main_fn(&mut self, func: &mut ItemFn) {
        self.found_main = true;

        // Check for async runtime attributes
        let async_runtime = detect_async_runtime(func);

        // Remove runtime attributes (we'll handle the runtime ourselves)
        func.attrs.retain(|attr| {
            let path_str = attr
                .path()
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            !matches!(
                path_str.as_str(),
                "tokio::main" | "async_std::main" | "main"
            )
        });

        // Rename main to rustbb_cmd_<name>
        let new_name = format!("rustbb_cmd_{}", sanitize_name(&self.cmd_name));
        func.sig.ident = Ident::new(&new_name, Span::call_site());

        // Make it public
        func.vis = Visibility::Public(Default::default());

        // Handle async vs sync differently
        let original_block = &func.block;
        match async_runtime {
            Some(AsyncRuntime::Tokio) => {
                // Remove async from signature
                func.sig.asyncness = None;
                // Wrap in tokio runtime
                func.block = parse_quote!({
                    tokio::runtime::Runtime::new()
                        .expect("Failed to create Tokio runtime")
                        .block_on(async #original_block);
                    0i32
                });
            }
            Some(AsyncRuntime::AsyncStd) => {
                // Remove async from signature
                func.sig.asyncness = None;
                // Wrap in async_std runtime
                func.block = parse_quote!({
                    async_std::task::block_on(async #original_block);
                    0i32
                });
            }
            None => {
                // Sync function - simple wrap
                func.block = parse_quote!({
                    (|| #original_block)();
                    0i32
                });
            }
        }

        // Update signature to return i32
        func.sig.output = parse_quote!(-> i32);
    }
}

#[derive(Debug, Clone, Copy)]
enum AsyncRuntime {
    Tokio,
    AsyncStd,
}

fn detect_async_runtime(func: &ItemFn) -> Option<AsyncRuntime> {
    for attr in &func.attrs {
        let path_str = attr
            .path()
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");

        match path_str.as_str() {
            "tokio::main" => return Some(AsyncRuntime::Tokio),
            "async_std::main" => return Some(AsyncRuntime::AsyncStd),
            // Also check for just "main" with tokio in scope
            "main" => {
                // This could be either, but tokio is more common
                // For now, assume tokio if async
                if func.sig.asyncness.is_some() {
                    return Some(AsyncRuntime::Tokio);
                }
            }
            _ => {}
        }
    }

    None
}

pub fn sanitize_name(name: &str) -> String {
    name.replace('-', "_").replace('.', "_").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_transform() {
        let source = r#"
fn main() {
    println!("Hello, world!");
}
"#;
        let result = transform_main(source, "hello").unwrap();
        assert!(result.contains("pub fn rustbb_cmd_hello"));
        assert!(result.contains("-> i32"));
    }

    #[test]
    fn test_transform_with_args() {
        let source = r#"
fn main() {
    let args: Vec<String> = std::env::args().collect();
    println!("{:?}", args);
}
"#;
        let result = transform_main(source, "my-cmd").unwrap();
        assert!(result.contains("pub fn rustbb_cmd_my_cmd"));
    }

    #[test]
    fn test_transform_tokio_main() {
        let source = r#"
#[tokio::main]
async fn main() {
    println!("Hello, async!");
}
"#;
        let result = transform_main(source, "async-cmd").unwrap();
        assert!(result.contains("pub fn rustbb_cmd_async_cmd"));
        assert!(result.contains("tokio::runtime::Runtime::new"));
        assert!(result.contains("block_on"));
        // Should not contain async fn
        assert!(!result.contains("async fn"));
    }
}
