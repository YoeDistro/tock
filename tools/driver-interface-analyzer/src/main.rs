// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2024.

//! Analyzes Tock OS SyscallDriver implementations to record their userspace interface.
//!
//! Usage:
//!   driver-interface-analyzer <rust-source-file> [output-file]
//!
//! If output-file is omitted, writes to <source-stem>_interface.txt.
//!
//! The output file is designed to be stable across re-runs so that a diff
//! reveals meaningful interface changes.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::PathBuf;
use syn::visit::Visit;
use syn::{
    Expr, ExprCall, ExprMatch, File, GenericArgument, ImplItem, Item, Pat, PathArguments,
    Signature, Type,
};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct GrantInfo {
    /// The value of `UpcallCount<N>` – either a decimal number or a path like
    /// `upcall::COUNT` when named constants are used.
    upcall_count: Option<String>,
    allow_ro_count: Option<String>,
    allow_rw_count: Option<String>,
}

/// Holds the complete analysis result for one driver file.
#[derive(Debug)]
struct DriverInterface {
    /// Name of the type that implements `SyscallDriver` (if found).
    impl_type: Option<String>,
    /// Name of the `command_num` parameter in the `command()` signature.
    command_num_param: Option<String>,
    grant: GrantInfo,
    /// Per-arm CommandReturn variants.  Key = pattern string (e.g. "0", "1 | 2", "_").
    /// Value = sorted set of `CommandReturn::<method>` strings found in that arm.
    /// Empty set means no direct `CommandReturn::*` calls were found in the arm
    /// (indirect pattern – see `function_returns`).
    per_arm: BTreeMap<String, BTreeSet<String>>,
    /// All `CommandReturn::*` calls found anywhere in the `command()` function body.
    function_returns: BTreeSet<String>,
    /// True when a match on command_num was found.
    found_command_match: bool,
    /// True when a SyscallDriver impl with a command() fn was found.
    found_syscall_driver_impl: bool,
}

// ---------------------------------------------------------------------------
// Visitor: collect all CommandReturn::* calls in an expression subtree
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CommandReturnCollector {
    returns: BTreeSet<String>,
    /// True if at least one `.into()` call was found that might represent an
    /// implicit CommandReturn conversion.
    has_into_conversion: bool,
}

impl<'ast> Visit<'ast> for CommandReturnCollector {
    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        if let Expr::Path(path_expr) = node.func.as_ref() {
            let segs: Vec<_> = path_expr.path.segments.iter().collect();
            if segs.len() >= 2 && segs[segs.len() - 2].ident == "CommandReturn" {
                let method = segs.last().unwrap().ident.to_string();
                self.returns.insert(format!("CommandReturn::{}", method));
                // Still recurse so we catch any CommandReturn calls inside arguments.
            }
        }
        syn::visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if node.method == "into" && node.args.is_empty() {
            self.has_into_conversion = true;
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

// ---------------------------------------------------------------------------
// Visitor: find Grant<T, UpcallCount<N>, AllowRoCount<N>, AllowRwCount<N>>
// ---------------------------------------------------------------------------

struct GrantVisitor {
    info: GrantInfo,
}

impl<'ast> Visit<'ast> for GrantVisitor {
    fn visit_type_path(&mut self, node: &'ast syn::TypePath) {
        if let Some(last) = node.path.segments.last() {
            if last.ident == "Grant" {
                if let PathArguments::AngleBracketed(args) = &last.arguments {
                    let gargs: Vec<_> = args.args.iter().collect();
                    // Grant<T, UpcallCount<N>, AllowRoCount<N>, AllowRwCount<N>>
                    if gargs.len() >= 4 {
                        self.info.upcall_count =
                            extract_count_arg(&gargs[1], "UpcallCount");
                        self.info.allow_ro_count =
                            extract_count_arg(&gargs[2], "AllowRoCount");
                        self.info.allow_rw_count =
                            extract_count_arg(&gargs[3], "AllowRwCount");
                    }
                }
            }
        }
        syn::visit::visit_type_path(self, node);
    }
}

/// Extract the const generic argument from `UpcallCount<N>` / `AllowRoCount<N>` /
/// `AllowRwCount<N>`.  Returns a human-readable string such as `"1"` or
/// `"upcall::COUNT"`.
fn extract_count_arg(arg: &GenericArgument, type_name: &str) -> Option<String> {
    if let GenericArgument::Type(Type::Path(tp)) = arg {
        if let Some(last) = tp.path.segments.last() {
            if last.ident == type_name {
                if let PathArguments::AngleBracketed(inner) = &last.arguments {
                    if let Some(first) = inner.args.first() {
                        return Some(generic_arg_to_string(first));
                    }
                }
            }
        }
    }
    None
}

/// Convert a `GenericArgument` (typically a `Const` expr) to a readable string.
fn generic_arg_to_string(arg: &GenericArgument) -> String {
    match arg {
        GenericArgument::Const(expr) => expr_to_string(expr),
        GenericArgument::Type(ty) => type_to_string(ty),
        _ => "<unknown>".to_string(),
    }
}

fn type_to_string(ty: &Type) -> String {
    match ty {
        Type::Path(tp) => tp
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>()
            .join("::"),
        _ => "<type>".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Pattern and expression stringification
// ---------------------------------------------------------------------------

fn pat_to_string(pat: &Pat) -> String {
    match pat {
        Pat::Lit(lit) => lit_to_string(&lit.lit),
        Pat::Range(range) => {
            let start = range
                .start
                .as_ref()
                .map(|e| expr_to_string(e))
                .unwrap_or_default();
            let end = range
                .end
                .as_ref()
                .map(|e| expr_to_string(e))
                .unwrap_or_default();
            let limits = match range.limits {
                syn::RangeLimits::HalfOpen(_) => "..",
                syn::RangeLimits::Closed(_) => "..=",
            };
            format!("{}{}{}", start, limits, end)
        }
        Pat::Or(or) => or
            .cases
            .iter()
            .map(pat_to_string)
            .collect::<Vec<_>>()
            .join(" | "),
        Pat::Wild(_) => "_".to_string(),
        Pat::Ident(ident) => ident.ident.to_string(),
        Pat::Paren(p) => pat_to_string(&p.pat),
        _ => "<complex>".to_string(),
    }
}

fn lit_to_string(lit: &syn::Lit) -> String {
    match lit {
        syn::Lit::Int(i) => i.base10_digits().to_string(),
        syn::Lit::Str(s) => format!("\"{}\"", s.value()),
        syn::Lit::Bool(b) => b.value.to_string(),
        _ => "<literal>".to_string(),
    }
}

fn expr_to_string(expr: &Expr) -> String {
    match expr {
        Expr::Lit(lit) => lit_to_string(&lit.lit),
        Expr::Path(path) => path
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>()
            .join("::"),
        Expr::Unary(u) => {
            let op = match u.op {
                syn::UnOp::Neg(_) => "-",
                syn::UnOp::Not(_) => "!",
                _ => "?",
            };
            format!("{}{}", op, expr_to_string(&u.expr))
        }
        Expr::Block(b) => {
            // Handle `{ some::CONST }` blocks used in const generics.
            if b.block.stmts.len() == 1 {
                if let syn::Stmt::Expr(inner, None) = &b.block.stmts[0] {
                    return expr_to_string(inner);
                }
            }
            "<block>".to_string()
        }
        _ => "<expr>".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Finding the SyscallDriver impl and command() function
// ---------------------------------------------------------------------------

/// Returns `(impl_type_string, command_fn)` for the first `impl SyscallDriver`
/// block that contains a `command()` method.
fn find_syscall_driver_command<'a>(
    file: &'a File,
) -> Option<(String, &'a syn::ImplItemFn)> {
    for item in &file.items {
        if let Item::Impl(impl_item) = item {
            if let Some((_, trait_path, _)) = &impl_item.trait_ {
                let is_syscall_driver = trait_path
                    .segments
                    .last()
                    .map(|s| s.ident == "SyscallDriver")
                    .unwrap_or(false);
                if is_syscall_driver {
                    let impl_type = type_to_string(&impl_item.self_ty);
                    for member in &impl_item.items {
                        if let ImplItem::Fn(fn_item) = member {
                            if fn_item.sig.ident == "command" {
                                return Some((impl_type, fn_item));
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// Extract the name of the first `usize` parameter after `&self` in the
/// `command()` signature (this is `command_num`).
fn command_num_param_name(sig: &Signature) -> Option<String> {
    for input in &sig.inputs {
        if let syn::FnArg::Typed(pat_type) = input {
            // Skip if the type is not usize-ish (but we just take the first typed param).
            if let Pat::Ident(ident) = pat_type.pat.as_ref() {
                return Some(ident.ident.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Finding and analyzing the match on command_num
// ---------------------------------------------------------------------------

/// Search for a `match <command_num_param> { ... }` anywhere in the function
/// body (including inside closures and blocks).
fn find_command_match<'a>(
    block: &'a syn::Block,
    param_name: &str,
) -> Option<&'a ExprMatch> {
    find_match_in_block(block, param_name)
}

fn find_match_in_block<'a>(block: &'a syn::Block, param: &str) -> Option<&'a ExprMatch> {
    for stmt in &block.stmts {
        let found = match stmt {
            syn::Stmt::Expr(expr, _) => find_match_in_expr(expr, param),
            syn::Stmt::Local(local) => local
                .init
                .as_ref()
                .and_then(|init| find_match_in_expr(&init.expr, param)),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

fn find_match_in_expr<'a>(expr: &'a Expr, param: &str) -> Option<&'a ExprMatch> {
    match expr {
        Expr::Match(m) if is_ident_expr(&m.expr, param) => Some(m),
        Expr::Match(m) => {
            for arm in &m.arms {
                if let Some(r) = find_match_in_expr(&arm.body, param) {
                    return Some(r);
                }
            }
            None
        }
        Expr::Block(b) => find_match_in_block(&b.block, param),
        Expr::If(if_expr) => {
            if let Some(r) = find_match_in_block(&if_expr.then_branch, param) {
                return Some(r);
            }
            if let Some((_, else_branch)) = &if_expr.else_branch {
                return find_match_in_expr(else_branch, param);
            }
            None
        }
        Expr::Return(ret) => ret
            .expr
            .as_ref()
            .and_then(|e| find_match_in_expr(e, param)),
        Expr::Call(call) => {
            // Search inside closure arguments.
            for arg in &call.args {
                if let Some(r) = find_match_in_expr(arg, param) {
                    return Some(r);
                }
            }
            // Also search inside the callee expression itself.
            find_match_in_expr(&call.func, param)
        }
        Expr::MethodCall(mc) => {
            if let Some(r) = find_match_in_expr(&mc.receiver, param) {
                return Some(r);
            }
            for arg in &mc.args {
                if let Some(r) = find_match_in_expr(arg, param) {
                    return Some(r);
                }
            }
            None
        }
        Expr::Closure(closure) => find_match_in_expr(&closure.body, param),
        _ => None,
    }
}

fn is_ident_expr(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident == name)
            .unwrap_or(false),
        _ => false,
    }
}

/// Analyze each arm of a `match command_num { ... }` expression.
/// Returns a map from pattern string → set of CommandReturn variants found.
fn analyze_command_match(
    match_expr: &ExprMatch,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut result: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for arm in &match_expr.arms {
        let pattern = pat_to_string(&arm.pat);
        let mut collector = CommandReturnCollector::default();
        collector.visit_expr(&arm.body);
        let mut entry = collector.returns;
        if collector.has_into_conversion {
            // Indicate that there is an `.into()` conversion that likely
            // produces a CommandReturn but the exact constructor is unknown.
            entry.insert("(via .into())".to_string());
        }
        result
            .entry(pattern)
            .or_default()
            .extend(entry);
    }
    result
}

// ---------------------------------------------------------------------------
// Main analysis
// ---------------------------------------------------------------------------

fn analyze_file(source: &str) -> Result<DriverInterface, syn::Error> {
    let file = syn::parse_file(source)?;

    let mut result = DriverInterface {
        impl_type: None,
        command_num_param: None,
        grant: GrantInfo::default(),
        per_arm: BTreeMap::new(),
        function_returns: BTreeSet::new(),
        found_command_match: false,
        found_syscall_driver_impl: false,
    };

    // Collect Grant info from the whole file.
    let mut grant_visitor = GrantVisitor {
        info: GrantInfo::default(),
    };
    grant_visitor.visit_file(&file);
    result.grant = grant_visitor.info;

    // Find the SyscallDriver command() implementation.
    if let Some((impl_type, cmd_fn)) = find_syscall_driver_command(&file) {
        result.found_syscall_driver_impl = true;
        result.impl_type = Some(impl_type);

        // Get the command_num parameter name.
        let param_name = command_num_param_name(&cmd_fn.sig)
            .unwrap_or_else(|| "command_num".to_string());
        result.command_num_param = Some(param_name.clone());

        // Collect every CommandReturn call in the whole function body.
        let mut full_collector = CommandReturnCollector::default();
        full_collector.visit_block(&cmd_fn.block);
        result.function_returns = full_collector.returns;

        // Find and analyze the match on command_num.
        if let Some(match_expr) = find_command_match(&cmd_fn.block, &param_name) {
            result.found_command_match = true;
            result.per_arm = analyze_command_match(match_expr);
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

fn format_output(path: &str, iface: &DriverInterface) -> String {
    let mut out = String::new();

    writeln!(out, "# Tock Syscall Driver Interface").unwrap();
    writeln!(out, "# Source: {}", path).unwrap();
    writeln!(out).unwrap();

    // ---- impl type ----
    if let Some(ref t) = iface.impl_type {
        writeln!(out, "[driver]").unwrap();
        writeln!(out, "type = {}", t).unwrap();
        writeln!(out).unwrap();
    }

    // ---- grant ----
    writeln!(out, "[grant]").unwrap();
    let na = "not found".to_string();
    writeln!(
        out,
        "upcall_count  = {}",
        iface.grant.upcall_count.as_deref().unwrap_or(&na)
    )
    .unwrap();
    writeln!(
        out,
        "allow_ro_count = {}",
        iface.grant.allow_ro_count.as_deref().unwrap_or(&na)
    )
    .unwrap();
    writeln!(
        out,
        "allow_rw_count = {}",
        iface.grant.allow_rw_count.as_deref().unwrap_or(&na)
    )
    .unwrap();
    writeln!(out).unwrap();

    // ---- commands ----
    writeln!(out, "[commands]").unwrap();

    if !iface.found_syscall_driver_impl {
        writeln!(out, "# No SyscallDriver implementation found in this file.").unwrap();
        return out;
    }

    if let Some(ref p) = iface.command_num_param {
        writeln!(out, "# command() first parameter: {}", p).unwrap();
    }

    if !iface.found_command_match {
        writeln!(
            out,
            "# No `match <command_num>` expression found in command() body."
        )
        .unwrap();
        writeln!(out, "# All CommandReturn variants in command():").unwrap();
        for r in &iface.function_returns {
            writeln!(out, "#   {}", r).unwrap();
        }
        return out;
    }

    // Check whether per-arm analysis found direct CommandReturn calls.
    let all_indirect = iface
        .per_arm
        .values()
        .all(|v| v.is_empty() || v.iter().all(|s| s.starts_with("(via")));

    for (pattern, returns) in &iface.per_arm {
        let returns_str = if returns.is_empty() {
            "(indirect)".to_string()
        } else {
            returns.iter().cloned().collect::<Vec<_>>().join(", ")
        };
        writeln!(out, "{} = {}", pattern, returns_str).unwrap();
    }

    // If all arms are indirect (console-style), also show function-level returns.
    if all_indirect && !iface.function_returns.is_empty() {
        writeln!(out).unwrap();
        writeln!(
            out,
            "# CommandReturn calls in command() function (indirect pattern):"
        )
        .unwrap();
        for r in &iface.function_returns {
            writeln!(out, "#   {}", r).unwrap();
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        eprintln!("Usage: {} <rust-source-file> [output-file]", args[0]);
        std::process::exit(1);
    }

    let input_path = PathBuf::from(&args[1]);
    let output_path = if args.len() == 3 {
        PathBuf::from(&args[2])
    } else {
        let stem = input_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        input_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!("{}_interface.txt", stem))
    };

    let source = match fs::read_to_string(&input_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading {}: {}", input_path.display(), e);
            std::process::exit(1);
        }
    };

    let iface = match analyze_file(&source) {
        Ok(i) => i,
        Err(e) => {
            eprintln!(
                "Error parsing {}: {}",
                input_path.display(),
                e
            );
            std::process::exit(1);
        }
    };

    let output = format_output(&input_path.display().to_string(), &iface);

    if let Err(e) = fs::write(&output_path, &output) {
        eprintln!("Error writing {}: {}", output_path.display(), e);
        std::process::exit(1);
    }

    print!("{}", output);
    eprintln!("Interface written to: {}", output_path.display());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const GPIO_LIKE: &str = r#"
        use kernel::grant::{Grant, UpcallCount, AllowRoCount, AllowRwCount};
        use kernel::syscall::{CommandReturn, SyscallDriver};
        use kernel::ProcessId;

        pub struct MyDriver {
            apps: Grant<(), UpcallCount<1>, AllowRoCount<0>, AllowRwCount<0>>,
        }

        impl SyscallDriver for MyDriver {
            fn command(&self, command_num: usize, r2: usize, _: usize, _: ProcessId) -> CommandReturn {
                match command_num {
                    0 => CommandReturn::success(),
                    1 => {
                        if r2 > 10 {
                            CommandReturn::failure(ErrorCode::INVAL)
                        } else {
                            CommandReturn::success_u32(r2 as u32)
                        }
                    }
                    2 | 3 => CommandReturn::success_u32_u32(1, 2),
                    _ => CommandReturn::failure(ErrorCode::NOSUPPORT),
                }
            }

            fn allocate_grant(&self, _: ProcessId) -> Result<(), kernel::process::Error> { Ok(()) }
        }
    "#;

    #[test]
    fn test_gpio_like() {
        let iface = analyze_file(GPIO_LIKE).unwrap();
        assert!(iface.found_syscall_driver_impl);
        assert!(iface.found_command_match);
        assert_eq!(iface.grant.upcall_count.as_deref(), Some("1"));
        assert_eq!(iface.grant.allow_ro_count.as_deref(), Some("0"));
        assert_eq!(iface.grant.allow_rw_count.as_deref(), Some("0"));
        assert_eq!(
            iface.per_arm.get("0").unwrap(),
            &BTreeSet::from(["CommandReturn::success".to_string()])
        );
        assert_eq!(
            iface.per_arm.get("1").unwrap(),
            &BTreeSet::from([
                "CommandReturn::failure".to_string(),
                "CommandReturn::success_u32".to_string()
            ])
        );
        assert_eq!(
            iface.per_arm.get("2 | 3").unwrap(),
            &BTreeSet::from(["CommandReturn::success_u32_u32".to_string()])
        );
        assert_eq!(
            iface.per_arm.get("_").unwrap(),
            &BTreeSet::from(["CommandReturn::failure".to_string()])
        );
    }

    const NAMED_CONST: &str = r#"
        mod upcall { pub const COUNT: u8 = 2; }
        mod ro_allow { pub const COUNT: u8 = 1; }
        mod rw_allow { pub const COUNT: u8 = 1; }

        pub struct ConsoleDriver {
            apps: Grant<App, UpcallCount<{ upcall::COUNT }>, AllowRoCount<{ ro_allow::COUNT }>, AllowRwCount<{ rw_allow::COUNT }>>,
        }

        impl SyscallDriver for ConsoleDriver {
            fn command(&self, cmd_num: usize, _: usize, _: usize, pid: ProcessId) -> CommandReturn {
                let res = self.apps.enter(pid, |_, _| {
                    match cmd_num {
                        0 => Ok(()),
                        1 => Err(ErrorCode::BUSY),
                        _ => Err(ErrorCode::NOSUPPORT),
                    }
                }).map_err(ErrorCode::from);
                match res {
                    Ok(Ok(())) => CommandReturn::success(),
                    Ok(Err(e)) => CommandReturn::failure(e),
                    Err(e) => CommandReturn::failure(e),
                }
            }

            fn allocate_grant(&self, _: ProcessId) -> Result<(), kernel::process::Error> { Ok(()) }
        }
    "#;

    #[test]
    fn test_named_const_grant() {
        let iface = analyze_file(NAMED_CONST).unwrap();
        assert!(iface.found_syscall_driver_impl);
        assert!(iface.found_command_match);
        // Named constant grant args.
        assert_eq!(iface.grant.upcall_count.as_deref(), Some("upcall::COUNT"));
        assert_eq!(iface.grant.allow_ro_count.as_deref(), Some("ro_allow::COUNT"));
        assert_eq!(iface.grant.allow_rw_count.as_deref(), Some("rw_allow::COUNT"));
        // The inner match is on cmd_num inside a closure.
        assert_eq!(iface.command_num_param.as_deref(), Some("cmd_num"));
        // The inner arms return Ok/Err, not CommandReturn directly.
        // The outer match returns CommandReturn – those should appear in function_returns.
        assert!(iface.function_returns.contains("CommandReturn::success"));
        assert!(iface.function_returns.contains("CommandReturn::failure"));
    }
}
