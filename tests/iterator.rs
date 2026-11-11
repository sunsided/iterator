//! End-to-end tests: iterators written with `iterator!`, driven through `Iterator`.

use iterator::iterator;

iterator! {
    /// `start` down to 0: `loop` with a conditional `break`.
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

iterator! {
    /// Three yields in a row, no control flow.
    fn example() -> i32 {
        yield 10;
        yield 20;
        yield 30;
    }
}

iterator! {
    /// The Collatz sequence: `while` with an `if`/`else` and a trailing `yield`.
    fn collatz(start: u64) -> u64 {
        let mut n = start;

        while n > 1 {
            yield n;

            if n.is_multiple_of(2) {
                n /= 2;
            } else {
                n = 3 * n + 1;
            }
        }

        yield n;
    }
}

iterator! {
    /// Odd numbers up to `limit`, via `continue` and an `else if` chain.
    pub fn odds(limit: u32) -> u32 {
        let mut i = 0;

        while i < limit {
            i += 1;
            if i % 2 == 0 {
                continue;
            } else if i > limit {
                break;
            }
            yield i;
        }
    }
}

iterator! {
    /// An empty body yields nothing.
    fn empty() -> u8 {}
}

iterator! {
    /// Logs before and after its only `yield`, to observe when the body runs.
    fn with_macro_stmt(log: &mut Vec<&'static str>) -> u8 {
        log.push("start");
        println!("statement macro");
        yield 1;
        log.push("end");
    }
}

iterator! {
    /// A user variable named like the generated program counter.
    fn uses_pc_name() -> i32 {
        let pc = 7;
        yield pc;
    }
}

/// The examples from the design: countdown, three yields, and Collatz.
#[test]
fn spec_examples() {
    assert_eq!(countdown(3).collect::<Vec<_>>(), vec![3, 2, 1, 0]);
    assert_eq!(countdown(-1).count(), 0);
    assert_eq!(example().collect::<Vec<_>>(), [10, 20, 30]);
    assert_eq!(
        collatz(6).collect::<Vec<_>>(),
        [6, 3, 10, 5, 16, 8, 4, 2, 1]
    );
}

/// `continue` jumps back to the loop header past an `else if` chain.
#[test]
fn continue_and_else_if() {
    assert_eq!(odds(7).collect::<Vec<_>>(), [1, 3, 5, 7]);
}

/// Empty body, name hygiene, statement macros, and `None` after the end.
#[test]
fn edge_cases() {
    assert_eq!(empty().next(), None);
    assert_eq!(uses_pc_name().collect::<Vec<_>>(), [7]);

    let mut log = Vec::new();
    let mut it = with_macro_stmt(&mut log);
    assert_eq!(it.next(), Some(1));
    assert_eq!(it.next(), None);
    assert_eq!(it.next(), None, "stays done after completion");
    drop(it);
    assert_eq!(log, ["start", "end"]);
}

/// Creating the iterator runs nothing past the leading `let` bindings.
#[test]
fn body_runs_only_when_pulled() {
    let mut log = Vec::new();
    drop(with_macro_stmt(&mut log));
    assert!(log.is_empty());
    assert_eq!(collatz(27).count(), 112);
}
