# Getting data in

Data reaches ProximaDB two ways, and one sequence number ties them together:

| Way | What | When |
|---|---|---|
| datasets | Quarry's backing datasets, Parquet (or CSV), loaded whole | at start, and on `POST /reload` after Quarry's sync rewrites them |
| actions | Quarry's actions, one at a time, applied on top | `POST /actions`, as users edit through Quarry |

Every action carries Quarry's sequence number. A load or reload says which action its datasets already hold
(`as_of_seq`); actions after it go on top.


## The schema file

The schema says which object types and link types to load, where their data is, and which properties to keep. Only
the properties searches filter on need to be listed.

```json
{
  "node_types": [
    {"name": "Operative", "file": "operatives.parquet", "primary_key": "id",
     "properties": {"status": "string", "rank": "integer", "joined": "date"}},
    {"name": "Station", "file": "stations.parquet", "primary_key": "id", "properties": {}},
    {"name": "Equipment", "file": "equipment.parquet", "primary_key": "serial", "properties": {"kind": "string"}},
    {"name": "Assignment", "file": "assignments.parquet", "primary_key": "id",
     "properties": {"since": "date", "operativeId": "string", "equipmentId": "string"}}
  ],
  "link_types": [
    {"name": "WORKS_AT", "from_type": "Operative", "to_type": "Station",
     "backing": {"foreign_key": {"side": "a", "property": "stationId"}}},
    {"name": "REPORTS_TO", "from_type": "Operative", "to_type": "Operative",
     "backing": {"dataset": {"file": "reporting.parquet", "a": "operative", "b": "manager"}}},
    {"name": "LOADOUT", "from_type": "Operative", "to_type": "Equipment",
     "backing": {"object": {"type": "Assignment", "a": "operativeId", "b": "equipmentId"}}}
  ]
}
```

### Node types

| Field | Meaning |
|---|---|
| `name` | the label used in queries |
| `file` | the dataset, relative to the data directory: `.parquet`, or else read as CSV with a header row |
| `primary_key` | the column holding each object's key |
| `properties` | name → kind: `string`, `integer`, `long`, `double`, `boolean`, `date`, `timestamp` |

### Link types

A link type joins side a (`from_type`) to side b (`to_type`). Its `backing` is one of Quarry's three:

| Backing | Where the pairs come from | Fields |
|---|---|---|
| `dataset` | a join dataset, one column per side holding that side's primary key | `file`, `a`, `b` |
| `foreign_key` | a property on side a's or side b's object type holding the other side's primary key, read from that type's own dataset | `side` (`a` or `b`), `property` |
| `object` | an intermediary object type with one key property per side | `type`, `a`, `b` |

An `object` backing loads the intermediary as nodes of its own (it must be in `node_types`) and makes two link types:

| Link type | From → to |
|---|---|
| `LOADOUT.a` | Operative → Assignment |
| `LOADOUT.b` | Assignment → Equipment |

Both point from side a towards side b, so operatives to their equipment is a forward walk across both, and the
assignments' own properties can be filtered on the way:

```cypher
MATCH (o:Operative {id: 'op-7'})-[:LOADOUT.a|LOADOUT.b*2 (r, n | n.since >= '2025-01-01')]->(e:Equipment) RETURN e
```

### Values

| Kind | Read from Parquet | Read from text (CSV, or Parquet text columns) |
|---|---|---|
| string | text | as is |
| integer, long | any integer column | decimal digits |
| double | any number column | a decimal number |
| boolean | boolean | `true` / `false` / `1` / `0` |
| date | date | `YYYY-MM-DD` |
| timestamp | timestamp | `YYYY-MM-DD HH:MM:SS` |

Integer primary keys are matched as their decimal digits, so a key column may be an integer in one dataset and text
in another. Parquet files may be compressed with Snappy, Zstd or LZ4.


## Loading

Loading reads only the columns the schema names, a batch of 65,536 rows at a time:

1. Nodes, one type after another: the primary key and the property columns.
2. Links, once every node exists, so a key may point at a type loaded later. Looking up the keys runs on every core.
3. The breadth-first layout ([How it works](how-it-works.md#7-layout-linked-nodes-close-together)).

Rows with bad keys are skipped and counted, never fatal:

| Skipped | Reported as |
|---|---|
| a row without a primary key | rows without a key |
| a row whose key its type already has | duplicate keys |
| a link with an empty key on either side | empty |
| a link key that names no node | pointing to no node |

Bad **values** are not reported: one that can't be read as its kind becomes "no value". Dates aren't checked against
the calendar, and an `integer` is 32 bits, so use `long` for anything larger.

The server prints these at start and returns them from `/reload`, much like Quarry's mapping health check.

On the test data, loading 7.3 million nodes and 45 million links takes 9.8 s from Parquet (13.3 s from CSV).


## Actions

`POST /actions` ([HTTP API](http-api.md#post-actions)) applies one action: add or delete a node, set a property, add
or remove a link. Sequence numbers must increase. Edits sit beside the packed graph until a rebuild folds them in
([How it works](how-it-works.md#6-rebuilds)).


## Reloads

When Quarry's sync (DuckDB) rewrites the backing datasets:

1. Quarry writes the Parquet files where ProximaDB reads them (a shared volume, the same machine, or object storage
   mounted as files).
2. Quarry calls `POST /reload {"as_of_seq": N}`, N being the last action the datasets hold.
3. ProximaDB reads the schema file again and loads everything on a background thread. Searches and actions carry on
   against the current graph.
4. The writer applies again the actions after N that it kept, then searches move to the new graph. No search sees the
   data go back in time.

```json
{"nodes": 458752, "links": 2763895, "load_seconds": 0.32,
 "skipped": [], "replayed": 3, "failed": ["action 1235: no User with key new-1"], "seq": 1237}
```

`failed` lists replayed actions that no longer apply to the new data, such as a link to an object the datasets don't
have. They are skipped; everything else stands.

The writer keeps the last 1,000,000 actions it applied since it started, and drops those a reload already holds. A
reload as of an action older than it keeps, or older than the last reload, is refused (409).
One reload runs at a time.


## Not yet

| Gap | Effect today |
|---|---|
| Persistence | a restart loads the datasets again; actions since the last reload are gone unless Quarry sends them again |
| A starting point | the data loaded at start is taken to be as of action 0, so after a restart ProximaDB can't tell that a reload is missing actions from before the restart |
| Authentication | anyone who can connect can read, write and reload |
| Batches of actions | one action per request |
| Partial reloads | a reload loads every dataset, even if one changed |
