# moa-retrieval

Graph-memory retrieval engine for MOA. This crate owns the memory-retrieval
pipeline and the query planner that drives it: hybrid vector/lexical/graph
retrieval, ranking and fusion, evidence-window admission, enrichment, and the
strategy/entity planning that seeds each retrieval. It is the self-contained
bottom layer of the context pipeline, split out of `moa-brain` so that
consumers which only need retrieval and planning types (eval, orchestrator,
loadtest, xtask) do not rebuild the full brain crate.

## Modules

The two modules form a cooperating pair: planning classifies a query and seeds
it, and retrieval executes the planned query.

- `planning` — query planning for graph-memory retrieval.
- `retrieval` — graph-memory retrieval for context assembly and planning.
