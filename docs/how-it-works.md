# How ProximaDB works

ProximaDB is an in-memory index of Quarry's objects and links, built for questions that follow links many steps
deep: who can reach whom, through which kind of node, how far apart two objects are. It holds the links and the
properties those questions filter on; Quarry's object store keeps everything else.

This page follows the data: how it is stored, how edits land on top of it, how a search reads it, and how the packed
form is rebuilt. Examples use a small graph:

| Person | status   | age |
|--------|----------|-----|
| ana    | active   | 30  |
| ben    | active   | 41  |
| cleo   | inactive | 25  |

| Thing  |
|--------|
| phone  |
| laptop |

Links: ana FOLLOWS ben, ana FOLLOWS cleo, ben OWNS phone, ben OWNS laptop.


## 1. Layers

| Module | Holds |
|---|---|
| `core` | ids, values, property kinds, directions |
| `storage` | the packed graph: node types, link type tables, property columns, keys |
| `kinetic` | edits on top of the packed graph, generations, the writer, rebuilds |
| `query` | conditions, walks, graph-shaped results, Cypher |
| `collections` | bitsets, block lists, `IndexVec` |
| `load` | building a graph from Parquet or CSV datasets |
| `src/bin/server` | the HTTP server |


## 2. Nodes and their numbers

Every node has a **node type** (Person, Thing) and two numbers:

| Number | Type | What it is |
|---|---|---|
| global number | `node::GlobalId` (u64) | the node's number across the whole graph |
| position | `node::Id` (u32) | the node's place within its own type |

Each type's nodes are numbered together, so a type owns one contiguous range of global numbers, and a node's position
is its global number minus the first of its type:

| Global | Node | Type | Position |
|---|---|---|---|
| 0 | ana | Person | 0 |
| 1 | ben | Person | 1 |
| 2 | cleo | Person | 2 |
| 3 | phone | Thing | 0 |
| 4 | laptop | Thing | 1 |

`node_type::Map` keeps one 2-byte type per node and the first global number of each type, so finding a node's type is
one read and its position one subtraction.

Quarry names objects by **primary key**. `PrimaryKeys` holds each node's key bytes, and `KeyIndex` maps (type, key)
back to the global number. Global numbers change at every rebuild; keys never do, which is why everything outside
ProximaDB addresses nodes by type and key.

Limits: 65,536 node types and link types; 4.3 billion nodes per node type and links per link type.


## 3. Links: two packed tables per link type

A link type joins one node type (side a, `from_type`) to another (side b, `to_type`). Each link type is stored twice,
once per direction, so a walk can follow links forward or backward at the same cost:

- `forward`: a row per node of the from-type, listing the to-type nodes it links to;
- `backward`: a row per node of the to-type, listing the from-type nodes that link to it.

Each table is three arrays, all in **positions**, not global numbers:

| Array | Type | Holds |
|---|---|---|
| `starts` | `Vec<link::Position>` (u32) | where each row's part begins in the other two, plus an end entry |
| `points_to` | `Vec<node::Id>` (u32) | the position of the node at the other end, rows back to back |
| `links` | `Vec<link::Id>` (u32) | each link's number within its link type |

OWNS, forward (rows are Persons, entries are Things):

| Row | `starts` |
|---|---|
| ana | 0 |
| ben | 0 |
| cleo | 2 |
| end | 2 |

| Position | `points_to` | `links` |
|---|---|---|
| 0 | phone (0) | 0 |
| 1 | laptop (1) | 1 |

Ben's part is positions 0 up to 2. A walk turns a position back into a global number by adding the first global
number of the type on the other side.

A link costs 16 bytes in its two tables (`points_to` and `links`, 4 + 4 per direction), and a node of a type taking
part costs 4 bytes per table in `starts`. On the test data that comes to about 18 bytes per link in all.


## 4. Properties

Only the properties that searches filter on are loaded. Each is one column per node type, indexed by position, with
a "has a value" bit per node:

| Kind | Column |
|---|---|
| string, few distinct values | `Coded8`: a 1-byte code per node and the list of values |
| string, up to 65,536 distinct values | `Coded16`: a 2-byte code per node |
| string, mostly unique | `Text`: all bytes back to back, plus where each starts |
| integer, long, double, date, timestamp | a plain array of the value |
| boolean | a bitset |

A condition such as `status = 'active' AND age > 40` turns into a bitset over the type's positions: one bit per
node, true where it matches. Coded strings and doubles are compared 32 or 8 nodes at a time with SIMD.


## 5. Edits, generations and views

The packed arrays never change. Edits land in structures beside them:

| Edit | Where it goes |
|---|---|
| link added | the side store: per link type and direction, chunks of entries per node, each with its action's sequence number |
| link removed | a bit per packed position (`removed`), or a removal number on a side-store entry |
| node added | appended after the packed nodes, with its type, position and key |
| node deleted | a bit per node |
| property changed | a change log per property |

The packed graph plus these structures is a **generation**. There is one **writer**: it applies one action's edits
and then publishes the action's sequence number (Quarry's) in one atomic store. A search takes a **view**: the
generation and the sequence number published when it started. Added nodes, added links and property changes newer
than that are ignored. Two kinds of edit carry no sequence number, deleted nodes and removed packed links: a search may
see one that happened while it ran, before the rest of its action. Walks never wait for the writer; looking up a key
added since the rebuild takes a short read lock.

Walks check the side store and removed bits only for nodes whose links changed (a bit per node).


## 6. Rebuilds

When the edits pass 5% of the packed links or nodes (at least 1,000 edits), the writer starts a **rebuild** on its
own thread:

1. A new packed graph is built from the old packed graph plus the writer's journal of actions since it was built.
   Deleted nodes are dropped, added ones join their type, and every node is renumbered.
2. Meanwhile, the writer keeps writing to the old generation and also records the new actions in a backlog.
3. When the build is done, the rebuild thread replays the backlog onto the new generation in rounds, each round taking
   what arrived during the previous one, until a round is small.
4. The writer replays the last few actions itself and swaps the new generation in. The pause is well under a
   millisecond.

Searches already running finish on the old generation, which is freed when the last one ends. Peak memory during a
rebuild is about twice the packed graph.


## 7. Layout: linked nodes close together

Every load and every rebuild lays out each type's nodes in **breadth-first order** over all link types, starting from
the highest-degree node. Nodes that link to each other end up with nearby numbers, so their rows share cache lines.
This made single-thread walks 15–25% faster on the test data.


## 8. Walks

| Walk | What it does |
|---|---|
| `walk::reach` | everyone reachable from a set of starts within a number of steps, counted exactly, on all cores |
| `walk::path_length` | the length of the shortest path between two nodes, searching from both ends and always growing the smaller side |
| `subgraph::neighbourhood` | the nearest matches as a graph to draw: each node with the link it was reached by |
| `subgraph::shortest_path` | the nodes and links of one shortest path |

`reach` switches between two ways of taking a step:

- **push**: for each node in the current step, go through its links and mark new neighbours;
- **pull**: for each node not reached yet, check whether any node linking to it is in the current step, stopping at the
  first.

Push is cheap while a step is small. Once a step holds a large share of the graph, pull reads far fewer links. The
switch follows Beamer's rule (`ALPHA = 14`, `BETA = 24` in `walk.rs`).

Conditions run before the walk, as bitsets: a **path filter** decides which nodes the walk may step onto, and an
**end filter** decides which reached nodes count. During the walk each check is one bit read.


## 9. Numbers

Measured in process, without HTTP, on an Apple M3 Pro on a generated social graph (users, devices, locations; follows, uses, seen at, lives
in), one thread / twelve threads:

| | S | M | L |
|---|---|---|---|
| Nodes / links | 0.46 M / 2.8 M | 1.8 M / 11 M | 7.3 M / 45 M |
| Memory of the packed graph | 83 MB | 335 MB | 1.42 GB |
| Load from Parquet | 0.4 s | 1.9 s | 9.8 s |
| 4 follows deep, only through active users (ordinary start) | 4.5 / 1.1 ms | 11 / 3.6 ms | 33 / 6.6 ms |
| Every node's distance to the nearest banned user | 9 / 2 ms | 37 / 7 ms | 150 / 27 ms |

At M, eight readers running filtered walks while one writer applied over a million actions a second kept a median of
about 150 ms; with no writes, 15 ms. Through the HTTP server, a round trip adds about 30 µs on a Unix socket and 50 µs
over TCP.

