# iterator

A procedural macro for writing C#-style iterators with `yield` in stable Rust. The function body is compiled into an explicit state machine driven by `std::iter::from_fn`, so there is no coroutine runtime and no allocation; each call to `next()` resumes where the last `yield` left off.

```rust
use iterator::iterator;

iterator! {
    fn countdown(start: i32) -> i32 {
        let mut n = start;

        loop {
            if n < 0 {
                break;
            }

            yield n;
            n -= 1;
        }
    }
}

fn main() {
    assert_eq!(countdown(3).collect::<Vec<_>>(), vec![3, 2, 1, 0]);
}
```

The declared return type is the type of the yielded items, not the return type of the function; the macro rewrites it to `impl Iterator<Item = i32>`.

This is an experiment and a deliberately small first version: it covers loops, branches, and state declared up front, and rejects everything else at compile time rather than guessing. YMMV.

## Why `iterator! { }` and not `#[iterator]`

An attribute, `#[iterator] fn countdown(…)`, would read nicer, but it can't work on stable: rustc checks the `yield` feature gate while parsing the item, before the attribute macro ever gets to rewrite it, and fails with

```
error[E0658]: yield syntax is experimental
```

The tokens of a function-like macro aren't parsed as Rust by the compiler, so `yield` makes it through to the macro unharmed. The body is written exactly as it would be inside the attribute version; only the wrapping differs.

## What's supported

- `yield expr;` as a suspension point,
- `break;` and `continue;` inside loops,
- `if` / `else` (including `else if` chains), `while`, and `loop`,
- `let` bindings at the start of the function, which live across yields,
- ordinary statements: assignments, compound assignments, calls, method calls, and macros such as `println!`,
- doc comments, attributes, visibility, and generics on the function.

The following are rejected with a compile error pointing at the offending code:

- `for` loops, `return`, and `?`,
- `let` anywhere but at the start of the function,
- labeled loops, labeled `break`/`continue`, and `break` with a value,
- `if let` and `while let`,
- `yield`, `break`, or `continue` nested inside another expression, e.g. `foo(yield 1)`.

The rules on `return`, `?`, and nesting matter more than they look: your code ends up inside the generated closure, so a `return` or `?` hidden in an ordinary expression would return from the closure instead, silently ending the iteration. Closures and nested loops inside an expression keep their own control flow and are fine.

## How it works

Take the simplest possible iterator:

```rust
iterator! {
    fn example() -> i32 {
        yield 10;
        yield 20;
        yield 30;
    }
}
```

The macro turns it into something roughly like this:

```rust
fn example() -> impl Iterator<Item = i32> {
    let mut pc = 1;

    std::iter::from_fn(move || {
        loop {
            match pc {
                1 => { pc = 2; return Some(10); }
                2 => { pc = 3; return Some(20); }
                3 => { pc = 0; return Some(30); }
                0 => return None,
                _ => unreachable!(),
            }
        }
    })
}
```

`pc` is the program counter, and state `0` means the iterator is done. Every statement becomes a numbered state that does its work and sets `pc` to the state that follows it; a `yield` sets `pc` and returns the value, which is the only place where `next()` hands control back to the caller. Branches and loops are jumps between states: a `while` header evaluates its condition and picks either the first state of the body or the state after the loop, and `break`/`continue` jump to those same two targets.

The compiler walks each block backwards, so every statement already knows its successor's state number when it is generated. The generated `pc` uses mixed-site hygiene, so a variable called `pc` in your own code doesn't clash with it.

## Limitations

The `let` bindings at the top of the function become the closure's captured state, which has two consequences: their initializers run when the function is called rather than on the first `next()` (the rest of the body is lazy), and there is no way to declare a variable inside a loop or branch.

Lifting that restriction needs a liveness analysis that works out, for each `yield`, which variables have to survive it, and stores only those in the state (an `enum` per suspension point rather than one flat closure environment). That is also what `for` loops and late `let`s need, and it's the next milestone.

## Development

The repository has a [Taskfile](https://taskfile.dev) for the usual chores; `task` lists them all:

```sh
task test   # unit, integration and doc tests
task ci     # format check, clippy with -D warnings, tests
```

Without `task`, plain `cargo test` and `cargo clippy --all-targets` do the same.
