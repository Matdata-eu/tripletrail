# tiny_http, patched for TripleTrail

This is [tiny_http](https://crates.io/crates/tiny_http) 0.12.0 (MIT OR Apache-2.0, by
Pierre Krieger, Corey Farwell and contributors; used here under the MIT license, see
`LICENSE-MIT`), used through `[patch.crates-io]` in the workspace. Every change is marked with
a `tripletrail (#…)` comment.

Issue #549: when a response's body fails to read half way (a streamed
SPARQL result whose query fails or times out after the headers were sent), tiny_http still
ended a chunked body with its last, empty chunk and kept the connection open. A client then
took the cut body for a whole one, and a pooled client could read the next response on that
connection out of step. Now:

- `src/response.rs`: the chunked encoder writes through a gate that is shut once the body
  has failed, so the last chunk it writes when dropped goes nowhere (nothing is leaked).
- `src/request.rs`, `src/client.rs`, `src/util/refined_tcp_stream.rs`: a response that failed
  half way shuts the connection down both ways, so the client sees a cut response and sends
  no further request on it.

Issue #690: `src/util/task_pool.rs`, `TaskPool::spawn` added a thread only when no thread was
waiting. Threads woken for queued connections still count as waiting until they take the
queue's lock, which `spawn` holds, so a burst of keep-alive connections was queued behind the
first four, each served until it closed (with a pooled client, never). Now a thread is added
unless more threads wait than tasks are queued. Upstream has the same code.
