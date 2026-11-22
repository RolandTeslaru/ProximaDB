# Benchmarks

ProximaDB in Docker, every query sent as Cypher to `POST /cypher`, on three sizes of the same generated graph. Every
answer was checked against a reference implementation: **0 wrong answers out of 4,245**.

Measured on 2026-11-23, commit `47fa820` (version 0.1.2).


## Setup

| | |
|---|---|
| Machine | Apple M3 Pro, 12 cores, 18 GB |
| Docker | Docker Desktop 28.5.1; its Linux VM has 12 CPUs and 7.7 GB |
| Container | the image built from this repository's `Dockerfile`, limited to **6 CPUs and 6 GB** |
| Client | Python on the Mac, one kept-open HTTP connection over TCP to the published port, one query at a time |
| Schema | [`schemas/social.json`](../schemas/social.json), datasets read from Parquet |

Each query is timed two ways:

| Time | What it covers |
|---|---|
| engine | the `x-proximadb-us` header: planning and running the query |
| round trip | what the client waited: the engine, plus HTTP, JSON and Docker's port forwarding from the Mac |

Per start: one warm-up, then three runs, of which the middle one counts. Tables give the median and the 95th
percentile across starts. Every start runs, not a sample.


## The data

A generated social graph. Follows come from Graph500's Kronecker generator, so a few users have very many links and
most have few, with loops. Each user also uses devices, devices are seen at locations, and users live in locations.

| | S | M | L |
|---|---|---|---|
| Users | 262,144 | 1,048,576 | 4,194,304 |
| Devices | 131,072 | 524,288 | 2,097,152 |
| Locations | 65,536 | 262,144 | 1,048,576 |
| **Nodes** | **458,752** | **1,835,008** | **7,340,032** |
| FOLLOWS (User → User) | 2,031,429 | 8,189,176 | 33,002,809 |
| USES (User → Device) | 262,142 | 1,048,574 | 4,194,301 |
| SEEN_AT (Device → Location) | 261,002 | 1,045,551 | 4,185,911 |
| LIVES_IN (User → Location) | 209,322 | 837,887 | 3,352,767 |
| **Links** | **2,763,895** | **11,121,188** | **44,735,788** |

Users have a status (active, inactive, banned), a language and an amount spent.

**Starts.** Each walk runs from 200 starts: 20 **hubs**, picked among the most-linked nodes, where walks fan out fast,
and 180 **ordinary** nodes. Paths run between 200 pairs, plus 15 long ones: chains of exactly 1,000 links, and pairs
with no path at all.


## The queries

| Case | Cypher |
|---|---|
| within 3 or 5 steps | `MATCH (a:User {nodeId: $id})-[*1..5]->(b) WHERE b <> a RETURN count(b)` |
| reaching this one within 5 steps | `MATCH (a:User {nodeId: $id})<-[*1..5]-(b) WHERE b <> a RETURN count(b)` |
| end filter | `MATCH (a:User {nodeId: $id})-[:FOLLOWS*1..4]->(b:User) WHERE b <> a AND b.status = $status AND b.language = $language RETURN count(b)` |
| path filter | `MATCH (a:User {nodeId: $id})-[:FOLLOWS*1..4 (r, n \| n.status = $status)]->(b:User) WHERE b <> a AND b.status = $status RETURN count(b)` |
| range filter | `MATCH (a:User {nodeId: $id})-[:FOLLOWS*1..4]->(b:User) WHERE b <> a AND b.spent >= $low AND b.spent < $high RETURN count(b)` |
| shortest path | `MATCH p = shortestPath((a:User {nodeId: $from})-[*..6]->(b:Location {nodeId: $to})) RETURN p`, up to 6 or 1,100 links |

Each start's label is its own node type (User, Device or Location). `$status` is `'active'`, the range is 100 to
under 500, and `$language` is the start's own.

Walks count every match exactly. The reference answers cap the walks at 10,000 nodes, so the count is capped the same
way before it's compared; the engine still counted everything.


## Results

Milliseconds, median / 95th percentile.

### S: 0.46 M nodes, 2.8 M links

| Query | Starts | Engine | Round trip |
|---|---|---|---|
| within 3 steps, hub | 20 | 1.82 / 2.28 | 2.81 / 3.78 |
| within 3 steps, ordinary | 180 | 0.005 / 1.51 | 0.31 / 1.86 |
| within 5 steps, hub | 20 | 4.16 / 5.22 | 4.75 / 5.75 |
| within 5 steps, ordinary | 180 | 0.005 / 3.94 | 0.37 / 4.29 |
| reaching this one within 5 steps, hub | 20 | 2.81 / 3.26 | 3.32 / 3.80 |
| reaching this one within 5 steps, ordinary | 180 | 1.50 / 3.59 | 1.93 / 4.15 |
| end filter, hub | 20 | 2.48 / 2.74 | 2.96 / 3.29 |
| end filter, ordinary | 180 | 1.70 / 1.98 | 2.26 / 2.84 |
| path filter, hub | 20 | 2.11 / 2.54 | 2.52 / 2.99 |
| path filter, ordinary | 180 | 1.75 / 1.91 | 2.14 / 2.46 |
| range filter, hub | 20 | 2.65 / 2.91 | 3.38 / 3.60 |
| range filter, ordinary | 180 | 2.18 / 2.55 | 2.76 / 3.16 |
| shortest path, up to 6 | 200 | 0.032 / 0.315 | 0.40 / 0.77 |
| 1,000-link chain from a hub | 5 | 0.394 / 0.400 | 3.20 / 3.89 |
| 1,000-link chain from an ordinary user | 5 | 0.537 / 0.606 | 4.54 / 5.33 |
| no path, up to 1,100 | 5 | 0.030 / 0.035 | 0.61 / 0.68 |

### M: 1.8 M nodes, 11 M links

| Query | Starts | Engine | Round trip |
|---|---|---|---|
| within 3 steps, hub | 20 | 5.41 / 6.11 | 6.00 / 7.08 |
| within 3 steps, ordinary | 180 | 0.007 / 4.10 | 0.35 / 4.69 |
| within 5 steps, hub | 20 | 12.10 / 12.59 | 12.76 / 13.06 |
| within 5 steps, ordinary | 180 | 0.007 / 12.39 | 0.63 / 13.09 |
| reaching this one within 5 steps, hub | 20 | 9.42 / 9.70 | 10.41 / 10.81 |
| reaching this one within 5 steps, ordinary | 180 | 5.02 / 12.37 | 5.67 / 13.54 |
| end filter, hub | 20 | 6.54 / 9.88 | 6.74 / 9.41 |
| end filter, ordinary | 180 | 6.08 / 7.52 | 6.80 / 8.16 |
| path filter, hub | 20 | 7.05 / 7.96 | 7.46 / 7.86 |
| path filter, ordinary | 180 | 4.92 / 6.65 | 6.06 / 8.05 |
| range filter, hub | 20 | 7.36 / 9.84 | 8.04 / 10.55 |
| range filter, ordinary | 180 | 7.89 / 8.75 | 8.72 / 9.76 |
| shortest path, up to 6 | 200 | 0.060 / 0.668 | 0.42 / 1.09 |
| 1,000-link chain from a hub | 5 | 0.338 / 0.413 | 1.74 / 1.94 |
| 1,000-link chain from an ordinary user | 5 | 0.312 / 0.317 | 1.84 / 2.25 |
| no path, up to 1,100 | 5 | 0.014 / 0.039 | 0.29 / 0.32 |

### L: 7.3 M nodes, 45 M links

| Query | Starts | Engine | Round trip |
|---|---|---|---|
| within 3 steps, hub | 20 | 20.10 / 25.02 | 21.43 / 25.92 |
| within 3 steps, ordinary | 180 | 0.018 / 16.02 | 0.43 / 17.15 |
| within 5 steps, hub | 20 | 49.81 / 55.03 | 49.43 / 52.08 |
| within 5 steps, ordinary | 180 | 0.018 / 48.98 | 0.65 / 50.01 |
| reaching this one within 5 steps, hub | 20 | 39.76 / 50.75 | 39.00 / 51.96 |
| reaching this one within 5 steps, ordinary | 180 | 2.86 / 51.10 | 3.41 / 51.88 |
| end filter, hub | 20 | 24.32 / 26.68 | 24.11 / 27.75 |
| end filter, ordinary | 180 | 24.13 / 28.63 | 24.68 / 26.92 |
| path filter, hub | 20 | 16.38 / 17.22 | 17.54 / 17.90 |
| path filter, ordinary | 180 | 10.78 / 19.19 | 11.65 / 19.99 |
| range filter, hub | 20 | 30.95 / 34.23 | 31.77 / 32.76 |
| range filter, ordinary | 180 | 30.71 / 33.11 | 32.12 / 33.93 |
| shortest path, up to 6 | 200 | 0.088 / 1.34 | 0.66 / 1.92 |
| 1,000-link chain from a hub | 5 | 0.534 / 0.927 | 2.90 / 3.81 |
| 1,000-link chain from an ordinary user | 5 | 0.389 / 0.423 | 3.04 / 3.36 |
| no path, up to 1,100 | 5 | 0.022 / 0.067 | 0.37 / 0.43 |

The engine and round-trip columns are medians of separate runs, so at L a round trip can come out slightly below the
engine time.


## Load and memory

| | S | M | L |
|---|---|---|---|
| Load from Parquet | 0.62 s | 2.50 s | 13.78 s |
| Ready after `docker run` | 1.0 s | 2.8 s | 13.9 s |
| Container memory right after loading | 0.26 GB | 0.96 GB | 3.55 GB |
| Container memory once settled | 0.22 GB | 0.52 GB | 1.64 GB |

Memory peaks after loading because the load buffers are freed back to the system over the next seconds. The datasets
reach the container through Docker Desktop's file sharing from macOS, which makes loading slower than on Linux.


## Throughput

Queries per second with several clients at once, each on its own connection, 10 seconds per cell, round trips.

| Query | Size | 1 client | 8 clients | 32 clients |
|---|---|---|---|---|
| path filter | S | 489 | 980 | 988 |
| path filter | M | 166 | 282 | 283 |
| path filter | L | 101 | 163 | 166 |
| shortest path, up to 6 | S | 2,045 | 8,790 | 10,589 |
| shortest path, up to 6 | M | 1,696 | 7,881 | 9,706 |
| shortest path, up to 6 | L | 1,410 | 6,684 | 8,405 |

A walk already runs on every core, so filtered walks gain little from more clients. Shortest paths are short searches
on one core each, so they scale with clients until the 6 CPUs are busy.


## Reading the numbers

- **Ordinary starts are often instant.** Many ordinary starts have no links leading out, so their walk ends at once;
  the medians of 0.005 to 0.018 ms are those. The 95th percentile shows the ordinary starts that do fan out.
- **Filters on the way are cheaper than filters at the end.** The path filter never steps onto inactive users, so it
  visits fewer nodes. The end and range filters walk all 4 steps and check each node reached.
- **The round trip of a long path is mostly its answer.** A shortest path returns its nodes and links to draw, about
  1,000 of each for the chains.
- **Through a Unix socket** from another container, a round trip is about 0.15 ms shorter than these, which go
  through Docker Desktop's port forwarding.


## Not covered

| Case | Why |
|---|---|
| shortest loop back to a node | not expressible in ProximaDB's Cypher; `POST /path` answers it |
| every node's distance to the nearest banned user | a whole-graph job, not a Cypher query |
| writes and reloads during reads | measured in process, not yet in this setup ([How it works](how-it-works.md#9-numbers)) |
| Linux on x86 | this run is Docker Desktop on an ARM Mac |

The harness that generates the data, the reference answers and runs these queries is kept outside this repository for
now.
