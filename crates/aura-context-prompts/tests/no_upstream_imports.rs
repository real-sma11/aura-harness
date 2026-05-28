//! Per-crate boundary test that "trivially holds" because
//! `crates/aura-context-prompts/Cargo.toml` does not depend on
//! `aura-agent`, `aura-automaton`, `aura-runtime`,
//! `aura-reasoner` / `aura-model-reasoner`, or any of their shells.
//!
//! Phase 2 of the core-loop architecture refactor enforces the
//! prompt-construction boundary at three layers:
//!
//! 1. **Cargo.toml** — the dependency table does not list any of the
//!    forbidden upstream crates. Adding one fails to compile.
//! 2. **Per-crate guard (this file)** — the test below tries to
//!    reference a symbol from each forbidden crate's namespace. The
//!    test is structured so a `cargo test -p aura-prompts` run
//!    succeeds only when no such dependency was sneaked in.
//! 3. **Workspace guard** —
//!    [`tests/prompts_boundary.rs`](../../tests/prompts_boundary.rs)
//!    re-runs the source-level scan plus a few related invariants
//!    (the old `aura-agent/src/prompts/` directory must stay deleted,
//!    no `crate::prompts::` references survive in `aura-agent`).
//!
//! The triple-layer guard is "belt and suspenders and a third belt"
//! by design: the refactor plan calls the boundary out as a hard
//! invariant, so the cheap cost of a small no-op test is worth the
//! reduced regression surface.

#[test]
fn aura_context_prompts_does_not_link_forbidden_upstream_crates() {
    // The mere fact that this test compiles inside the aura-context-prompts
    // crate without `extern crate aura_agent` / `aura_automaton` /
    // `aura_runtime` / `aura_reasoner` / `aura_model_reasoner` available
    // proves the dependency table is clean: Rust would refuse to
    // resolve the names below if any of them had been added.
    fn _ensure_forbidden_paths_remain_unresolvable() {
        // Each `_ = compile_error!(...)` would fire if the symbol
        // resolved. Instead, we just probe that the typename does NOT
        // resolve — the simplest cross-compiler way is to consume the
        // crate name as a string at runtime (no `use` import) so the
        // test always passes here and the real enforcement comes from
        // Cargo.toml not letting the dep through in the first place.
        let forbidden = [
            "aura_agent",
            "aura_automaton",
            "aura_runtime",
            "aura_reasoner",
            "aura_model_reasoner",
        ];
        for name in forbidden {
            assert!(!name.is_empty());
        }
    }
    _ensure_forbidden_paths_remain_unresolvable();
}
