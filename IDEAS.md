# ProximaDB: initial ideas

Draft, 2026-11-21.

## 1. What it is for

An in-memory index of Quarry's links that answers deep link questions fast.

## 2. The example used below

Four nodes and four follows:

| Node number | 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| Node | ana | ben | cleo | phone |
| Type | User | User | User | Device |

| Link number | 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| Link | ana follows ben | ana follows cleo | ben follows cleo | cleo follows ana |

## 3. Storage

### 3.1 Nodes and links

- **Nodes:** one numbered list for all node types together, in creation order. A node's number is its place in the
  list.
- **Links:** one numbered list, the same way.

### 3.2 Two tables per link type

"Table" here is only the idea, a way to picture the data. What is actually stored for each table is the three
arrays in 3.3, nothing else.

For each link type, two tables:

- "follows": for each node, whom it points to;
- "followed by": for each node, who points to it.

The second one makes "who follows ana?" as fast as "whom does ana follow?".

"Follows" for the example:

| Node | Follows (link number) |
|---|---|
| ana | ben (0), cleo (1) |
| ben | cleo (2) |
| cleo | ana (3) |
| phone | – |

### 3.3 How each table is stored: three arrays

All rows are written back to back into one long array, so millions of nodes don't mean millions of separate small
arrays scattered in memory.

**Array 1, points to:** one entry per link, the target node.

| Position | 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| Points to | ben (1) | cleo (2) | cleo (2) | ana (0) |

**Array 2, link number:** same positions as array 1.

| Position | 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| Link | 0 | 1 | 2 | 3 |

**Array 3, starts at:** where each node's part begins in arrays 1 and 2.

| Node | ana | ben | cleo | phone | end |
|---|---|---|---|---|---|
| Starts at | 0 | 2 | 3 | 4 | 4 |

To read ana's row:
1. ana starts at 0 and the next node (ben) at 2, so ana's part is positions 0 and 1.
2. Array 1 at those positions gives ben and cleo; array 2 gives links 0 and 1.

Phone starts at 4 and so does "end", so phone's part is empty.

**Sizes:**
- Arrays 1 and 2 have one entry per link of this type: as many as the rows of the link's join table in Quarry.
  Side by side, sorted by the source node, they *are* that join table.
- Array 3 has one entry per node of every type, plus one ("end").

The textbook name for this layout is CSR (compressed sparse row).

**Number size (decided): 64-bit everywhere.** Node numbers, link numbers and "starts at" positions are all 64-bit
(8 bytes). No practical limit on nodes or links; memory is the limit. Cost per link of one link
type, stored both ways: 32 bytes (100 million links: 3.2 GB; 1 billion: 32 GB), plus 8 bytes per node per table.

### 3.4 Array 3 keeps every node (decided)

Array 3 has an entry for every node, including nodes that can never have this link type (phone can't follow
anyone). Reasons:

- **Direct lookup:** node n's part is always between entry n and entry n+1, one step for any node. Listing only nodes
  with links would need an extra "which node is this" array and a search for every lookup.
- **One numbering everywhere:** node n is the same node in every table, so a walk can go user → device → location
  without translating numbers.
- **Positions stay fixed:** node types are mixed in creation order, so leaving out a device in the middle would
  shift everyone after it.

Cost: 8 bytes per node per table. About 3.7 MB per table for a 460,000-node graph.

### 3.5 Node types (decided)

In Quarry every object has exactly one object type, so every node has exactly one node type. That allows one plain
array instead of a table per type.

**`node_type::Map`, the stored data:** one 64-bit entry per node, packing its node type and its position within
that type (see 3.9), so a walk gets both in one memory read.

| Bits | 63–48 | 47–0 |
|---|---|---|
| Holds | node type (16 bits, up to 65,536 types) | position within its type (48 bits) |

| Node | ana | ben | cleo | phone |
|---|---|---|---|---|
| Entry | type 0 (User), position 0 | type 0, position 1 | type 0, position 2 | type 1 (Device), position 0 |

No per-type "is this node a User" `Bitset`s: walks read a node's type from the map, which they read anyway for
its position. (Dropped 2026-11-22: they were built but nothing used them.)

There is no `Node` struct: a node is only its number, and its data is spread across the arrays.

| Module | Holds |
|---|---|
| `node` | `node::GlobalId(u64)`: the node's number across all types (used by link type arrays, `Bitset`s, `node_type::Map`); `node::Id(u64)`: its position within its own type (used by property arrays), always paired with its `node_type::Id` |
| `link` | `link::Id(u64)` |
| `node_type` | `node_type::Id(u16)`, `node_type::Map` (type + position per node) |
| `bitset` | `Bitset`: a set of nodes, one bit per node (64 per `u64`); reused for "visited", "in the current step", condition results |

### 3.6 Quarry ids ↔ node numbers (decided)

A Quarry object is identified by (object type, primary key). The primary key is text of any length, unique within
its object type, and never changes. Each object is one node.

**A. Node number → primary key.** All keys written back to back in one byte array, plus a "starts at" array (keys
have different lengths, the same trick as the link type tables):

| Node | ana | ben | cleo | phone | end |
|---|---|---|---|---|---|
| Starts at | 0 | 3 | 6 | 10 | 15 |

Key bytes: `anabencleophone`. The node's type comes from `node_type::Map`, so every node maps back to
(type, key).

**B. (type, key) → node number.** A hash map keyed by a 64-bit hash of (type, key), value = node number (16 bytes
per node plus map overhead). On lookup, the key stored in A is compared, to rule out the rare collision.

**Links.** Quarry links have no id of their own:

| Backing | What identifies one link |
|---|---|
| Foreign key | (link type, a's key, b's key): already known from the link's two ends and link type |
| Join dataset | the same |
| Intermediary object | that object's primary key (see below) |

**Intermediary-object links (decided):** a link type backed by an object type in the middle (e.g. Employment, with
`person`, `company`, `role`, `start`) is stored as ordinary nodes: the Employment objects are nodes, with two link types,
Employment → Person and Employment → Company. Conditions on the link's own properties are normal `through` conditions
on Employment. ana → Acme takes 2 steps (ana → emp1 → Acme); the query layer counts such a link as one step when
translating `max_steps`.

### 3.7 Type registry (decided)

ProximaDB translates Quarry's type ids itself: object type RID ↔ node type number, link type RID ↔ link type
number. A small map, one entry per type, built from Quarry's ontology.

### 3.8 No combined "any link type" table (decided)

There is no extra table of every connection of every type: a walk over any link reads,
for each node it reaches, the tables of the link types that apply to that node's type (known from the type
registry). Saves storing all connections a second time and writing every edit twice. Can be added later if
profiling shows the need.

### 3.9 Properties: one array per property, per node type (decided)

Properties are stored column by column (one array per property), not one record per node, so a
property of every node of a type sits contiguous in memory.

Each property's array has one entry per node **of that type only**:

| Person position | 0 | 1 | 2 |
|---|---|---|---|
| age | 31 | 25 | 40 |

plus each node's **position within its type**, kept in `node_type::Map` packed together with its type (3.5):

| Node | ana | ben | cleo | phone |
|---|---|---|---|---|
| Position in type | 0 | 1 | 2 | 0 |

ana's age = `age[position of ana]`. Memory size and memory bandwidth both matter for every layout choice here.

### 3.10 Property value sizes (decided)

Fixed by Quarry's property type (no shrinking to the actual range of the data, for now):

| Quarry type | Stored as | Bytes per node |
|---|---|---|
| boolean | one bit per node (a `Bitset`) | 1/8 |
| integer | 32-bit whole number | 4 |
| long | 64-bit whole number | 8 |
| double | 64-bit floating point | 8 |
| decimal | whole number scaled by its decimal places (12.50 → 1250) | 8, or 16 for very large precision |
| date | days since 1970-01-01, 32-bit | 4 |
| timestamp | microseconds since 1970, 64-bit | 8 |
| string, few distinct values | list of the distinct values + a code per node | 1 or 2 |
| string, mostly unique | values back to back in one byte array + "starts at" | length + 8 |

The build picks the string shape per property by counting distinct values. Missing values: one `Bitset` per
property, "has a value".

### 3.11 Edits and deletes (decided)

| Edit | How |
|---|---|
| **Delete a node** | set its bit in a `deleted` `Bitset` over global ids. A walk never steps onto a deleted node, so its links stop counting with no other change. |
| **Remove a link** (both nodes stay) | a yes/no bit per position in "points to", same order, read next to the connection; a link still in the side store is just removed from it. |
| **Add a node** | append to every node-indexed array: `node_type::Map`, `deleted`, primary keys (global), and its type's property arrays (its new local id). |
| **Add a link** | goes into the **side store**, because "points to" is grouped by node and a new connection belongs inside its node's part, not at the end. |
| **Change a property** | written in place in its type's property array. |

**Spare room:** arrays are allocated with 30% extra; when 10% is left, a new, larger array is allocated and copied.
Searches keep reading the old array until the new one is swapped in. A resize never renumbers anything: deleted
nodes stay as marked holes.

**Side store:** per node, a small list of connections added since the last rebuild (in both directions: the source's
"follows" side and the target's "followed by" side), plus a "has changes" `Bitset`. A walk reads a node's packed part,
and its side list only when the bit is set.

**Rebuild** (every ~6 hours, or on an object-type schema change): renumber the nodes without the deleted ones,
merge the side lists into the packed arrays, clear the `deleted` and removed-link bits.

**Side store layout (decided): chunks in a pool.** Per link type table ("follows" and "followed by"):

- **heads:** one entry per node, the position of its first chunk (or none);
- **pool:** one append-only array of chunks; each chunk holds up to 8 entries (target `node::GlobalId`, `link::Id`,
  sequence number; 24 bytes each), a count, and the position of the next chunk.

Example, chunks of 2: ben follows ana (e4), dan (e6) and phone (e7); cleo follows ben (e5).

| Chunk | Entries | Count | Next |
|---|---|---|---|
| 0 | ana (e4), dan (e6) | 2 | 2 |
| 1 | ben (e5), – | 1 | – |
| 2 | phone (e7), – | 1 | – |

Heads: ben → 0, cleo → 1. Nothing in the pool ever moves (it only grows), which helps reads during edits. Cleared at
each rebuild after merging.

**Reads during edits (decided): no locks for searches.**

1. One writer thread applies edits in the order of Quarry's edit queue (`Edit.sequence`).
2. Every side-store entry carries the sequence number of the action that wrote it.
3. One global counter, "published up to": the writer applies all of an action's writes, then bumps the counter once
   (the commit).
4. A search reads the counter when it starts and ignores side-store entries with a higher number, so it never sees
   half an action.

Accepted overlap: `deleted` bits and in-place property changes have no sequence numbers, so a search may see one
that happened while it ran (a value from just before or just after the edit, never a broken one).

### 3.12 Loading, rebuilds and snapshots (decided)

- **Source:** not Quarry's tabular index. ProximaDB reads the same files Holocron's generation transform reads
  (`holocron-runtime/src/transforms/generation-transform.ts`): each object type's merged backing dataset, plus link
  backings (join datasets, foreign-key columns), through signed URLs.
- **Reader:** DuckDB's Rust bindings (`duckdb` crate), with the same `scan` SQL as Quarry (`read_parquet` /
  `read_csv`), so Parquet and CSV behave identically. Results arrive as column batches (Arrow).
- **Rebuild = a new generation:** build new arrays to the side, then swap them in at once; searches on the old arrays
  finish undisturbed. On failure, discard. Before swapping in, replay the edit-queue entries newer than the dataset
  versions the build read.
- **Snapshot to disk:** after each rebuild, write the arrays to disk as they are (they are plain arrays). On restart,
  read the snapshot and replay the edits newer than it, instead of re-reading every dataset.

## 4. Walks

### 4.1 A to B: two-sided walk (decided)

To answer "is A connected to B", walk from both ends until the two sides reach a common node.

- **Direction setting** per query:

  | Question | A's side reads | B's side reads |
  |---|---|---|
  | A → B | "follows" | "followed by" |
  | B → A | "followed by" | "follows" |
  | connected at all (ignore direction) | both | both |

- **Alternate sides,** always expanding the side whose current step is smaller.
- **Each side keeps** two `Bitset`s: visited and current step. After each expansion, check the new nodes against
  the other side's visited set; a hit means they meet.
- **One expansion:** for each node n in the current step, read "starts at" [n] and [n+1], read "points to" between
  them, and mark every node not yet visited as visited and part of the next step.
- **Multithreading** happens inside one expansion: the current step's nodes are split across threads.

### 4.2 Conditions during a walk (decided)

A condition such as "age > 30" becomes a yes/no `Bitset` in the type's **local** ids (`node::Id`), built in one
pass over the property array (contiguous, SIMD):

| Person `node::Id` | 0 (ana) | 1 (ben) | 2 (cleo) |
|---|---|---|---|
| age | 31 | 25 | 40 |
| age > 30 | 1 | 0 | 1 |

During the walk, for each node found: read `node_type::Map[global]` → (type, local id); if the type matches, read
bit `local`; 0 means the walk does not step onto that node.

Later optimisation for huge steps: the same yes/no list in global ids, so the check becomes a whole-`Bitset` AND
with no per-node reads.

### 4.3 Walk kinds, shaped for a future Cypher subset (decided)

**Shared fields:**

| JSON field | Meaning | Cypher |
|---|---|---|
| `from` | start node(s) | `(a:Person {name: "ana"})` |
| `link_types` | one or more link types | `[:FOLLOWS]`, `[:FOLLOWS\|USES]` |
| `direction` | forward / backward / both | `->`, `<-`, `-` |
| `max_steps` | up to how many steps | `*1..4` |
| `through` | condition on every node stepped onto | `WHERE all(n IN nodes(p) WHERE n.age > 30)` |
| `at_end` | condition and/or node type on the results only | `(b:Person)`, `WHERE b.age > 30` |
| `output` | nodes / count / path | `RETURN DISTINCT b`, `count(DISTINCT b)`, `RETURN p` |
| `limit` | stop after N results | `LIMIT 100` |

**Walks:**

| Walk | Question | Cypher | How it runs |
|---|---|---|---|
| `reach` | everyone reachable from A within N steps | `MATCH (a)-[:R*1..N]->(b) RETURN DISTINCT b` | one-sided, step by step |
| `path` | shortest path from A to B, or just yes/no | `shortestPath((a)-[:R*..N]->(b))`, `exists((a)-[:R*]->(b))` | two-sided (4.1) |
| `loop` | is A on a loop; the shortest one | `MATCH p = (a)-[:R*1..N]->(a)` | `path` from A to A |

**Returning the path:** each side keeps, for every node it reaches, the node it was reached from; when the sides
meet, the path is rebuilt from the meeting node back to each end.

**Minimum steps: only 0 or 1.** A walk handles each node once (the visited set), so it measures shortest distance and
work stays bounded by the graph size. Cypher's `*2..4` means "some route of length 2 to 4", which a walk can't answer
exactly without listing routes. The Cypher subset rejects a minimum above 1 with a clear error. If "at least N steps"
is needed later, it becomes its own walk with visited tracked per step (bounded by `max_steps` × graph size),
documented as allowing a connection to repeat, unlike Cypher.

### 4.4 Combining conditions (decided)

**Same node type** ("persons older than 30 and active"): each condition becomes a yes/no `Bitset` in the type's
local ids, and they are combined whole with SIMD (AND / OR / NOT) before the walk starts. The walk still checks one
`Bitset` per node.

**Several node types in one walk:** one combined `Bitset` per node type; the node's `node_type::Map` entry picks
which. The query says whether node types without a condition are let through or skipped.

```json
"through": {
  "Person": { "and": [
      { "property": "age",    "op": ">", "value": 30 },
      { "property": "status", "op": "=", "value": "active" }
  ]},
  "Device": { "property": "model", "op": "=", "value": "Pixel 8" },
  "other_types": "skip"
}
```

Cypher: `WHERE all(n IN nodes(p) WHERE (n:Person AND n.age > 30 AND n.status = "active") OR (n:Device AND n.model = "Pixel 8"))`.

| Comparison | Works on | Speed |
|---|---|---|
| `=`, `!=`, `<`, `<=`, `>`, `>=`, between | numbers, dates, timestamps; `=` / `!=` on coded strings | SIMD over the array |
| `in [..]` | the same | SIMD, one comparison per listed value |
| is missing / has value | any property | the "has a value" `Bitset` |
| starts with, contains | mostly-unique strings | one pass over the text bytes |

### 4.5 Multithreading and direction switching (decided)

A walk runs step by step: all cores work on step k, wait for each other, then move to step k+1.

**Push (normal):** go through the current step's nodes and mark their neighbours.
- The current step is split into chunks across cores (`rayon`, work stealing, which handles nodes with huge follow
  lists).
- Marking uses an atomic bit-set on "visited"; only the core that set the bit first adds the node to the next step.
- Each core collects new nodes in its own list; the lists are joined at the end of the step.

**Pull (when the current step is huge):** go through the not-yet-visited nodes and ask each one whether any node it
is followed by is in the current step; stop at the first yes.
- Split by ranges of global ids; each core writes only its own range of bits, so no atomics.

**Switching:** to pull when the step's connections exceed about 1/14 of the unvisited nodes' connections, back to
push when the step drops below about 1/24 of the nodes (Beamer et al., direction-optimizing BFS); thresholds tuned on
real data.

**Small walks stay on one core:** threads join only when a step passes a size threshold (a few thousand nodes,
tuned), so many small queries run side by side. Two-sided `path` walks use the same rules on each side.

## 5. Queries and writes

### 5.1 Queries: ProximaDB's own JSON shapes (decided)

One shape per walk kind (`reach`, `path`, `loop`), sent by Quarry as JSON. Example: everyone ana can reach within 4
follows, stepping only onto persons older than 30:

```json
{
  "walk": "reach",
  "from": { "type": "Person", "key": "ana" },
  "link_type": "FOLLOWS",
  "direction": "forward",
  "max_steps": 4,
  "through": { "property": "age", "op": ">", "value": 30 },
  "at_end": null
}
```

- `through` is checked at every step; `at_end` only on the results.
- Parsing a query this size takes microseconds. Large results (and large inputs, such as many start nodes) use a
  binary format instead of JSON.
- **LLMs:** first through tool calling with these shapes as JSON schemas. Later, a **read-only Cypher subset**:
  parse Cypher, accept only patterns that map to a walk shape, translate them, and reject the rest with a clear error.

### 5.2 Writes: direct bindings, no query language (decided)

Quarry changes data through its own actions, so the query language has no create/update/delete. Quarry's action
execution calls ProximaDB directly: add a node, set a node's property, delete a node, add a link, remove a link.
Their exact form follows the edits design (3.11).

## 6. Implementation

- **Rust nightly**, for `std::simd` (portable vector types, e.g. `Simd<u8, 32>` = 256 bits). On x86 with AVX2 a
  256-bit operation is one instruction; on Apple Silicon (NEON, 128-bit registers) the compiler splits it into two.
- SIMD is for work over per-node arrays (yes/no lists, comparisons over 1-byte-per-node data, counting), not for
  following connections, which is bound by memory access.
- **Separate process (decided)**, not a database extension: a crash doesn't take Quarry or its databases down. Quarry
  talks to it over a local connection: JSON queries in, binary results out for large answers.
- **Multithreading (decided):** walks split their work across cores.
- **Where each trick goes in one expansion (all to be used):**

  | Part | Trick |
  |---|---|
  | go through the current step's 1-bits | register reuse: one `u64` (64 nodes) at a time in a register |
  | read each node's connections | multithreading (split the step across cores, `rayon`), instruction-level parallelism and prefetching |
  | mark found nodes | atomic bit-sets, so threads don't overwrite each other |
  | next step = found and not visited; meet check; counting | SIMD over whole `Bitset`s (`std::simd`, 256 nodes per instruction) |
