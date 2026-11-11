//! C#-style iterators with `yield` for stable Rust.
//!
//! The [`iterator!`] macro takes a function whose body uses `yield` and
//! compiles it into an explicit state machine driven by
//! [`core::iter::from_fn`]. There is no coroutine runtime and no allocation:
//! the returned iterator is a closure holding the function's local state and a
//! program counter, and each call to [`Iterator::next`] resumes at the
//! statement after the last `yield`.
//!
//! ```
//! use iterator::iterator;
//!
//! iterator! {
//!     fn countdown(start: i32) -> i32 {
//!         let mut n = start;
//!
//!         loop {
//!             if n < 0 {
//!                 break;
//!             }
//!
//!             yield n;
//!             n -= 1;
//!         }
//!     }
//! }
//!
//! assert_eq!(countdown(3).collect::<Vec<_>>(), vec![3, 2, 1, 0]);
//! ```
//!
//! See [`iterator!`] for the accepted syntax and what is rejected.
//!
//! # How the state machine is built
//!
//! Every statement of the body becomes one numbered state, i.e. one arm of a
//! `match` on the program counter. A state does its work and then sets the
//! program counter to the state that follows it; `yield` additionally returns
//! its value, which is where `next()` hands control back to the caller. State
//! `0` means "finished" and keeps returning `None`.
//!
//! Branches and loops don't nest at runtime: they are compiled into jumps
//! between states. To know each statement's successor when generating it, a
//! block is compiled from its last statement to its first.

#![warn(missing_docs, clippy::missing_docs_in_private_items)]

use proc_macro::TokenStream;
use proc_macro2::{Ident, Span, TokenStream as TokenStream2};
use quote::quote;
use syn::visit::{self, Visit};
use syn::{Block, Expr, ItemFn, ReturnType, Stmt, parse_macro_input, parse_quote};

/// Turns a function with `yield` statements into a lazy iterator.
///
/// The macro takes exactly one function. Its declared return type is the
/// *item* type: `fn f() -> T` becomes `fn f() -> impl Iterator<Item = T>`.
/// Attributes (including doc comments), visibility, generics, and parameters
/// are kept as written.
///
/// ```
/// use iterator::iterator;
///
/// iterator! {
///     /// The Collatz sequence starting at `start`, ending with 1.
///     pub fn collatz(start: u64) -> u64 {
///         let mut n = start;
///
///         while n > 1 {
///             yield n;
///
///             if n % 2 == 0 {
///                 n /= 2;
///             } else {
///                 n = 3 * n + 1;
///             }
///         }
///
///         yield n;
///     }
/// }
///
/// assert_eq!(collatz(6).collect::<Vec<_>>(), [6, 3, 10, 5, 16, 8, 4, 2, 1]);
/// ```
///
/// # Body
///
/// The body consists of `let` bindings followed by statements:
///
/// - **`let` bindings** must all come first. They are evaluated when the
///   function is called, and the variables live in the iterator, so their
///   values persist across `yield`s.
/// - **`yield expr;`** produces `expr` as the next item and suspends.
/// - **`if` / `else`** (including `else if` chains), **`while`**, and
///   **`loop`** may contain any of these statements, including `yield`.
/// - **`break;`** and **`continue;`** apply to the innermost `while` or `loop`.
/// - **Ordinary statements** (assignments, compound assignments, calls, method
///   calls, macros like `println!`) run when execution reaches them.
///
/// Execution of the body past the `let` bindings is lazy: nothing runs until
/// the first call to `next()`. When the end of the body is reached, the
/// iterator returns `None` from then on.
///
/// # Compile errors
///
/// Anything outside the subset above is rejected with an error pointing at
/// the offending code, rather than being miscompiled:
///
/// - `for` loops, `return`, and the `?` operator,
/// - `let` after the first non-`let` statement, and nested item definitions,
/// - labeled loops, labeled `break`/`continue`, and `break` with a value,
/// - `if let` and `while let`,
/// - a function without a return type,
/// - `yield`, `break`, or `continue` inside another expression, e.g.
///   `foo(yield 1)`.
///
/// Closures and `async` blocks inside an expression keep their own control
/// flow, so `return` and `?` are fine there, as is `break` inside a loop
/// that is itself part of an expression.
///
/// ```compile_fail
/// iterator::iterator! {
///     fn items(v: Vec<i32>) -> i32 {
///         for x in v { // error: unsupported iterator control flow
///             yield x;
///         }
///     }
/// }
/// ```
///
/// ```compile_fail
/// iterator::iterator! {
///     fn parsed(s: &str) -> i32 {
///         yield s.parse::<i32>().ok()?; // error: `?` is not supported
///     }
/// }
/// ```
///
/// # Why not `#[iterator]`
///
/// An attribute would read nicer, but rustc feature-gates `yield` while
/// parsing the item an attribute is attached to (`error[E0658]: yield syntax
/// is experimental`), before the macro can rewrite it. The tokens of a
/// function-like macro are not parsed by rustc, so `yield` reaches the macro
/// intact on stable.
#[proc_macro]
pub fn iterator(input: TokenStream) -> TokenStream {
    let function = parse_macro_input!(input as ItemFn);

    match expand(function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Jump targets for `break` and `continue` in the code being compiled.
///
/// Both are `None` outside of a loop, which turns a stray `break` or
/// `continue` into a compile error.
#[derive(Clone, Copy, Default)]
struct Context {
    /// State to continue with after the innermost loop (`break`).
    break_to: Option<usize>,
    /// State of the innermost loop's header (`continue`).
    continue_to: Option<usize>,
}

/// Lowers a function body into the arms of the state machine's `match`.
///
/// Every `compile_*` method takes the state that follows the compiled code
/// (`next`) and returns the state that enters it, so sequences are compiled
/// back to front and loops can point back at their own header.
struct Compiler {
    /// Next unused state number; `0` is reserved for "finished".
    next_id: usize,
    /// Generated `match` arms, one per state, in allocation order.
    arms: Vec<TokenStream2>,
    /// Program counter; mixed-site hygiene keeps it invisible to user code.
    pc: Ident,
}

impl Compiler {
    /// Creates a compiler with no states allocated yet.
    fn new() -> Self {
        Self {
            next_id: 1, // 0 means finished
            arms: Vec::new(),
            pc: Ident::new("pc", Span::mixed_site()),
        }
    }

    /// Reserves a fresh state number.
    fn alloc(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Compiles a `{ ... }` block; see [`Compiler::compile_seq`].
    fn compile_block(&mut self, block: &Block, next: usize, ctx: Context) -> syn::Result<usize> {
        self.compile_seq(&block.stmts, next, ctx)
    }

    /// Compiles a statement sequence that continues at `next` when done and
    /// returns its entry state (`next` itself for an empty sequence).
    ///
    /// The sequence is compiled backwards, so each statement already knows the
    /// state of the statement after it.
    fn compile_seq(&mut self, stmts: &[Stmt], mut next: usize, ctx: Context) -> syn::Result<usize> {
        for stmt in stmts.iter().rev() {
            next = self.compile_stmt(stmt, next, ctx)?;
        }
        Ok(next)
    }

    /// Compiles a single statement that continues at `next`, returning its
    /// entry state.
    ///
    /// `let` bindings and item definitions are rejected here: only those at
    /// the start of the function are allowed, and [`expand`] has already
    /// taken them out.
    fn compile_stmt(&mut self, stmt: &Stmt, next: usize, ctx: Context) -> syn::Result<usize> {
        match stmt {
            Stmt::Expr(expr, _) => self.compile_expr(expr, next, ctx),
            // `println!(..);` parses as a statement macro, not an expression.
            Stmt::Macro(_) => {
                check_plain(|c| c.visit_stmt(stmt))?;
                Ok(self.plain(quote! { #stmt }, next))
            }
            Stmt::Local(_) => Err(syn::Error::new_spanned(
                stmt,
                "`let` is only supported at the start of an `iterator!` function",
            )),
            Stmt::Item(_) => Err(syn::Error::new_spanned(
                stmt,
                "items are not supported inside an `iterator!` body",
            )),
        }
    }

    /// Compiles an expression statement that continues at `next`, returning
    /// its entry state.
    ///
    /// - `yield` becomes a state that sets the program counter to `next` and
    ///   returns the value.
    /// - `break` and `continue` become jumps to the targets in `ctx`.
    /// - `loop` and `while` get a header state; the body continues at the
    ///   header, and `ctx` is replaced so `break` leads to `next`.
    /// - `if` becomes a state that branches to the entry of either arm, both
    ///   of which continue at `next`.
    /// - Ordinary expressions become a [`Compiler::plain`] state.
    ///
    /// Everything else is rejected with an error.
    fn compile_expr(&mut self, expr: &Expr, next: usize, ctx: Context) -> syn::Result<usize> {
        let pc = self.pc.clone();

        match expr {
            Expr::Yield(y) => {
                let value = y
                    .expr
                    .as_ref()
                    .ok_or_else(|| syn::Error::new_spanned(y, "yield needs a value"))?;
                check_plain(|c| c.visit_expr(value))?;

                let id = self.alloc();
                self.arms.push(quote! {
                    #id => {
                        #pc = #next;
                        return ::core::option::Option::Some(#value);
                    }
                });
                Ok(id)
            }

            Expr::Break(b) => {
                if b.label.is_some() || b.expr.is_some() {
                    return Err(syn::Error::new_spanned(
                        b,
                        "labeled/value breaks are not supported",
                    ));
                }
                let target = ctx
                    .break_to
                    .ok_or_else(|| syn::Error::new_spanned(b, "break outside loop"))?;
                Ok(self.jump(target))
            }

            Expr::Continue(c) => {
                if c.label.is_some() {
                    return Err(syn::Error::new_spanned(
                        c,
                        "labeled continue is not supported",
                    ));
                }
                let target = ctx
                    .continue_to
                    .ok_or_else(|| syn::Error::new_spanned(c, "continue outside loop"))?;
                Ok(self.jump(target))
            }

            Expr::Loop(l) => {
                if l.label.is_some() {
                    return Err(syn::Error::new_spanned(
                        l,
                        "labeled loops are not supported",
                    ));
                }
                let header = self.alloc();
                let body = self.compile_block(
                    &l.body,
                    header,
                    Context {
                        break_to: Some(next),
                        continue_to: Some(header),
                    },
                )?;
                self.arms.push(quote! {
                    #header => { #pc = #body; }
                });
                Ok(header)
            }

            Expr::While(w) => {
                if w.label.is_some() {
                    return Err(syn::Error::new_spanned(
                        w,
                        "labeled loops are not supported",
                    ));
                }
                let cond = check_cond(&w.cond)?;
                let header = self.alloc();
                let body = self.compile_block(
                    &w.body,
                    header,
                    Context {
                        break_to: Some(next),
                        continue_to: Some(header),
                    },
                )?;
                self.arms.push(quote! {
                    #header => { #pc = if #cond { #body } else { #next }; }
                });
                Ok(header)
            }

            Expr::If(i) => {
                let cond = check_cond(&i.cond)?;
                let then_entry = self.compile_block(&i.then_branch, next, ctx)?;
                let else_entry = match &i.else_branch {
                    Some((_, expr)) => self.compile_expr(expr, next, ctx)?,
                    None => next,
                };
                let id = self.alloc();
                self.arms.push(quote! {
                    #id => { #pc = if #cond { #then_entry } else { #else_entry }; }
                });
                Ok(id)
            }

            Expr::Block(b) if b.label.is_none() && b.attrs.is_empty() => {
                self.compile_block(&b.block, next, ctx)
            }

            Expr::Return(_) | Expr::ForLoop(_) | Expr::Let(_) => Err(syn::Error::new_spanned(
                expr,
                "unsupported iterator control flow",
            )),

            // Ordinary statements have no suspension.
            Expr::Assign(_)
            | Expr::Binary(_)
            | Expr::Call(_)
            | Expr::MethodCall(_)
            | Expr::Macro(_)
            | Expr::Path(_)
            | Expr::Unary(_)
            | Expr::Paren(_) => {
                check_plain(|c| c.visit_expr(expr))?;
                Ok(self.plain(quote! { #expr; }, next))
            }

            _ => Err(syn::Error::new_spanned(
                expr,
                "unsupported expression in iterator body",
            )),
        }
    }

    /// Adds a state that runs `code` and then continues at `next`, returning
    /// the new state.
    fn plain(&mut self, code: TokenStream2, next: usize) -> usize {
        let id = self.alloc();
        let pc = &self.pc;
        self.arms.push(quote! {
            #id => {
                #code
                #pc = #next;
            }
        });
        id
    }

    /// Adds a state that does nothing but continue at `target`, returning the
    /// new state.
    fn jump(&mut self, target: usize) -> usize {
        self.plain(TokenStream2::new(), target)
    }
}

/// Validates the condition of an `if` or `while`, which is emitted verbatim.
///
/// `if let` / `while let` are rejected: their bindings would only be visible
/// in the branching state, not in the states compiled from the body.
fn check_cond(cond: &Expr) -> syn::Result<&Expr> {
    if let Expr::Let(_) = cond {
        return Err(syn::Error::new_spanned(
            cond,
            "`if let` / `while let` are not supported",
        ));
    }
    check_plain(|c| c.visit_expr(cond))?;
    Ok(cond)
}

/// Runs `visit` with a fresh [`PlainChecker`] and returns the first error found.
///
/// Used for all user code that is emitted verbatim. Control flow in such code
/// would act on the generated closure instead of the user's function, e.g.
/// `?` or `return` would silently end the iteration.
fn check_plain(visit: impl FnOnce(&mut PlainChecker)) -> syn::Result<()> {
    let mut checker = PlainChecker {
        loop_depth: 0,
        error: None,
    };
    visit(&mut checker);
    checker.error.map_or(Ok(()), Err)
}

/// Syntax visitor that finds control flow the state machine can't express.
///
/// It reports `yield`, `return`, `?`, and any `break`/`continue` that isn't
/// inside a loop of the visited code itself. Closures, `async` blocks, and
/// nested items are skipped, since their control flow stays inside them.
struct PlainChecker {
    /// Number of loops of the visited code enclosing the current node.
    loop_depth: usize,
    /// The first error found; later ones are dropped.
    error: Option<syn::Error>,
}

impl PlainChecker {
    /// Records an error at `node`, unless one was recorded already.
    fn fail(&mut self, node: &dyn quote::ToTokens, msg: &str) {
        if self.error.is_none() {
            self.error = Some(syn::Error::new_spanned(node, msg));
        }
    }

    /// Runs `f` with the loop depth increased by one.
    fn in_loop(&mut self, f: impl FnOnce(&mut Self)) {
        self.loop_depth += 1;
        f(self);
        self.loop_depth -= 1;
    }
}

impl<'a> Visit<'a> for PlainChecker {
    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Yield(_) => self.fail(expr, "`yield` must be a statement of its own"),
            Expr::Return(_) => self.fail(expr, "`return` is not supported in an `iterator!` body"),
            Expr::Try(_) => self.fail(expr, "`?` is not supported in an `iterator!` body"),
            Expr::Break(_) | Expr::Continue(_) if self.loop_depth == 0 => {
                self.fail(expr, "`break`/`continue` must be a statement of its own")
            }
            Expr::Loop(_) | Expr::While(_) | Expr::ForLoop(_) => {
                self.in_loop(|this| visit::visit_expr(this, expr))
            }
            // Closures and async blocks have their own control flow.
            Expr::Closure(_) | Expr::Async(_) => {}
            _ => visit::visit_expr(self, expr),
        }
    }

    // Nested items have their own control flow.
    fn visit_item(&mut self, _: &'a syn::Item) {}
}

/// Expands one function into its iterator-returning form.
///
/// The leading `let` bindings are kept at the top of the generated function,
/// where the `move` closure captures them, and the rest of the body is
/// compiled into states. Errors carry the span of the offending code.
fn expand(mut function: ItemFn) -> syn::Result<TokenStream2> {
    let item_type = match &function.sig.output {
        ReturnType::Type(_, ty) => ty.clone(),
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &function.sig,
                "iterator requires an item return type",
            ));
        }
    };

    // Leading `let`s become the state captured by the closure.
    let split = function
        .block
        .stmts
        .iter()
        .position(|stmt| !matches!(stmt, Stmt::Local(_)))
        .unwrap_or(function.block.stmts.len());
    let (initializers, body) = function.block.stmts.split_at(split);
    for init in initializers {
        check_plain(|c| c.visit_stmt(init))?;
    }

    let mut compiler = Compiler::new();
    let entry = compiler.compile_seq(body, 0, Context::default())?;
    let Compiler { arms, pc, .. } = compiler;

    function.sig.output = parse_quote!(-> impl ::core::iter::Iterator<Item = #item_type>);
    let attrs = &function.attrs;
    let vis = &function.vis;
    let sig = &function.sig;

    Ok(quote! {
        #(#attrs)*
        #vis #sig {
            #(#initializers)*

            let mut #pc: usize = #entry;

            ::core::iter::from_fn(move || {
                loop {
                    match #pc {
                        0 => return ::core::option::Option::None,
                        #(#arms,)*
                        _ => ::core::unreachable!("invalid iterator state"),
                    }
                }
            })
        }
    })
}

/// Tests for expansion errors, which the integration tests can't observe.
#[cfg(test)]
mod tests {
    use super::*;

    /// Expands `f`, expecting an error, and returns its message.
    fn error(f: ItemFn) -> String {
        expand(f).expect_err("should be rejected").to_string()
    }

    /// Every unsupported construct fails with its own message.
    #[test]
    fn rejects_unsupported_constructs() {
        let cases: [(ItemFn, &str); 9] = [
            (
                parse_quote!(
                    fn f() {
                        yield 1;
                    }
                ),
                "iterator requires an item return type",
            ),
            (
                parse_quote!(
                    fn f() -> i32 {
                        yield 1;
                        let x = 2;
                    }
                ),
                "`let` is only supported",
            ),
            (
                parse_quote!(
                    fn f(v: Vec<i32>) -> i32 {
                        for x in v {
                            yield x;
                        }
                    }
                ),
                "unsupported iterator control flow",
            ),
            (
                parse_quote!(
                    fn f() -> i32 {
                        return;
                    }
                ),
                "unsupported iterator control flow",
            ),
            (
                parse_quote!(
                    fn f() -> i32 {
                        yield g()?;
                    }
                ),
                "`?` is not supported",
            ),
            (
                parse_quote!(
                    fn f() -> i32 {
                        g(yield 1);
                    }
                ),
                "`yield` must be a statement",
            ),
            (
                parse_quote!(
                    fn f() -> i32 {
                        loop {
                            g(break);
                        }
                    }
                ),
                "`break`/`continue` must be a statement",
            ),
            (
                parse_quote!(
                    fn f() -> i32 {
                        'a: loop {
                            yield 1;
                        }
                    }
                ),
                "labeled loops are not supported",
            ),
            (
                parse_quote!(
                    fn f(o: Option<i32>) -> i32 {
                        if let Some(x) = o {
                            yield x;
                        }
                    }
                ),
                "`if let` / `while let`",
            ),
        ];
        for (f, expected) in cases {
            let msg = error(f);
            assert!(msg.contains(expected), "expected {expected:?}, got {msg:?}");
        }
    }

    /// Control flow that stays inside a closure or an inner loop is accepted.
    #[test]
    fn allows_control_flow_in_closures_and_inner_loops() {
        let f: ItemFn = parse_quote!(
            fn f() -> i32 {
                let get = || -> Option<i32> { Some(g()?) };
                h(loop {
                    break 1;
                });
                yield 1;
            }
        );
        assert!(expand(f).is_ok());
    }
}
