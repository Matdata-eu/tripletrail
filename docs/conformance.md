# Conformance

TripleTrail's results on the W3C SPARQL, RDF and SHACL test suites and the GeoSPARQL Compliance Benchmark, run against TripleTrail's own store, planner, operators, geometry functions and SHACL validator (not upstream Oxigraph). The SPARQL and RDF tests come from [w3c/rdf-tests](https://github.com/w3c/rdf-tests) at commit [`202a74eb81f4`](https://github.com/w3c/rdf-tests/tree/202a74eb81f47bcad13bbafdd37eba69c361c1c2), the SHACL tests from [w3c/data-shapes](https://github.com/w3c/data-shapes) at commit [`37aca10843ae`](https://github.com/w3c/data-shapes/tree/37aca10843ae370ddb7db515957ae3873e7f560a/data-shapes-test-suite/tests), and the GeoSPARQL benchmark from [OpenLinkSoftware/GeoSPARQLBenchmark](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark) at commit [`879e0746e74c`](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/tree/879e0746e74c1327f74e57366104bcf1a6d63fb1). CI runs them on every pull request and fails on any change against [`known-failures.txt`](../crates/tripletrail-testsuite/known-failures.txt).

To run them yourself, and to update this page and the known failures after a fix:

```sh
git clone https://github.com/w3c/rdf-tests
git -C rdf-tests checkout 202a74eb81f47bcad13bbafdd37eba69c361c1c2
git clone https://github.com/w3c/data-shapes
git -C data-shapes checkout 37aca10843ae370ddb7db515957ae3873e7f560a
git clone https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark
git -C GeoSPARQLBenchmark checkout 879e0746e74c1327f74e57366104bcf1a6d63fb1
cargo run -p tripletrail-testsuite -- --tests rdf-tests --shacl-tests data-shapes \
  --geosparql-tests GeoSPARQLBenchmark \
  --report docs/conformance.md \
  --write-known crates/tripletrail-testsuite/known-failures.txt --earl tripletrail-earl.ttl \
  --server target/debug/tripletrail   # after cargo build -p tripletrail-server
```

CI runs the commits in `RDF_TESTS_COMMIT`, `DATA_SHAPES_COMMIT` and `GEOSPARQL_BENCHMARK_COMMIT` of `.github/workflows/ci.yml`; bump them with the page and the known failures.

Each test runs on a new in-memory store. Results compare up to blank node renaming, and numeric literals compare by value (`"1.0"` and `"1"` are the same `xsd:decimal`), as in Jena's and Oxigraph's runners: the expected results come from several implementations that write numbers differently. Rows compare in order only when the query has `ORDER BY`. "Not run" counts tests the runner can't run yet (entailment regimes TripleTrail lacks, graphs named by server path, which Fuseki doesn't serve either) and tests the working group didn't approve; a pass rate counts only the tests that ran. The RDF suites load each file with `Engine::load`, the loader the server and the CLI use, and read the statements back from the store: they test the parsers and what the store keeps (literals exactly as written, language directions, triple terms).

The Protocol and Graph Store Protocol suites replay their HTTP requests against `tripletrail serve` (with `--server target/debug/tripletrail`), on one in-memory dataset emptied before each test; `/sparql/` maps to the dataset's `/ds/sparql` and `/gsp` to `/ds/data`. The Service Description suite reads the description at `/ds/sparql`. In the Federated Query suite, a `tripletrail serve` of its own stands in for each test's endpoints, a dataset per endpoint holding its `qt:data`; the endpoint IRIs in the test's files become that server's URLs, and the query runs in the store with SERVICE calls allowed to it.

The SHACL suites load each test's data and shapes graphs into the store and validate with `Engine::validate`, the validator of `tripletrail validate` and `tripletrail infer`. The report must be the expected one up to blank node renaming, with the blank nodes it reaches (paths, blank node shapes) and leaving out `sh:resultMessage`, as the suite asks; a test that expects `sht:Failure` passes when validation fails. Test IRIs are those of the W3C SHACL implementation report (`urn:x-shacl-test:/…`).

The [GeoSPARQL Compliance Benchmark](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark) (Jovanovik, Homburg and Spasić, [2021](https://www.mdpi.com/2220-9964/10/7/487)) has 206 queries on one dataset that cover the requirements of GeoSPARQL 1.0, and is what GeoSPARQL support of RDF stores is usually measured with; OGC's executable test suite for GeoSPARQL 1.1 tests only the core so far. The dataset loads into the default graph, and a query passes when its answer is the expected one or one of the benchmark's alternatives. As in the benchmark, WKT literals compare without white space and case; GML, KML and GeoJSON literals compare without the white space between elements. The benchmark's own score, the compliance percentage, weighs each of the 30 requirements equally and shares a requirement's weight among its queries.

The Entailment Regimes suite answers a test under the regime it lists that TripleTrail has the most rules of: OWL 2 RDF-Based semantics as the OWL 2 RL rules (`tripletrail entail --regime owl-rl`), else RDFS as Jena's default RDFS rules (`--regime rdfs`), which leave out the axiomatic and reflexive triples. The default graph's entailment is materialized and added to it before the query runs. Tests under only simple, D, RDF, OWL 2 Direct or RIF entailment are not run.

The RDF 1.1 semantics tests load the action and, under RDFS, close it with the same RDFS rules. The runner then checks the entailment: the result graph, its blank nodes as variables, maps into the closure, with literals of a recognized datatype compared by value; a graph is inconsistent when it holds an ill-typed literal of a recognized datatype or, under RDFS, a literal outside a recognized datatype in the range of its property. Simple and RDF entailment add no rules.

| Suite | Passed | Failed | Not run | Pass rate |
| --- | ---: | ---: | ---: | ---: |
| SPARQL 1.0 Query | 487 | 0 | 0 | 100.0 % |
| SPARQL 1.1 Query | 353 | 0 | 0 | 100.0 % |
| SPARQL 1.1 Update | 157 | 0 | 0 | 100.0 % |
| SPARQL 1.1 Query Results CSV, TSV and JSON | 11 | 0 | 0 | 100.0 % |
| SPARQL 1.1 Federated Query | 10 | 0 | 0 | 100.0 % |
| SPARQL 1.1 Protocol | 34 | 0 | 0 | 100.0 % |
| SPARQL 1.1 Graph Store Protocol | 8 | 1 | 4 | 88.9 % |
| SPARQL 1.1 Service Description | 3 | 0 | 0 | 100.0 % |
| SPARQL 1.1 Entailment Regimes | 39 | 5 | 26 | 88.6 % |
| SPARQL 1.2 | 271 | 0 | 0 | 100.0 % |
| RDF 1.1 N-Triples | 70 | 0 | 0 | 100.0 % |
| RDF 1.1 N-Quads | 87 | 0 | 0 | 100.0 % |
| RDF 1.1 Turtle | 313 | 0 | 0 | 100.0 % |
| RDF 1.1 TriG | 357 | 0 | 0 | 100.0 % |
| RDF 1.1 RDF/XML | 164 | 2 | 0 | 98.8 % |
| RDF 1.1 Semantics | 36 | 3 | 9 | 92.3 % |
| RDF 1.2 N-Triples | 74 | 1 | 0 | 98.7 % |
| RDF 1.2 N-Quads | 72 | 0 | 0 | 100.0 % |
| RDF 1.2 Turtle | 109 | 0 | 0 | 100.0 % |
| RDF 1.2 TriG | 64 | 0 | 0 | 100.0 % |
| RDF 1.2 RDF/XML | 29 | 2 | 0 | 93.5 % |
| SHACL Core | 98 | 0 | 0 | 100.0 % |
| SHACL-SPARQL | 22 | 0 | 0 | 100.0 % |
| GeoSPARQL 1.0 Compliance Benchmark | 148 | 58 | 0 | 71.8 % |
| **All** | **3016** | **72** | **39** | **97.7 %** |

## SPARQL 1.0 Query

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| Basic | 28 | 0 | 0 |
| Triple Match | 4 | 0 | 0 |
| open world value testing tests | 18 | 0 | 0 |
| Algebra | 14 | 0 | 0 |
| bnode co-reference | 1 | 0 | 0 |
| OPTIONAL | 7 | 0 | 0 |
| OPTIONAL FILTER | 5 | 0 | 0 |
| GRAPH | 21 | 0 | 0 |
| dataset | 12 | 0 | 0 |
| Type Promotion | 30 | 0 | 0 |
| Casting | 7 | 0 | 0 |
| Boolean Effective Value | 7 | 0 | 0 |
| bound | 1 | 0 | 0 |
| Built-ins | 25 | 0 | 0 |
| XPath operators | 18 | 0 | 0 |
| equality of values | 15 | 0 | 0 |
| REGEX | 21 | 0 | 0 |
| I18N | 5 | 0 | 0 |
| CONSTRUCT | 5 | 0 | 0 |
| ASK | 4 | 0 | 0 |
| DISTINCT | 11 | 0 | 0 |
| SORT | 14 | 0 | 0 |
| Solution Sequence | 13 | 0 | 0 |
| REDUCED | 2 | 0 | 0 |
| Syntax 1 | 81 | 0 | 0 |
| Syntax 2 | 53 | 0 | 0 |
| Syntax 3 | 51 | 0 | 0 |
| Syntax 4 | 12 | 0 | 0 |
| Syntax 5 | 2 | 0 | 0 |

## SPARQL 1.1 Query

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| Aggregates | 47 | 0 | 0 |
| BIND | 10 | 0 | 0 |
| SPARQL 1.1 Query Language | 30 | 0 | 0 |
| Casting | 6 | 0 | 0 |
| CONSTRUCT | 7 | 0 | 0 |
| Positive Exists | 6 | 0 | 0 |
| Built-in Functions | 76 | 0 | 0 |
| Grouping | 6 | 0 | 0 |
| Negation | 12 | 0 | 0 |
| Project Expression | 7 | 0 | 0 |
| Property Path | 33 | 0 | 0 |
| Sub query | 16 | 0 | 0 |
| Syntax Query | 97 | 0 | 0 |

## SPARQL 1.1 Update

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| Add | 8 | 0 | 0 |
| Basic Update | 13 | 0 | 0 |
| CLEAR | 4 | 0 | 0 |
| Copy | 6 | 0 | 0 |
| DELETE DATA | 6 | 0 | 0 |
| DELETE INSERT | 17 | 0 | 0 |
| DELETE WHERE | 6 | 0 | 0 |
| DELETE | 19 | 0 | 0 |
| DROP | 4 | 0 | 0 |
| Move | 6 | 0 | 0 |
| Syntax Update 1 | 54 | 0 | 0 |
| Syntax Update 2 | 1 | 0 | 0 |
| SPARQL 1.1 Update test cases for SILENT | 13 | 0 | 0 |

## SPARQL 1.1 Query Results CSV, TSV and JSON

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| CSV/TSV Result Format | 7 | 0 | 0 |
| JSON Result Format | 4 | 0 | 0 |

## SPARQL 1.1 Federated Query

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| Syntax Federation | 3 | 0 | 0 |
| SPARQL Service | 7 | 0 | 0 |

## SPARQL 1.1 Protocol

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| SPARQL Protocol | 34 | 0 | 0 |

## SPARQL 1.1 Graph Store Protocol

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| SPARQL Graph Store Protocol - Direct Graph Identification | 0 | 0 | 4 |
| SPARQL Graph Store Protocol - Indirect Graph Identification | 8 | 1 | 0 |

### Failures

- [post_get_new_graph: Graph creation via POST on the GSP endpoint, then GET of the resulting graph (di…](http://www.w3.org/2009/sparql/docs/tests/data-sparql11/http-rdf-update/manifest#post_get_new_graph) (SPARQL Graph Store Protocol - Indirect Graph Identification): request 1 (POST /gsp): status 204, expected Created

### Not run

- 4 × direct graph identification (graph IRIs as server paths): not served, as in Fuseki

## SPARQL 1.1 Service Description

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| SPARQL Service Description | 3 | 0 | 0 |

## SPARQL 1.1 Entailment Regimes

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| entailment regime test cases | 39 | 5 | 26 |

### Failures

- [paper-sparqldl-Q1-rdfs: paper-sparqldl-Q1-rdfs](https://github.com/w3c/rdf-tests/blob/main/sparql/sparql11/entailment/paper-sparqldl-Q1.rq) (entailment regime test cases): expected 2 rows, got 1; missing `?c=<http://example.org/Student>`
- [rdfs11: RDFS inference test subProperty and instances](https://github.com/w3c/rdf-tests/blob/main/sparql/sparql11/entailment/rdfs11.rq) (entailment regime test cases): expected 2 rows, got 1; missing `?x=<http://example.org/ns#p>`
- [sparqldl-10: sparqldl-10.rq: undist vars test](https://github.com/w3c/rdf-tests/blob/main/sparql/sparql11/entailment/sparqldl-10.rq) (entailment regime test cases): expected 3 rows, got 0; missing `?X=<http://example.org/test#a> ?Y=<http://example.org/test#b>`
- [sparqldl-11: sparqldl-11.rq: domain test](https://github.com/w3c/rdf-tests/blob/main/sparql/sparql11/entailment/sparqldl-11.rq) (entailment regime test cases): expected 2 rows, got 0; missing `?C=<foo://bla/names#Parent>`, `?C=<http://www.w3.org/2002/07/owl#Thing>`
- [sparqldl-12: sparqldl-12.rq: range test](https://github.com/w3c/rdf-tests/blob/main/sparql/sparql11/entailment/sparqldl-12.rq) (entailment regime test cases): expected 2 rows, got 3; unexpected `?C=_:b`

### Not run

- 1 × entailment regime D (TripleTrail has RDFS and OWL 2 RL)
- 18 × entailment regime OWL-Direct (TripleTrail has RDFS and OWL 2 RL)
- 3 × entailment regime RDF (TripleTrail has RDFS and OWL 2 RL)
- 4 × entailment regime RIF (TripleTrail has RDFS and OWL 2 RL)

## SPARQL 1.2

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| Grouping | 2 | 0 | 0 |
| SPARQL Codepoint-Escaping Tests | 14 | 0 | 0 |
| Negative Syntax Triple Terms | 65 | 0 | 0 |
| Positive Syntax Triple Terms | 113 | 0 | 0 |
| Evaluation Triple Terms | 41 | 0 | 0 |
| Expression | 5 | 0 | 0 |
| Language and base direction | 13 | 0 | 0 |
| RDF 1.1 literals handling | 3 | 0 | 0 |
| Version announcement | 9 | 0 | 0 |
| Syntax | 6 | 0 | 0 |

## RDF 1.1 N-Triples

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| N-Triples tests | 70 | 0 | 0 |

## RDF 1.1 N-Quads

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| N-Quads tests | 87 | 0 | 0 |

## RDF 1.1 Turtle

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| Turtle tests | 313 | 0 | 0 |

## RDF 1.1 TriG

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| TriG tests | 357 | 0 | 0 |

## RDF 1.1 RDF/XML

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| RDF/XML Syntax tests | 164 | 2 | 0 |

### Failures

- [xml-canon-test001: xml-canon-test001](https://github.com/w3c/rdf-tests/blob/main/rdf/rdf11/rdf-xml/xml-canon/test001.rdf) (RDF/XML Syntax tests): expected 1 quads, got 1; missing `<http://www.example.org/a> <http://example.org/prop> "<br></br>"^^<http://www.w3.org/1999/02/22-rdf-syntax-ns#XMLLiteral>`; unexpected `<http://www.example.org/a> <http://example.org/prop> "<br xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:eg=\"htt…
- [xml-canon-test002: xml-canon-test002](https://github.com/w3c/rdf-tests/blob/main/rdf/rdf11/rdf-xml/xml-canon/test002.rdf) (RDF/XML Syntax tests): expected 5 quads, got 5; missing `<http://example.com/#reif> <http://www.w3.org/1999/02/22-rdf-syntax-ns#object> "<br></br>"^^<http://www.w3.org/1999/02/22-rdf-syntax-ns#XMLLiteral>`, `<http://www.example.org/a> <http://example.org/prop> "<br></br>"^^<http://www.w3.org/1999/02/22-rdf-syntax-ns#XMLLi…

## RDF 1.1 Semantics

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| RDF Schema and Semantics tests | 36 | 3 | 9 |

### Failures

- [rdfms-seq-representation-test002: rdfms-seq-representation-test002](https://github.com/w3c/rdf-tests/blob/main/rdf/rdf11/rdf-mt/rdfms-seq-representation/empty.nt) (RDF Schema and Semantics tests): https://w3c.github.io/rdf-tests/rdf/rdf11/rdf-mt/rdfms-seq-representation/test002.nt is not entailed, but it should be
- [rdfms-seq-representation-test003: rdfms-seq-representation-test003](https://github.com/w3c/rdf-tests/blob/main/rdf/rdf11/rdf-mt/rdfms-seq-representation/test003a.nt) (RDF Schema and Semantics tests): https://w3c.github.io/rdf-tests/rdf/rdf11/rdf-mt/rdfms-seq-representation/test003b.nt is not entailed, but it should be
- [rdfms-seq-representation-test004: rdfms-seq-representation-test004](https://github.com/w3c/rdf-tests/blob/main/rdf/rdf11/rdf-mt/rdfms-seq-representation/empty.nt) (RDF Schema and Semantics tests): https://w3c.github.io/rdf-tests/rdf/rdf11/rdf-mt/rdfms-seq-representation/test004.nt is not entailed, but it should be

### Not run

- 9 × not approved (NotClassified)

## RDF 1.2 N-Triples

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| RDF 1.2 N-Triples Canonicalization Test Suite | 41 | 0 | 0 |
| RDF 1.2 N-Triples Syntax Tests | 33 | 1 | 0 |

### Failures

- [ntriples-version-1: N-Triples with version announcement](https://github.com/w3c/rdf-tests/blob/main/rdf/rdf12/rdf-n-triples/syntax/ntriples-version-1.nt) (RDF 1.2 N-Triples Syntax Tests): refused: Parser error at line 1 between columns 1 and 8: The subject of a triple must be an IRI or a blank node

## RDF 1.2 N-Quads

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| RDF 1.2 N-Quads Canonicalization Test Suite | 41 | 0 | 0 |
| RDF 1.2 N-Quads Syntax Tests | 31 | 0 | 0 |

## RDF 1.2 Turtle

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| RDF 1.2 Turtle Evaluation Tests | 32 | 0 | 0 |
| RDF 1.2 Turtle Syntax Tests | 77 | 0 | 0 |

## RDF 1.2 TriG

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| RDF 1.2 TriG Evaluation Tests | 26 | 0 | 0 |
| RDF 1.2 TriG Syntax Tests | 38 | 0 | 0 |

## RDF 1.2 RDF/XML

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| RDF 1.2 XML Evaluation Tests | 29 | 2 | 0 |

### Failures

- [rdf12-xml-an-13: rdf12-xml-an-13](https://github.com/w3c/rdf-tests/blob/main/rdf/rdf12/rdf-xml/eval/rdf12-xml-an-13.rdf) (RDF 1.2 XML Evaluation Tests): expected 2 quads, got 2; missing `<http://example.com/triple1> <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( <http://www.example.org/a> <http://example.org/prop> "<br></br>"^^<http://www.w3.org/1999/02/22-rdf-syntax-ns#XMLLiteral> )>>`, `<http://www.example.org/a> <http://example.org/prop…
- [rdf12-xml-an-14: rdf12-xml-an-14](https://github.com/w3c/rdf-tests/blob/main/rdf/rdf12/rdf-xml/eval/rdf12-xml-an-14.rdf) (RDF 1.2 XML Evaluation Tests): expected 2 quads, got 2; missing `<http://www.example.org/a> <http://example.org/prop> "<br></br>"^^<http://www.w3.org/1999/02/22-rdf-syntax-ns#XMLLiteral>`, `_:b <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> <<( <http://www.example.org/a> <http://example.org/prop> "<br></br>"^^<http://www.w3…

## SHACL Core

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| core/complex | 2 | 0 | 0 |
| core/misc | 5 | 0 | 0 |
| core/node | 32 | 0 | 0 |
| core/path | 13 | 0 | 0 |
| core/property | 38 | 0 | 0 |
| core/targets | 7 | 0 | 0 |
| core/validation-reports | 1 | 0 | 0 |

## SHACL-SPARQL

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| sparql/component | 3 | 0 | 0 |
| sparql/node | 4 | 0 | 0 |
| sparql/pre-binding | 14 | 0 | 0 |
| sparql/property | 1 | 0 | 0 |

## GeoSPARQL 1.0 Compliance Benchmark

GeoSPARQL compliance percentage (the benchmark's weighting): **75.97 %**

| Tests | Passed | Failed | Not run |
| --- | ---: | ---: | ---: |
| R1 core/sparql-protocol | 1 | 0 | 0 |
| R2 core/spatial-object-class | 1 | 0 | 0 |
| R3 core/feature-class | 1 | 0 | 0 |
| R4 topology-vocab-extension/sf-spatial-relations | 2 | 6 | 0 |
| R5 topology-vocab-extension/eh-spatial-relations | 1 | 7 | 0 |
| R6 topology-vocab-extension/rcc8-spatial-relations | 2 | 6 | 0 |
| R7 geometry-extension/geometry-class | 1 | 0 | 0 |
| R8 geometry-extension/feature-properties | 2 | 0 | 0 |
| R9 geometry-extension/geometry-properties | 6 | 0 | 0 |
| R10 geometry-extension/wkt-literal | 1 | 0 | 0 |
| R11 geometry-extension/wkt-literal-default-srs | 1 | 0 | 0 |
| R12 geometry-extension/wkt-axis-order | 1 | 0 | 0 |
| R13 geometry-extension/wkt-literal-empty | 2 | 0 | 0 |
| R14 geometry-extension/geometry-as-wkt-literal | 1 | 0 | 0 |
| R15 geometry-extension/gml-literal | 1 | 0 | 0 |
| R16 geometry-extension/gml-literal-empty | 2 | 0 | 0 |
| R18 geometry-extension/geometry-as-gml-literal | 1 | 0 | 0 |
| R19 geometry-extension/query-functions | 3 | 25 | 0 |
| R20 geometry-extension/srid-function | 2 | 0 | 0 |
| R21 geometry-topology-extension/relate-query-function | 4 | 0 | 0 |
| R22 geometry-topology-extension/sf-query-functions | 32 | 0 | 0 |
| R23 geometry-topology-extension/eh-query-functions | 32 | 0 | 0 |
| R24 geometry-topology-extension/rcc8-query-functions | 32 | 0 | 0 |
| R25 rdfs-entailment-extension/bgp-rdfs-ent | 0 | 3 | 0 |
| R26 rdfs-entailment-extension/wkt-geometry-types | 0 | 2 | 0 |
| R27 rdfs-entailment-extension/gml-geometry-types | 0 | 1 | 0 |
| R28 query-rewrite-extension/sf-query-rewrite | 7 | 1 | 0 |
| R29 query-rewrite-extension/eh-query-rewrite | 6 | 2 | 0 |
| R30 query-rewrite-extension/rcc8-query-rewrite | 3 | 5 | 0 |

### Failures

- [query-r04-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r04-1.rq) (R4 topology-vocab-extension/sf-spatial-relations): expected 1 rows, got 2
- [query-r04-3.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r04-3.rq) (R4 topology-vocab-extension/sf-spatial-relations): expected 1 rows, got 1
- [query-r04-4.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r04-4.rq) (R4 topology-vocab-extension/sf-spatial-relations): expected 1 rows, got 3
- [query-r04-6.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r04-6.rq) (R4 topology-vocab-extension/sf-spatial-relations): expected 1 rows, got 3
- [query-r04-7.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r04-7.rq) (R4 topology-vocab-extension/sf-spatial-relations): expected 1 rows, got 1
- [query-r04-8.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r04-8.rq) (R4 topology-vocab-extension/sf-spatial-relations): expected 1 rows, got 2
- [query-r05-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r05-1.rq) (R5 topology-vocab-extension/eh-spatial-relations): expected 1 rows, got 2
- [query-r05-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r05-2.rq) (R5 topology-vocab-extension/eh-spatial-relations): expected 1 rows, got 23
- [query-r05-3.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r05-3.rq) (R5 topology-vocab-extension/eh-spatial-relations): expected 1 rows, got 3
- [query-r05-4.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r05-4.rq) (R5 topology-vocab-extension/eh-spatial-relations): expected 1 rows, got 4
- [query-r05-6.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r05-6.rq) (R5 topology-vocab-extension/eh-spatial-relations): expected 1 rows, got 2
- [query-r05-7.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r05-7.rq) (R5 topology-vocab-extension/eh-spatial-relations): expected 1 rows, got 2
- [query-r05-8.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r05-8.rq) (R5 topology-vocab-extension/eh-spatial-relations): expected 1 rows, got 1
- [query-r06-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r06-1.rq) (R6 topology-vocab-extension/rcc8-spatial-relations): expected 1 rows, got 2
- [query-r06-3.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r06-3.rq) (R6 topology-vocab-extension/rcc8-spatial-relations): expected 1 rows, got 2
- [query-r06-4.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r06-4.rq) (R6 topology-vocab-extension/rcc8-spatial-relations): expected 1 rows, got 2
- [query-r06-6.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r06-6.rq) (R6 topology-vocab-extension/rcc8-spatial-relations): expected 1 rows, got 2
- [query-r06-7.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r06-7.rq) (R6 topology-vocab-extension/rcc8-spatial-relations): expected 1 rows, got 2
- [query-r06-8.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r06-8.rq) (R6 topology-vocab-extension/rcc8-spatial-relations): expected 1 rows, got 2
- [query-r19-1-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-1-1.rq) (R19 geometry-extension/query-functions): expected 3 rows, got 3
- [query-r19-1-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-1-2.rq) (R19 geometry-extension/query-functions): expected 3 rows, got 3
- [query-r19-1-3.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-1-3.rq) (R19 geometry-extension/query-functions): expected 3 rows, got 3
- [query-r19-1-4.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-1-4.rq) (R19 geometry-extension/query-functions): expected 3 rows, got 3
- [query-r19-2-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-2-1.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-2-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-2-2.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-4-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-4-1.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-4-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-4-2.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-4-3.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-4-3.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-4-4.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-4-4.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-5-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-5-1.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-5-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-5-2.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-5-3.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-5-3.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-5-4.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-5-4.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-6-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-6-1.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-6-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-6-2.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-6-3.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-6-3.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-6-4.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-6-4.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-7-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-7-1.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-7-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-7-2.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-7-3.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-7-3.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-7-4.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-7-4.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-8-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-8-1.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-8-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-8-2.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r19-9-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r19-9-2.rq) (R19 geometry-extension/query-functions): expected 1 rows, got 1
- [query-r25-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r25-1.rq) (R25 rdfs-entailment-extension/bgp-rdfs-ent): expected 13 rows, got 12
- [query-r25-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r25-2.rq) (R25 rdfs-entailment-extension/bgp-rdfs-ent): expected 13 rows, got 12
- [query-r25-3.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r25-3.rq) (R25 rdfs-entailment-extension/bgp-rdfs-ent): expected 13 rows, got 12
- [query-r26-1.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r26-1.rq) (R26 rdfs-entailment-extension/wkt-geometry-types): expected 7 rows, got 0
- [query-r26-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r26-2.rq) (R26 rdfs-entailment-extension/wkt-geometry-types): expected 3 rows, got 0
- [query-r27.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r27.rq) (R27 rdfs-entailment-extension/gml-geometry-types): expected 7 rows, got 0
- [query-r28-3.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r28-3.rq) (R28 query-rewrite-extension/sf-query-rewrite): expected 12 rows, got 18
- [query-r29-5.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r29-5.rq) (R29 query-rewrite-extension/eh-query-rewrite): expected 4 rows, got 2
- [query-r29-6.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r29-6.rq) (R29 query-rewrite-extension/eh-query-rewrite): expected 2 rows, got 0
- [query-r30-2.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r30-2.rq) (R30 query-rewrite-extension/rcc8-query-rewrite): expected 8 rows, got 10
- [query-r30-5.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r30-5.rq) (R30 query-rewrite-extension/rcc8-query-rewrite): expected 8 rows, got 2
- [query-r30-6.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r30-6.rq) (R30 query-rewrite-extension/rcc8-query-rewrite): expected 8 rows, got 2
- [query-r30-7.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r30-7.rq) (R30 query-rewrite-extension/rcc8-query-rewrite): expected 4 rows, got 2
- [query-r30-8.rq](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/gsb_queries/query-r30-8.rq) (R30 query-rewrite-extension/rcc8-query-rewrite): expected 4 rows, got 2

