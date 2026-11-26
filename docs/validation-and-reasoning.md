# Validation and reasoning

TripleTrail checks data against SHACL shapes, runs SHACL-AF rules, and derives what RDFS and
OWL 2 RL entail. Each works from the command line on files or on an on-disk dataset, from
Rust and Python, in a [pipeline](server.md#pipelines), and in the
[Workbench](server.md#workbench), where shapes and rules are developed against sample data.

## SHACL validation

`engine.validate(&shapes_graph, data_graph, &mut sink)` validates a graph of the store against
the shapes of a named graph and gives the W3C validation report as triples, as they are made, so
a report with a million results is never held in memory; it returns `sh:conforms`.
`engine.validate_results` gives the results as values instead. There is one `sh:ValidationResult` per
failing constraint (per failing value node for constraints on each value), with `sh:focusNode`,
`sh:resultPath`, `sh:value`, `sh:sourceShape` (the property shape for property constraints),
`sh:sourceConstraintComponent`, `sh:resultSeverity` (the shape's `sh:severity`) and
`sh:resultMessage` (the shape's `sh:message`s, or those of the SPARQL constraint; for a
SPARQL-based result, each `{$var}` and `{?var}` is filled in from the solution and the
component's parameters, as SHACL 5.3.2 and 6.4 say: an IRI as the IRI, a literal as its
lexical form, an unbound variable left as written; solutions that make the same result, with
the same focus node, value, path and message, give it once, #91). Failures of
shapes reached through `sh:node`, `sh:not`, `sh:and`, `sh:or`, `sh:xone` or
`sh:qualifiedValueShape` are one result of that constraint, whose `sh:detail`s are the results
of the value node against the nested shapes it fails: the shape of `sh:node`, the members of
`sh:and`, `sh:or` and `sh:xone`, and the qualified shape when there are too few qualified values
(`sh:not` has none, as its shape conforms). Details nest as deep as the shapes, are not counted
as results, and stop at about 1,000 per focus node and targeted shape. The server's JSON results
(`/$/datasets/NAME/validate`) give them as `details`. SHACL-AF rules of the shapes graph run
first, in memory.

```sh
tripletrail validate --data kg.ttl --shapes shapes.ttl --report report.ttl   # in memory
tripletrail validate --location /data/tripletrail --dataset kg --shapes-graph https://example.com/shapes
```

The command exits with status 1 when the data does not conform (warnings included, as
`sh:conforms` is defined); the report goes to standard output as Turtle without `--report`.

Lookup data (#91), such as the vocabularies, code lists and ontologies that `sh:class`, paths
and SPARQL constraints read, can be read without being validated: `--lookup FILE` (repeatable;
loaded into the graph `urn:x-tripletrail:lookup`) with files, `--lookup-graph IRI` (repeatable)
with `--location`. The data and the lookups are then one data graph to the shapes, but focus
nodes come from the data only: class targets are the instances the data types (subclasses come
from both), `sh:targetSubjectsOf` and `sh:targetObjectsOf` the data's triples, SPARQL targets
the nodes the data has triples about, and `sh:targetNode` stays as written. In Rust,
`Engine::validate_report_with` and `Engine::validate_counted_with` take the lookup graphs;
lookups need a named data graph when the default graph is the union of the graphs.
From Python, `Engine.validate(shapes_graph, *, data_graph=None, report_graph=None)` returns a
report with `conforms`, `results` (dicts, ready for `pandas.DataFrame`) and `serialize()`.

For a non-conformance rate per shape, `--stats` (`statistics=True` in Python,
`Engine::validate_counted` in Rust) also counts the focus and value nodes each shape was
checked against, conforming shapes and targeted shapes with no focus nodes included. A shape
reached through `sh:property`, `sh:node`, `sh:and`, `sh:or`, ... counts the value nodes of
its parent, once per parent focus node, so `ttsh:focusNodes` counts a node reached from two
parents twice. `ttsh:distinctFocusNodes` counts each focus node once, however it was reached
and however many targets select it, and `ttsh:failingFocusNodes` the distinct focus nodes of
the results whose `sh:sourceShape` is the shape (those in `sh:detail`s included), so their
ratio is the shape's failure rate. Both are counted by a 64-bit hash of each node, 8 bytes a
node; two nodes count as one with a probability under one in a million below six million
nodes. The report gets one IRI node per shape (no blank nodes, for pipelines that
skolemise):

```turtle
[] a sh:ValidationReport ;
  ttsh:shapeStatistics <urn:tripletrail:shape-statistics:...:0> .
<urn:tripletrail:shape-statistics:...:0> ttsh:shape ex:PersonShape ;
  ttsh:focusNodes 1200 ; ttsh:valueNodes 1200 ;
  ttsh:distinctFocusNodes 1200 ; ttsh:failingFocusNodes 37 .
```

`ttsh:` is `http://w3id.org/tripletrail/shacl#`; in Python, `report.shape_counts` holds one dict
per shape (`shape`, `property`, `focus_nodes`, `value_nodes`, `distinct_focus_nodes`,
`failing_focus_nodes`); the server's JSON answer has `distinctFocusNodes` and
`failingFocusNodes`.

## SHACL-AF rules

SHACL-AF rules (`sh:SPARQLRule` with `$this` pre-bound, `sh:TripleRule` with node
expressions, `sh:condition`, SPARQL targets) also run on their own:
`engine.infer(&shapes_graph, data_graph, into, max_passes)` runs them and returns the
inferred triples that are not in the data, written to the graph `into` if given. Rules are
stratified by the predicates they write and read: each runs after the rules that feed it
(otherwise in `sh:order`), a rule that no rule of its group feeds runs once, and a group of
rules that feed each other runs pass after pass until one infers nothing new, rerunning only
the rules whose inputs changed.

Only triples that are not already in the data are inferred. For an export built from what the
rules construct, `--all-constructed` (`InferOptions::include_existing` with
`engine.infer_with`, `include_existing=True` in Python) returns every triple the rules
construct, once, as TopBraid's `shaclinfer` does.

```sh
tripletrail infer --data kg.ttl --shapes rules.ttl --output inferred.nt          # in memory
tripletrail infer --location /data/tripletrail --dataset kg --shapes-graph https://example.com/rules \
  --into https://example.com/inferred
```

From Python: `Engine.infer(shapes_graph, *, data_graph=None, into=None, max_iterations=64,
include_existing=False, ordered=False)` returns the triples, or their number when written to
`into`.

## User-defined functions

SHACL-AF `sh:SPARQLFunction`s declare functions as data (#235). A shapes graph may hold
them; its constraints and rules can then call them by their IRIs, as can any query or update
on an engine or server given them:

```turtle
ex:kmToM a sh:SPARQLFunction ;
    sh:parameter [ sh:path ex:km ; sh:datatype xsd:decimal ] ;
    sh:returnType xsd:decimal ;
    sh:select "SELECT ($km * 1000 AS ?m) WHERE {}" .

ex:labelOf a sh:SPARQLFunction ;
    sh:prefixes ex:prefixes ;
    sh:parameter [ sh:path ex:node ] ;
    sh:select "SELECT ?l WHERE { $node rdfs:label ?l }" .
```

```sparql
SELECT ?s (ex:kmToM(?km) AS ?m) (ex:labelOf(?s) AS ?label) WHERE { ?s ex:km ?km }
```

- A parameter's variable is the local name of its `sh:path`. Arguments are given in
  `sh:order` (else in the order of the local names). Optional parameters (`sh:optional
  true`) come last and may be left out.
- A call runs the `sh:select` body with the parameters pre-bound to the arguments, as SHACL
  pre-binds `$this`, and gives the first row's value of the first selected variable (unbound
  when there is no row). An `sh:ask` body gives `true` or `false`. A call with too few or too
  many arguments is unbound. `sh:returnType` is not checked.
- The body reads the store's default graph (all graphs with a union default graph), not the
  calling query's `FROM`, and sees every committed write.
- A body that is one expression using only the parameters, `SELECT (expr AS ?r) WHERE {}`, is
  inlined into the calling query: the call costs what the expression costs. An inlined call
  sees an unbound argument as unbound (`BOUND($x)` is false), where a call that runs its
  body gets no value. Queries are inlined; updates run the body.
- Functions may call one another but not themselves, directly or through others: such a
  shapes graph is refused, as is a function without an IRI or without a body.
- A caller with access to only some graphs or rows (graph grants, row rules) can call only
  inlined functions: a body that reads the data would read all of it, so such a call is
  refused with 403, as `SERVICE` is.
- A body stops with its query (a timeout, say), and the query then fails with the timeout
  rather than giving an unbound value.

`tripletrail validate` and `tripletrail infer` load the functions of the shapes they are given.
`tripletrail serve --functions functions.ttl` (`TRIPLETRAIL_FUNCTIONS`, comma-separated) lets the
queries and updates of every dataset call the functions of those files. In Python,
`engine.with_functions(turtle, "ttl")` (or `path=`) returns an engine on the same store that
can call them; in Rust, `Engine::with_functions_from(triples)`.

JavaScript or WASM bodies (Jena's `js:`, GraphDB's `jsfn:`) are not supported.

## RDFS entailment

`engine.entail_rdfs(into)` makes the named graph `into` hold what the RDFS rules derive
from the other graphs, in one transaction, and returns how many triples it holds, and how
many came in and left. The rules are those of Jena's default RDFS reasoner:
- `rdfs:domain` and `rdfs:range` types (a literal object gets no type);
- `rdfs:subPropertyOf` (the triple is repeated with the super property) and its closure;
- `rdfs:subClassOf` types and its closure.

The axiomatic triples, `rdf:type rdfs:Resource` and the reflexive subclass and subproperty
triples are left out. The rules run to their fixpoint: derived triples are read by the rules
again, so schema written with RDFS itself works (the `rdfs.ttl` axioms such as
`rdf:type rdfs:range rdfs:Class`, or a subproperty of `rdfs:subClassOf`). Triples already in
the data are not repeated, and the graph `into` is not read as data. Run it again after the
data changes: triples no longer derived leave the graph. So does anything else in `into`:
give it a graph of its own (`tripletrail entail` prints how many triples left).

The inferred graph mixes what every graph implies. A caller allowed to read it sees
consequences of graphs it may not read (a type derived from a triple in a restricted graph,
say): with [access control](server.md#dataset-access-with-policies), grant the inferred graph only to callers who may read all
graphs.

On 5 million quads with a class tree of 1,000 classes it derives 14.5 million triples in 10 s
on 4 cores, most of it writing them; running it again with nothing changed takes 1.6 s.

**Kept up to date.** `engine.set_rdfs_entailment(Some(graph))` (or `tripletrail serve --rdfs kg`,
`--rdfs kg=GRAPH`) entails once, then every write brings the graph up to date in its own
commit, so a query never sees data without its entailment:
- added data derives from the added triples only;
- removed data goes by delete and rederive: what the removed triples led to is set aside, and
  what another triple still leads to comes back;
- a change of the schema (`rdfs:subClassOf`, `rdfs:subPropertyOf`, `rdfs:domain`,
  `rdfs:range`), a write into the inferred graph itself, and a bulk load entail again.

Moved to another graph, the old one is emptied. Stopped (`None`), the graph stays as it is,
as data that no longer follows writes.

On 1.5 million quads with 9.3 million entailed triples, an update of a few triples takes
about 1 ms more than without; inserting 10,000 triples 330 ms and deleting them 570 ms in all.

```sh
tripletrail entail --location /data/tripletrail --dataset kg --into urn:tripletrail:inferred
tripletrail entail --data ontology.ttl --data kg.ttl --output entailed.nt   # in memory
```

## OWL 2 RL

```sh
tripletrail entail --regime owl-rl --data ontology.ttl --data kg.ttl --output entailed.nt
tripletrail entail --regime owl-rl --location /data/tripletrail --dataset kg --into urn:tripletrail:inferred
```

`--regime owl-rl` (`engine.entail(into, Regime::OwlRl)`, or `reason: owl-rl` in a pipeline)
adds the OWL 2 RL rules to the RDFS rules: `owl:equivalentClass` and
`owl:equivalentProperty` (cax-eqc, prp-eqp, scm-eqc1, scm-eqp1), `owl:inverseOf` and
`owl:SymmetricProperty` (prp-inv, prp-symp), `owl:TransitiveProperty` (prp-trp), the value
restrictions `owl:hasValue`, `owl:someValuesFrom` and `owl:allValuesFrom` on an `owl:onProperty`
(cls-hv1/2, cls-svf1/2, cls-avf, scm-hv, scm-svf1/2, scm-avf1/2), `owl:intersectionOf` and
`owl:unionOf` (cls-int1/2, cls-uni, scm-int, scm-uni) and `owl:propertyChainAxiom` (prp-spo2).
`owl:sameAs` between resources (eq-sym, eq-trans, eq-rep-s/p/o; not eq-ref, as in Jena),
`owl:FunctionalProperty` and `owl:InverseFunctionalProperty` (prp-fp, prp-ifp, between
distinct resources as in Jena). Lists are read from the data. Inside the fixpoint the rules
read generalized triples (a property chain on a blank node gives a triple with a blank
predicate, which other rules read), but only RDF triples are written. Also: a maximum
(qualified) cardinality of 1 (cls-maxc2, cls-maxqc3/4), `owl:hasKey` (prp-key),
`owl:oneOf` (cls-oo), and classes or properties that are sub classes (properties) of each
other become equivalent (scm-eqc2, scm-eqp2). A class typed `owl:Class` is its own sub
class and equivalent, a sub class of `owl:Thing`, and `owl:Nothing` is a sub class of it
(scm-cls; owl:Thing and owl:Nothing count as classes when the data declares one), so its
members are typed `owl:Thing`; a property typed `owl:ObjectProperty` or
`owl:DatatypeProperty` is its own sub property and equivalent (scm-op, scm-dp). A property's
`rdfs:domain` and `rdfs:range` are written with those of its super properties and their
super classes (scm-dom1/2, scm-rng1/2), which Jena's RDFS rules leave out.

**Kept up to date.** `engine.set_entailment(Some(graph), Regime::OwlRl)` (or `tripletrail serve
--owl-rl kg`, `--owl-rl kg=GRAPH`, or `entailment=owl-rl` in the admin API) keeps the OWL 2 RL
entailment as `--rdfs` keeps the RDFS one:
- a write that only adds data is entailed from the added triples, round by round with the
  schema as it is;
- a removal, a write that adds schema (a schema property, a property typed symmetric,
  transitive, functional or inverse functional, a class or property declaration, a list
  cell), a write that adds `owl:sameAs` or leads to it (through a functional property or a
  key, say), a write about a resource in an `owl:sameAs` class, a write into the inferred
  graph and a bulk load entail the whole dataset again.

OWL 2 RL has no delete and rederive here: on a large dataset, keep removals for batches.
Entailing into the kept graph under the other regime is refused.

The W3C SPARQL 1.1 Entailment Regimes tests run in CI on the materialized entailment: 39 of
the 44 RDFS and OWL 2 RDF-Based tests pass; see [conformance](conformance.md) for the five
that differ (the reflexive RDFS rules rdfs6 and rdfs10 and eq-ref are left out, as in Jena,
and two need more than OWL 2 RL).

### Consistency

The OWL 2 RL rules that conclude `false` (cax-dw, cax-adc, cls-com, cls-nothing2,
cls-maxc1, cls-maxqc1/2, prp-irp, prp-asyp, prp-pdw, prp-adp, prp-npa1/2, eq-diff1-3) write
nothing. As with Jena's validity report, entailment goes on with inconsistent data, and the
contradictions are listed after it:

- `tripletrail entail --regime owl-rl` prints how many there are, and the first 20, to
  standard error, for example `cax-dw: <h1> <Station> <Track>` (the resource and the two
  disjoint classes it has).
- A pipeline `reason: owl-rl` step reports them as `results: {inconsistencies: N}`. With
  `fail_on_inconsistency: true` it fails on one, with the entailment written.
- `engine.inconsistencies(limit)` returns the count and the first `limit`.

The checks read all graphs as they are, so they see what entailment wrote (a type through a
sub class, an `owl:sameAs` from a functional property). A contradiction counts once, however
many forms owl:sameAs rewriting wrote of it, and a disjointness stated both ways is one.
Lists (`owl:members`, `owl:distinctMembers`) are read outside the entailment graph, as the
data wrote them.

The count stops at a million (`entail::CAP`): each contradiction is kept to count it once,
and one resource with every class of a list of 6,000 disjoint classes alone makes 18
million. Past the cap the CLI says "at least 1000000", the pipeline step reports
`inconsistencies_at_least: 1000000` in place of `inconsistencies`, and
`fail_on_inconsistency` still fails the step.

**Timeouts.** A pipeline `reason:` step with a `timeout` stops when it passes, also while the
derived triples are merged and noted for writing, and leaves the graph as it was. The write
that follows is not stopped: once begun it finishes (on 36 million derived triples, about
15 s), so a timeout that passes during it, or during the OWL 2 RL consistency checks after
it, fails the step with the entailment written, and the error says so.

## Tests and pipelines

Regression tests for shapes, rules and queries are folders of a workspace's `tests/`: an input,
a target and the expected output (see [Tests](server.md#tests)). `tripletrail test` runs them
in CI, and `--record` writes the expected output from what the target gives now:

```sh
tripletrail test my-workspace --junit results.xml   # exit status 1 when a test fails
```

A whole ETL (lift, load, updates, rules, a validation gate, export) is one `pipeline.yaml` (see
[Pipelines](server.md#pipelines)), run headless with a JSON report of each step:

```sh
tripletrail pipeline run pipeline.yaml --location /data/tripletrail --dataset kg -v date=2026-01-01 --report run.json
```
