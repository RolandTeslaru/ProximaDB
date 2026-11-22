# HTTP API

The server is JSON over HTTP/1.1 with keep-alive, on a Unix domain socket, a TCP port, or both.

| Route | Does |
|---|---|
| `GET /health` | node count and the last published action |
| `POST /cypher` | a Cypher query, answered as a graph to draw and a count ([Cypher](cypher.md)) |
| `POST /reach` | who can be reached from given nodes, with filters |
| `POST /path` | the length of the shortest path between two nodes |
| `POST /actions` | apply one of Quarry's actions |
| `POST /reload` | load the datasets again and swap the new graph in ([Data](data.md)) |


**There is no authentication.** Anyone who can connect can read everything, write and reload. Keep the port on
loopback or a private network; the socket is writable by any user who can reach its directory.


## Running it

```sh
cargo build --release --features server --bin server

./target/release/server --schema schemas/social.json --data <data dir> \
    --unix /tmp/proximadb.sock --tcp 127.0.0.1:7600
```

| Flag | Meaning |
|---|---|
| `--schema <file>` | the schema file ([Data](data.md)); read again on every reload |
| `--data <dir>` | the directory the schema's dataset files are in |
| `--unix <path>` | listen on a Unix domain socket (at most 104 characters on macOS) |
| `--tcp <host:port>` | listen on TCP; with neither flag, `127.0.0.1:7600` |

At start it prints how long the load took and anything it skipped (rows without a key, links that point nowhere).

### Docker

The `Dockerfile` builds one static binary on an empty image (about 12 MB, most of it the Parquet reader), for `linux/amd64` and `linux/arm64`. The
image runs as an unprivileged user and expects the schema at `/config/schema.json` and the data at `/data`:

```sh
docker buildx build --platform linux/amd64,linux/arm64 -t proximadb .

docker run -p 127.0.0.1:7600:7600 \
    -v $PWD/schema.json:/config/schema.json:ro \
    -v /path/to/datasets:/data:ro \
    -v proximadb-socket:/run/proximadb \
    proximadb
```

The socket appears at `/run/proximadb/proximadb.sock`; share that volume with Quarry's container to connect over it.
`.github/workflows/ci.yml` tests every pull request. A version tag that passes the tests publishes the image for both
architectures, built on native runners, as `ghcr.io/rolandteslaru/proximadb:<version>` and `:latest`:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

Quarry should pin a version, not `latest`, so ProximaDB only changes under it when the version is bumped.


## Connecting from Quarry

Keep a pool of open connections, and prefer the Unix socket when both run on the same machine:

```ts
import { Pool } from 'undici'

const proxima = new Pool('http://localhost', {
    socketPath: '/run/proximadb/proximadb.sock',
    connections: 16,
})

const response = await proxima.request({
    method: 'POST',
    path: '/cypher',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ query: 'MATCH (a:User {nodeId: $id})-[:FOLLOWS*1..3]->(b:User) RETURN b', params: { id: 42 } }),
})
```

| Transport | Round trip of a tiny request |
|---|---|
| Unix socket | about 30 µs |
| TCP on localhost | about 50 µs |


## Conventions

**Nodes are named by type and key**: `{"type": "User", "key": "42"}`. Keys are text; a numeric primary key is its
decimal digits.

**Read-your-writes**: `/cypher`, `/reach` and `/path` take an optional `after`, an action's sequence number, and waits (up to 10 s) until
that action is published. They answer with `seq`, the last action it saw.

**Engine time**: reads answer with a header `x-proximadb-us`, the microseconds the engine spent; the rest of the round
trip is the transport.

**Errors** from ProximaDB are `{"error": "..."}`. A body that isn't the expected JSON is refused before that, as
plain text: 400 for bad syntax, 415 for a missing content type, 422 for a missing or mistyped field.

| Status | When |
|---|---|
| 400 | a wrong name, value, condition or query; a sequence number that isn't after the last one |
| 404 | a node key that doesn't exist in `/reach` or `/path` (in `/cypher` and `/actions` it is a 400) |
| 409 | a reload while another runs, or as of an action no longer kept |
| 422 | a reload whose datasets can't be loaded |
| 503 | the writer thread has stopped |
| 504 | `after` waited 10 s without the action being published |

**Conditions**, used by `/reach` and `/path`:

| Form | Example |
|---|---|
| compare | `{"property": "spent", "op": "ge", "value": 100}`, ops `eq ne lt le gt ge` |
| has a value | `{"has": "email"}` |
| combine | `{"and": [ … ]}`, `{"or": [ … ]}`, `{"not": { … }}` |

Coded strings and booleans allow only `eq` and `ne`. Dates may be given as `"2024-01-31"`.


## `GET /health`

```json
{"ok": true, "nodes": 458752, "seq": 1234}
```


## `POST /cypher`

```json
{"query": "MATCH (a:User {nodeId: $id})-[:FOLLOWS*1..4]->(b:User) WHERE b.status = 'active' RETURN b LIMIT 200",
 "params": {"id": 42},
 "after": 1234}
```

```json
{"nodes": [{"id": "User:42", "type": "User", "key": "42", "role": "start"},
           {"id": "User:57", "type": "User", "key": "57", "role": "match"}],
 "links": [{"type": "FOLLOWS", "from": "User:42", "to": "User:57"}],
 "count": 41461,
 "truncated": true,
 "seq": 1234}
```

| Field | Meaning |
|---|---|
| `nodes` | what to draw; `role` is `start`, `match`, or `via` (on the way to a match without being one) |
| `links` | the link each drawn node was reached by; `from` and `to` are node `id`s, as the link type stores them |
| `count` | every match, exactly, even when fewer are drawn |
| `truncated` | the drawing stopped at the limit |
| `path_length` | for `shortestPath` queries |

Titles and other properties come from Quarry's object store, by type and key. The supported Cypher is in
[cypher.md](cypher.md).


## `POST /reach`

The JSON form of a walk, for callers that build queries in code.

```json
{"start": [{"type": "User", "key": "42"}],
 "link_types": ["FOLLOWS"],
 "direction": "forward",
 "max_steps": 4,
 "through": [{"type": "User", "where": {"property": "status", "op": "eq", "value": "active"}}],
 "where": {"type": "User", "where": {"property": "spent", "op": "ge", "value": 100}},
 "keys": 10}
```

| Field | Required | Meaning |
|---|---|---|
| `start` | yes | one or more start nodes |
| `direction` | yes | `forward`, `backward` or `both` |
| `max_steps` | yes | how many links deep |
| `link_types` | no | which link types to follow; all when missing |
| `cap` | no | stop after this many nodes reached |
| `through` | no | step only onto nodes of these types that meet their condition |
| `through_others` | no | whether nodes of types not in `through` may be stepped onto (default no, unlike Cypher's filter) |
| `where` | no | count only reached nodes of this type that meet this condition |
| `keys` | no | also return up to this many of them (at most 100,000) |
| `after` | no | read-your-writes |

```json
{"count": 41461, "reached": 80436, "capped": false,
 "keys": [{"type": "User", "key": "57679"}], "seq": 1234}
```


## `POST /path`

```json
{"from": {"type": "User", "key": "42"},
 "to": {"type": "User", "key": "9"},
 "direction": "forward",
 "max_steps": 6,
 "link_types": ["FOLLOWS"],
 "through": [{"type": "User", "where": {"property": "status", "op": "eq", "value": "active"}}]}
```

```json
{"length": 3, "seq": 1234}
```

`length` is `null` when there is no path within `max_steps`. For the path's nodes and links, use `shortestPath` in
Cypher.


## `POST /actions`

One of Quarry's actions: its sequence number, which must be greater than the last one, and its edits. The whole
action is checked first; if any edit is wrong, nothing is applied. Otherwise every edit is applied and the action is
published at once.

```json
{"seq": 1235,
 "edits": [
   {"op": "add_node", "type": "User", "key": "new-1", "properties": {"status": "active", "joined": "2026-11-22"}},
   {"op": "add_link", "link_type": "FOLLOWS", "from": {"type": "User", "key": "42"}, "to": {"type": "User", "key": "new-1"}},
   {"op": "set_property", "node": {"type": "User", "key": "42"}, "property": "status", "value": "inactive"},
   {"op": "remove_link", "link_type": "FOLLOWS", "from": {"type": "User", "key": "42"}, "to": {"type": "User", "key": "9"}},
   {"op": "delete_node", "node": {"type": "User", "key": "7"}}
 ]}
```

```json
{"seq": 1235, "missing_links": 0}
```

Later edits may name a node an earlier edit of the same action added. `missing_links` counts `remove_link` edits whose
link didn't exist.


## `POST /reload`

```json
{"as_of_seq": 1234}
```

```json
{"nodes": 458752, "links": 2763895, "load_seconds": 0.32, "skipped": [],
 "replayed": 3, "failed": [], "seq": 1237}
```

How reloads fit with Quarry's sync is in [data.md](data.md).
