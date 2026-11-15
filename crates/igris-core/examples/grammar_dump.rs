//! Grammar dump probe (v3): prints the GBNF tool-call grammar that
//! `tool_calls_grammar` builds for the seeded registry.
//!
//! Usage: `cargo run -p igris-core --example grammar_dump [-- --raw]`
//!
//! Default output escapes angle brackets (`\x3c` style) so the dump is
//! safe to display in terminals/logs where tag-like text is a parsing
//! hazard. `--raw` prints the grammar untouched for piping straight into
//! a llama-server probe (the `grammar` field of /v1/completions).
//!
//! Live-acceptance recipe:
//! ```text
//! cargo run -p igris-core --example grammar_dump -- --raw > grammar.txt
//! # then POST grammar.txt's content as the `grammar` field and check the
//! # server accepts it and emits a well-formed envelope
//! ```

fn main() {
    let raw = std::env::args().any(|a| a == "--raw");
    let registry = igris_core::tools::registry();
    let Some(g) = igris_core::toolgrammar::tool_calls_grammar(&registry.specs()) else {
        eprintln!("no tools registered — nothing to dump");
        std::process::exit(1);
    };
    if raw {
        print!("{g}");
    } else {
        let escaped = g.replace('\u{3c}', "\\x3c").replace('\u{3e}', "\\x3e");
        print!("{escaped}");
    }
}
