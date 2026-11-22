# Cypher

ProximaDB answers a subset of Cypher: start from one node or a set of nodes, follow links some steps, then draw or
count what was found. Anything else is an error, never a slow fallback.

Send queries to `POST /cypher` ([HTTP API](http-api.md)), or call `query::cypher::run` from Rust
([Library](library.md)).


## The example graph

| Person | status | age | follows | owns |
|---|---|---|---|---|
| ana | active | 30 | ben, cleo | phone |
| ben | active | 41 | dan | |
| cleo | inactive | 25 | eve | |
| dan | active | 52 | eve | laptop |
| eve | active | 19 | | |


## The shape of a query

```text
MATCH pattern [WHERE condition] RETURN item {, item} [LIMIT n]
```

| Part | Forms |
|---|---|
| pattern | a node, a node-link-node, or `p = shortestPath(node-link-node)` |
| one start | `(a:Person {key: 'ana'})`, by the label's primary key |
| many starts | `(a:Person)`, `(a:Person {status: 'active'})`, or `WHERE` on `a` |
| direction | `-[:FOLLOWS]->`, `<-[:FOLLOWS]-`, `-[:FOLLOWS]-` |
| link types | `[:FOLLOWS]`, `[:FOLLOWS\|OWNS]`, `[r]` for any, `[:LOADOUT.a]` for dotted names |
| length | none (one step), `*2`, `*1..4`, `*..6`, `*0..2` |
| filter on the way | `(r, n \| n.status = 'active')`, inside the brackets |
| condition | `=` `<>` `<` `<=` `>` `>=`, `IS NULL`, `IS NOT NULL`, with `AND`, `OR`, `NOT`, parentheses |
| not the start | `WHERE b <> a`, joined to the rest with `AND` |
| value | `'text'`, a number, `true`, `false`, `$parameter`; a date is `'2024-01-31'` |
| return | a node, link or path name; `count(b)` |


## Examples

### One node

```cypher
MATCH (a:Person {key: 'ana'}) RETURN a
```

Answer: ana.

### Neighbours

```cypher
MATCH (a:Person {key: 'ana'})-[:FOLLOWS]->(b) RETURN b
```

Answer: ben, cleo.

```cypher
MATCH (a:Person {key: 'eve'})<-[:FOLLOWS]-(b) RETURN b
```

Answer: cleo, dan.

```cypher
MATCH (a:Person {key: 'ana'})-[r]-(b) RETURN a, r, b
```

Answer: ben, cleo, phone.

### Several steps

```cypher
MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3]->(b:Person) RETURN b
```

Answer: ben, cleo, dan, eve.

```cypher
MATCH (a:Person {key: 'ana'})-[:FOLLOWS*2]->(b:Person) RETURN b
```

Answer: dan, eve.

### Filter what is found

```cypher
MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3]->(b:Person) WHERE b.age > 40 RETURN b
```

Answer: ben, dan.

### Filter the way there

```cypher
MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3 (r, n | n.status = 'active')]->(b:Person) RETURN b
```

Answer: ben, dan, eve. The walk never steps onto cleo, and reaches eve through dan.

### Start from every node that matches

```cypher
MATCH (a:Person) WHERE a.age > 40 RETURN a
```

Answer: ben, dan.

```cypher
MATCH (a:Person)-[:FOLLOWS]->(b:Person) WHERE a.age > 40 RETURN a, b
```

Answer: dan, eve.

```cypher
MATCH (t:Thing)<-[:OWNS]-(p:Person) RETURN p
```

Answer: ana, dan.

### Shortest path

```cypher
MATCH p = shortestPath((a:Person {key: 'ana'})-[:FOLLOWS*..6]->(b:Person {key: 'eve'})) RETURN p
```

Answer: eve. The path is ana, cleo, eve.

```cypher
MATCH p = shortestPath((a:Person {key: 'ana'})-[:FOLLOWS*..6 (r, n | n.status = 'active')]->(b:Person {key: 'eve'})) RETURN p
```

Answer: eve. The path is ana, ben, dan, eve.

### Not the start

With a loop, a walk can lead back to its start, and the start then counts like any other node, as in standard
Cypher. With `ben → ana` added to the example graph:

```cypher
MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3]->(b:Person) RETURN b
```

Answer: ana, ben, cleo, dan, eve.

```cypher
MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3]->(b:Person) WHERE b <> a RETURN b
```

Answer: ben, cleo, dan, eve.

### Count

```cypher
MATCH (a:Person {status: 'active'}) RETURN count(a)
```

Count: 4.

```cypher
MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3]->(b:Person) WHERE b.age > 40 RETURN count(b)
```

Count: 2.

### Parameters and limits

```cypher
MATCH (a:Person {key: $who})-[:FOLLOWS*1..3]->(b:Person) RETURN b LIMIT 200
```

Answer, with `who` = `ana`: ben, cleo, dan, eve.


## What comes back

| Field | Meaning |
|---|---|
| nodes | each with a role: `start`, `match`, or `via` (on the way to a match) |
| links | the link each drawn node was reached by, always written from side a to side b |
| count | every match, exactly; for a shortest path, 1 or 0 |
| truncated | the drawing stopped early |
| path length | for `shortestPath` |

The drawing is a tree: one link back towards a start for each node, nearest matches first. With many starts, only
starts that lead to a drawn match appear. A query that only counts draws nothing.

`LIMIT` caps the matches drawn (1,000 by default, 10,000 at most) and never changes the count. The drawing also stops
after visiting 50 nodes per match allowed, so a rare match can draw fewer than `LIMIT` with `truncated` set.

Properties aren't returned. The caller fetches them from Quarry by type and key.


## Where it differs from standard Cypher

| Topic | ProximaDB |
|---|---|
| count | `count(b)` counts distinct nodes, not paths; `count(b)` and `count(DISTINCT b)` are the same |
| distance | with `*min..max` a node has only its shortest distance: one at distance 1 isn't matched by `*2..3` |
| filter on the way | applies to each node type that has all the properties it names; **nodes of other types pass** |
| filter in a shortest path | the two ends are exempt |
| `shortestPath` | takes both ends by key, takes no `WHERE`, and ignores the minimum length |
| `WHERE` | each part joined by `AND` is about one node, the start or the end, or is `b <> a` |
| `b <> a` with many starts | the end is never any of the starts, not just the one it was reached from |
| filtering the end | needs a label on the end node |

The filter rule is what lets a filter pick out an intermediary object type (loans since 2025) without blocking the
people and things either side of it.


## Limits

| Limit | Value |
|---|---|
| words and symbols in a query | 2,000 |
| nesting of parentheses and `NOT` | 64 |
| steps in a link pattern | 1,100, and a maximum must be given |


## Not supported

| Not supported | Example |
|---|---|
| several patterns, chains | `MATCH (a), (c)`, `(a)-[]->(b)-[]->(c)` |
| a link without brackets | `(a)-->(b)`: write `(a)-[]->(b)` |
| a start without a label | `MATCH (a)-[:FOLLOWS]->(b)` |
| returning properties | `RETURN b.name` |
| counting starts that reach something | `RETURN count(a)` with a link |
| sorting, grouping, other aggregates | `ORDER BY`, `collect()`, `avg()` |
| writes | `CREATE`, `SET`, `DELETE`: writes are actions |
| comments and semicolons | `// note`, a final `;` |
| quantified path patterns | `(()-[]->()){1,4}` |
