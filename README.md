<p align="center"><img src="docs/header.jpg" alt="Proxima Centauri b with Proxima Centauri behind it" width="100%"></p>
<p align="center"><sub>Artist's impression of Proxima Centauri b. Image: ESO/M. Kornmesser, <a href="https://creativecommons.org/licenses/by/4.0/">CC BY 4.0</a>.</sub></p>

# ProximaDB

A graph index for Quarry: deep connection queries (many hops, long paths, loops, conditions along a path) over
Quarry's objects and links, with reads that never wait for writes.

It keeps the links, and the properties searches filter on, packed in memory. Edits land beside the packed graph and are
folded in by background rebuilds. Everything else about an object stays in Quarry.


## Quickstart

```sh
cargo build --release --features server --bin server

mkdir -p demo

cat > demo/people.csv <<'CSV'
id,status
ana,active
ben,active
cleo,active
CSV

cat > demo/follows.csv <<'CSV'
from,to
ana,ben
ben,cleo
CSV

cat > demo/schema.json <<'JSON'
{
  "node_types": [
    {"name": "Person", "file": "people.csv", "primary_key": "id", "properties": {"status": "string"}}
  ],
  "link_types": [
    {"name": "FOLLOWS", "from_type": "Person", "to_type": "Person",
     "backing": {"dataset": {"file": "follows.csv", "a": "from", "b": "to"}}}
  ]
}
JSON

./target/release/server --schema demo/schema.json --data demo --tcp 127.0.0.1:7600
```

Then, in another terminal:

```sh
curl -s http://127.0.0.1:7600/cypher -H 'content-type: application/json' \
  -d '{"query": "MATCH (a:Person {id: $who})-[:FOLLOWS*1..3]->(b:Person) RETURN b", "params": {"who": "ana"}}'
```

The answer has ana as the start, ben and cleo as matches, the two links between them, and a count of 2.

The server has no authentication: keep it on loopback or a private network.


## Documentation

| Page | Covers |
|---|---|
| [How it works](docs/how-it-works.md) | storage, edits and views, rebuilds, walks, measurements |
| [HTTP API](docs/http-api.md) | running the server, every route, errors, connecting from Quarry |
| [Cypher](docs/cypher.md) | what queries are supported and what they return |
| [Getting data in](docs/data.md) | the schema file, link backings, actions, reloads |
| [Rust library](docs/library.md) | using ProximaDB without the server |
| [Benchmarks](docs/benchmarks.md) | the server in Docker, queried with Cypher, on 0.5 to 7.3 million nodes |

Code rules are in
`AGENTS.md`.
