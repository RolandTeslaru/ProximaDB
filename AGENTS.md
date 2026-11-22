# ProximaDB: code-writing guidelines

These rules apply to all code written or changed in this repository, scaffolding included. The user's explicit
instructions come first. Then these rules, which adapt Quarry's `AGENTS.md` and PretzelGraph's style
(`~/Desktop/Pretzel/PretzelGraph`, e.g. `packages/worker/src/S2/engine.ts`, `turboGraph/index.ts`,
`engine/index.ts`) to Rust. Where Rust's own tools have a convention (rustfmt, clippy, rustdoc, cargo), it wins over
a TypeScript habit, as noted below.

## Tools are the formatting authority

- Run `cargo fmt --all` after every change. `rustfmt.toml` keeps the defaults except one setting that makes code
  breathe (below). Don't align `=` or field values by hand: rustfmt removes it.
- `cargo clippy --all-targets --features server` stays free of warnings.
- `cargo test --release` passes, and a change to storage, walks or edits is benchmarked before and after
  (`research/`, outside the repository).

## Hard rule: breathable code

Code is read far more than written. Give every step room.

- **Between items**: two blank lines between functions and `impl` blocks, so each stands on its own
  (`blank_lines_upper_bound = 3` keeps them).
- **Inside a function**: a blank line between its steps: reading state, checking, doing the work, publishing,
  returning. A block that does one thing has no blank lines inside; a function that does three things has two.
- **Guards stand apart**: an early `return`, `continue` or `let … else` is its own short block, followed by a blank
  line.
- **Parameters**: rustfmt's default. A signature that fits stays on one line; one that doesn't gets one parameter
  per line. Don't force short signatures (`fn chunk(&self, i: u64)`) onto several lines.
- **One step per line**: no chain of three calls in one expression to save a `let`. Name the intermediate value.
- **Overview first**: a type or module that runs a sequence of steps opens with a `//!` or `///` overview listing
  them in order, like `TurboGraph`'s numbered list. The functions below it then need little or no comment.

```rust
// Avoid
let rel = &graph.link_types[r];
if self.get(from).0 != rel.from_type || self.get(to).0 != rel.to_type {
    return false;
}
let mut found = false;
for (side, base, node, other) in [/* … */] {
    let mut done = false;
    // …
}
found

// Use
let storage = &graph.link_types[link_type];

if self.type_of(from) != storage.from_type || self.type_of(to) != storage.to_type {
    return false;
}

let mut found = false;

for (side, table, node, other) in [/* … */] {
    // …
}

found
```

## Hard rule: no defensive noise

Trust the code you call. Write only what the code has to do.

- Don't guard against states this crate can't produce. An invariant is an `expect("why it holds")`, not an
  `if let … else` fallback or an `unwrap_or` default.
- Check outside input once, at the boundary where it enters (the server's `check` module, `load`), then trust it.
  The release profile aborts on a panic, so anything a client sends must be checked there and become an error
  response, never a panic.
- Don't reword another layer's error case by case. Turn errors into a result once, at the boundary.

## Hard rule: plain, readable code

- **One job per function.** A function that parses, loads, starts threads and serves is four functions.
- **Related values are one type.** Several loose `Option`s that travel together become a named struct with its
  defaults and checks in one place.
- **Names say what something is.** `link_type`, `storage`, `property`, `row`, not `r`, `rel`, `pi`. Single letters
  only where graph code conventionally uses them: `u` and `v` for nodes in a walk, `g` for a global node number,
  `t` for a node type in a short loop.
- **Encapsulate.** A module's internals are private; others go through its methods. `pub(crate)` fields are rare,
  not the way modules talk to each other.
- **Named constants** for operational values: timeouts, queue sizes, thresholds, chunk sizes.
- **Pass functions directly**, not in a closure that only calls them.
- **Sets for membership**: a bitset or `HashSet`, never `Vec::contains` on ids.
- **`unsafe`** only with a `// Safety:` comment saying why it holds.

## Comments

- `///` on public items and `//!` on modules, as rustdoc expects. These can be several lines when they explain
  what the code can't: an invariant, an ordering, a layout, a cost.
- Never restate the name, the signature or the code.
- Inside a function, a comment is one short line, and only where the reason isn't visible.

## Structure and naming

- Layers: `core` (ids, values), `storage` (packed graph), `kinetic` (edits, generations, rebuild), `query` (walks,
  conditions), `collections`, `load`. The server lives in `src/bin/server/`, one module per job.
- Ids are typed (`node::GlobalId`, `node::Id`, `node_type::Id`, `link_type::Id`, `link::Id`); arrays indexed by an
  id are `IndexVec`. Don't pass a bare integer where an id type exists.
- Vocabulary: node, node type, link, link type, property. Examples use generic names (ana, ben, cleo, phone).
- Unit tests sit in a `#[cfg(test)] mod tests` in the same file (the Rust convention); `tests/` holds integration
  tests.

## Before writing

- Read the neighbouring code end to end, and `docs/how-it-works.md` for the design.
- Before a structural change, say what moves where and why the current layout falls short.
- Don't add an abstraction because it is easy to write; say what problem it solves.
