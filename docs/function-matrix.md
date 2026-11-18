# Function matrix: target state

This is the function-level matrix from the plan (`plan/01a-function-matrix.md` of 2026-10-20,
which is not in this repository),
rewritten for the state of TripleTrail once every open issue is fixed. It was written on `main`
at `fcefee5`; its marks were refreshed on 2026-11-07, when #53-#72 (except the tracker #62)
had closed, some with follow-up issues for what was left (#102-#110), on 2026-11-13,
when those follow-ups had closed too, and on 2026-11-14, when the pull requests #73-#75 had
long merged and #187 had closed. [architecture.md](architecture.md)
describes the same target state from the design side.

The plan's matrix was a wish list: what to build, with a priority. Almost all of it has been
built since, so this version says instead how each function **runs** in the target state, and
which issue changes that.

## Legend

| Mark | Meaning |
|---|---|
| ✅ | Done: on `main` today. |
| 📋 | Planned: an open issue describes the work, with no pull request yet. |
| ⬜ | Gap: in the plan, but neither built nor covered by an open issue. |

- **Kind**: S = scalar, T = table-valued (set-returning), A = aggregate, W = window-style
  (one value per input row), I = index-pushable (the planner can answer it from the spatial
  index or another index instead of testing every row).
- **Impl**: what it is built on today. `geo` = georust; `own` = written in this repository;
  `h3o` = H3. There is no GEOS, PROJ or SFCGAL dependency: the plan's GEOS/PROJ/SFCGAL entries
  were all written in pure Rust instead (see [architecture.md](architecture.md), D10).
- **IRIs**: `geof:` = `http://www.opengis.net/def/function/geosparql/`, `st:` =
  `http://w3id.org/tripletrail/st#`, `pgr:` = `http://w3id.org/tripletrail/pgr#`, `rail:` =
  `http://w3id.org/tripletrail/rail#`. A PostGIS function `ST_Foo` is `st:ST_Foo`; a pgRouting function
  `pgr_foo` is `pgr:function pgr:pgr_foo` in the `SERVICE <http://w3id.org/tripletrail/pgr>` block.
  Set-returning and window `ST_*` functions are `st:function st:ST_Foo` in
  `SERVICE <http://w3id.org/tripletrail/st>`.

## Summary

| Area | Functions in the plan | Done | Remaining |
|---|---:|---|---|
| GeoSPARQL 1.1 (properties, functions, rewrite) | ~120 | all functions and relation rewrites ✅ | virtual `hasCentroid`/`hasBoundingBox`/`hasVolume` ⬜; mixed-dimension set operations ✅ #73 |
| PostGIS `ST_*` | ~400 | every name in the plan ✅, including the P2 rows | speed of the relation, distance and length paths ✅ #73, ✅ #70; `st:version()` ⬜ |
| pgRouting `pgr_*` | ~110 | every function ✅, VRP included | components and cost matrix speed ✅ #72; persisted networks at start ✅ #69 |
| `rail:` beyond pgRouting | ~20 | all ✅ except the two below | `rail:materializeRelations` ⬜; profiles RTM, OSM railway, OpenTNF ⬜ |
| SHACL (not in the plan) | | Core, SPARQL, AF rules ✅ | validation report ✅ #59; CLI/Python ✅ #59 #61; per-focus rule planning ✅ #61; `sh:detail` ✅ #109 |
| Reasoning (not in the plan) | | RDFS ✅, OWL 2 RL with keys, cardinalities and consistency rules ✅ #187, both kept up to date with writes | query-time reasoning ⬜; see [validation-and-reasoning.md](validation-and-reasoning.md) |
| Full-text search (not in the plan) | | `tt:search`, Jena's `text:query` ✅ #188 | see [full-text-search.md](full-text-search.md) |
| Vector search (not in the plan) | | `tt:nearest`, `tt:cosine` ✅ #236 | see [vector-search.md](vector-search.md) |

Almost every function in the plan exists and the performance issues of the first benchmark
are closed. What is left is listed with 📋 below; the Fuseki replacement features (access
control, admin, Workbench, maps) are described in [server.md](server.md), not here.

## Open issues and what they change

| Issue | Title (short) | State | Changes in this matrix |
|---|---|---|---|
| #53 | Removes on the TripleTrail store get slower with every write | ✅ | F: delta writes stay O(log n) |
| #54 | tripletrail `quads_for_pattern` 8–10× slower than pyoxigraph | ✅ (object building ✅ #108) | F: Python term conversion |
| #55 | tripletrail not a drop-in for `pyoxigraph.Store` | ✅ | F: `bulk_load`, dataset options, `QuerySolution.get`, oxrdflib, canonical decimals |
| #56 | Request timeout not enforced on spatial UPDATEs | ✅ #75 | G: planning is cancellable |
| #57 | Correlated sub-plans: 12 rail KG UPDATEs time out | ✅ | G: LATERAL planning after BIND/OPTIONAL/VALUES and correlated UNION |
| #58 | SHACL: SPARQL-based constraints and targets | ✅ (#52) | E: skip-unsupported compile option not built ⬜ |
| #59 | SHACL validation report, severities, Python/CLI | ✅ (`sh:detail` ✅ #109) | E |
| #61 | SHACL-AF rule inference to fixpoint; 11 RINF rules never finish | ✅ (incremental passes ✅ #110) | E: Python/CLI entry points, per-focus evaluation |
| #62 | Close the performance gap to PostGIS (tracking) | | sub-issues #63–#72 |
| #63 | Point/line × polygon joins 20–170 s | ✅ #73 | B.9, G: parsed geometries, prepared polygons, evaluate once |
| #64 | Route-then-spatial-filter runs as a nested loop | ✅ #75 | G: service rows looked up laterally |
| #65 | `geof:intersection` etc. of line/point × polygon return nothing | ✅ #73 | A.2, B.11: mixed-dimension set operations |
| #66 | Spatial index only covers `geo:asWKT` | ✅ #74 | G: every geometry literal, any predicate |
| #67 | Point × line distance joins 6× slower | ✅ #73 | B.10: ellipsoidal point-to-geometry distance |
| #68 | Constant selections and KNN 20–70 ms | ✅ #73, KNN ✅ #104, #121 | G |
| #69 | Index and networks rebuilt on the first query | ✅, index persisted ✅ #105 | G, C: persisted R-tree and networks, background build, ready state |
| #70 | Length aggregates 5× slower | ✅, per-region join ✅ #106 | A.2, B.10: store geometries to functions, cheaper geodesic length, materialised metrics, parallel aggregates |
| #71 | Property range query 50 ms vs 0.2 ms | ✅ #101 | G: value-ordered range scans on numeric literals |
| #72 | Routing leftovers: components, cost matrix | ✅ | C.1, C.2 |

---

## A. GeoSPARQL 1.1

### A.1 Properties

| Property | Status | Impl | Target state | Issues |
|---|---|---|---|---|
| `geo:hasGeometry`, `geo:hasDefaultGeometry` | ✅ | own | Drive the Query Rewrite (default geometry first, else any). | |
| `geo:asWKT`, `asGeoJSON`, `asGML`, `asKML`, `asDGGS` | ✅ | wkt, geojson, own (quick-xml), h3o | Every geometry literal datatype is indexed, on any predicate. | ✅ #74 |
| `geo:hasSerialization` | ✅ | own | | |
| `geo:hasMetricLength`, `hasLength`, `hasMetricArea`, `hasArea`, `hasMetricPerimeterLength`, `hasPerimeterLength`, `hasMetricSize`, `hasSize` | ✅ virtual (asserted value wins) | geo, own | Metric values stored with the index entries at build time, so the property reads a number. | ✅ #70 |
| `geo:dimension`, `coordinateDimension`, `spatialDimension`, `isEmpty`, `isSimple` | ✅ virtual | own | | |
| `geo:hasCentroid`, `geo:hasBoundingBox` (virtual) | ⬜ | | Only as functions (`geof:centroid`… / `geof:envelope`); not computed as properties. | none |
| `geo:hasVolume`, `hasMetricVolume` | ⬜ | | Volume exists as `st:ST_Volume`; the property is not virtual. | none |
| `geo:hasSpatialResolution`, `hasSpatialAccuracy` (+ metric) | ✅ | | Asserted only, as planned; no code needed. | |
| `geo:sf*`, `geo:eh*`, `geo:rcc8*` as predicates | ✅ (I) | own rewrite | Rewritten to a FILTER and then to an index join; asserted relation triples unioned in. | ✅ #73 #74 |

### A.2 Functions

| Function | Kind | Status | Impl | Target state | Issues |
|---|---|---|---|---|---|
| `geof:sfEquals … sfOverlaps` (8) | S, I | ✅ | own DE-9IM on `geo` | Point-in-polygon and banded-edge fast paths before the full matrix; one prepared structure per polygon; pair tested once on parsed geometries. `sfDisjoint` stays unindexed (anti-join). | ✅ #73 |
| `geof:eh*` (8), `geof:rcc8*` (8) | S, I | ✅ | derived DE-9IM | Same shortcuts as the Simple Features relations. | ✅ #73 |
| `geof:relate` | S | ✅ | own | Full matrix (no shortcut possible). | |
| `geof:distance`, `geof:metricDistance` | S, I | ✅ | geo (Karney), own | Point × point on the ellipsoid; point × geometry on a tangent plane, Karney beyond 50 m (within 2 mm). Fixes the 0.3 % spherical error. | ✅ #73 (#67) |
| `geof:buffer`, `geof:metricBuffer` | S | ✅ | own | Receives the store's parsed geometry instead of re-parsing. | ✅ #70 |
| `geof:convexHull`, `geof:concaveHull` | S | ✅ | geo | As above. | ✅ #70 |
| `geof:intersection`, `union`, `difference`, `symDifference` | S | ✅ | geo `BooleanOps` + own clip | Lines and points against polygons follow PostGIS: result dimension, `GEOMETRYCOLLECTION` for mixed parts, typed empties. | ✅ #73 (#65) |
| `geof:envelope`, `boundary`, `getSRID`, `boundingCircle` | S | ✅ | own | | |
| `geof:area`, `length`, `perimeter`, `metricArea`, `metricLength`, `metricPerimeter` | S | ✅ | geo (Karney) | Store geometry handed to the function; cheaper geodesic length (short-segment fast path, Karney behind a flag); `SUM` over large groups in parallel. Target: total length of 344 k elements in ~300 ms. | ✅ #70 |
| `geof:transform` | S | ✅ | own + proj4rs (no PROJ) | CRS84, EPSG:4326, 3857, UTM (326zz/327zz/258zz), 3006, 3035, 4258, and 65 national CRSs (Belgian Lambert 31370/3812, Lambert-93, RD New, Gauss-Krüger, LV95, BNG…, see [spatial](spatial.md#jena-spatialf)), and the projected CRSs `crs-definitions` has (east/north axes, no grids). | |
| `geof:asWKT`, `asGeoJSON`, `asGML`, `asKML`, `asDGGS` | S | ✅ | own, h3o | | |
| `geof:geometryType`, `numGeometries`, `geometryN`, `dimension`, …, `is3D`, `isMeasured` | S | ✅ | own | | |
| `geof:minX … maxZ` | S | ✅ | own | | |
| `geof:latitude`, `geof:longitude` (QLever) | S | ✅ | own | | |
| `geof:aggBoundingBox`, `aggCentroid`, `aggConcaveHull`, `aggConvexHull`, `aggBoundingCircle`, `aggUnion` | A | ✅ | own | | |
| Jena `spatialf:` functions, CRS as string | S | ✅ (#24) | own | Drop-in for Fuseki queries. | |
| Jena `spatial:` property functions (`nearby`, `withinCircle`, `withinBox`, `intersectBox`, `north` … `west`, and the `…Geom` forms) | T | ✅ (#827) | own, on the spatial index | Drop-in for Fuseki queries, with Jena's answers (`withinBox` tests bounding boxes when it searches, #838; see [spatial](spatial.md#jenas-spatial-search-spatial)). | |

### A.3 Query Rewrite (`geor:`)

All 24 rules ✅: `?a geo:sfX ?b` becomes a join through default/any geometry plus a FILTER,
which then goes through the spatial planner (section G). Target: on any geometry predicate
(✅ #74), each pair tested once (✅ #73).

---

## B. PostGIS `ST_*`

Every function name in the plan's tables B.1–B.19 is registered (checked against the code on
`main`). The rows below keep the plan's grouping; the status is per group. Only rows whose
behaviour changes in the target state have a note.

| Group | Functions (as in the plan) | Kind | Status | Impl | Target state / issues |
|---|---|---|---|---|---|
| B.1 Constructors | `ST_Collect`, `ST_MakeLine` (S + A), `ST_MakeEnvelope`, `ST_MakePoint*`, `ST_Point*`, `ST_MakePolygon`, `ST_Polygon`, `ST_LineFromMultiPoint`, `ST_TileEnvelope`, `ST_HexagonGrid`/`ST_SquareGrid` (T), `ST_Hexagon`, `ST_Square`, `ST_Letters` | S, A, T | ✅ | own | |
| B.2 Accessors | `ST_Boundary`, `ST_Envelope`, `ST_Dimension`, `ST_NDims`, `ST_HasZ/M`, `ST_Dump*` (T), `ST_StartPoint`, `ST_PointN`, `ST_NPoints`, rings, `ST_GeometryN`, `ST_Is*`, `ST_X/Y/Z/M`, `ST_MemSize`, `ST_Summary`, curve/patch accessors | S, T | ✅ | own | |
| B.3 Editors | `ST_AddPoint`, `ST_SetPoint`, `ST_Collection*`, `ST_Multi`, `ST_Force*`, `ST_FlipCoordinates`, `ST_SwapOrdinates`, `ST_Reverse`, `ST_LineExtend`, `ST_Project`, `ST_Segmentize`, `ST_RemoveRepeatedPoints`, `ST_SnapToGrid`, `ST_Snap`, `ST_QuantizeCoordinates`, `ST_ShiftLongitude`, `ST_WrapX`, curve functions | S | ✅ | own, geo | |
| B.4 Validation | `ST_IsValid`, `ST_IsValidReason`, `ST_IsValidDetail`, `ST_MakeValid` | S | ✅ | own | |
| B.5 Spatial reference | `ST_SRID`, `ST_SetSRID`, `ST_Transform`, `ST_TransformPipeline`, `ST_InverseTransformPipeline`, `st:srs`, `st:srsSearch` | S, T | ✅ | own (no PROJ database) | Datum shifts and grids stay out (no PROJ). |
| B.6 Input | `ST_GeomFromText` and all `*FromText`, WKB, EWKB, GeoJSON, GML, KML, GeoHash, TWKB, encoded polyline, FlatGeobuf, MARC21 | S, T | ✅ | own | `ST_FromFlatGeobufToTable`: rows become triples through an update (no tables). |
| B.7 Output | `ST_AsText`, `ST_AsEWKT`, `ST_AsBinary`, `ST_AsEWKB`, `ST_AsHEXEWKB`, `ST_AsGeoJSON`, `ST_AsGML`, `ST_AsKML`, `ST_AsMVTGeom`, `ST_AsMVT` (A and T), `ST_AsSVG`, `ST_AsLatLonText`, `ST_GeoHash`, `ST_AsTWKB`, `ST_AsEncodedPolyline`, `ST_AsX3D`, `ST_AsGeobuf`, `ST_AsFlatGeobuf`, `ST_AsMARC21` | S, A | ✅ | own | |
| B.8 Operators | `st:bboxIntersects` (`&&`), `bboxIntersects3D`, the 8 directional bbox operators, `bboxContains`, `bboxWithin`, `bboxEquals`, `geomEquals`, `distanceKnn` (`<->`), `bboxDistance`, `cpaDistance` | S, I | ✅ | own | KNN: candidates pinned before the class triple, read from the index; target < 5 ms (✅ #73 partly, ✅ #68 remainder). |
| B.9 Relationships | `ST_Intersects`, `ST_Contains`, `ST_ContainsProperly`, `ST_Covers`, `ST_CoveredBy`, `ST_EnvelopesIntersect` (#838), `ST_Crosses`, `ST_Disjoint`, `ST_Equals`, `ST_Overlaps`, `ST_Touches`, `ST_Within`, `ST_Relate`, `ST_RelateMatch`, `ST_OrderingEquals`, `ST_LineCrossingDirection`, `ST_DWithin`, `ST_DFullyWithin`, `ST_PointInsideCircle`, `ST_3DIntersects`, `ST_3DDWithin`, `ST_3DDFullyWithin` | S, I | ✅ | own DE-9IM | Same fast paths as A.2 relations. ✅ #73 |
| B.10 Measurement | `ST_Distance`, `ST_DistanceSphere`, `ST_DistanceSpheroid`, `ST_Length*`, `ST_Perimeter*`, `ST_Area`, `ST_Azimuth`, `ST_Angle`, `ST_ClosestPoint`, `ST_ShortestLine`, `ST_LongestLine`, `ST_MaxDistance`, `ST_HausdorffDistance`, `ST_FrechetDistance`, `ST_MinimumClearance*`, `ST_3D*` measures | S, I | ✅ | geo, own | Spheroid distances agree with PostGIS to the millimetre (✅ #73); length fast path (✅ #70). |
| B.11 Overlay | `ST_Intersection`, `ST_Union` (S + A), `ST_UnaryUnion`, `ST_MemUnion`, `ST_Difference`, `ST_SymDifference`, `ST_Node`, `ST_Split`, `ST_Subdivide` (T), `ST_ClipByBox2D` | S, A, T | ✅ | geo, own | Mixed-dimension semantics. ✅ #73 (#65) |
| B.12 Processing | `ST_Buffer`, `ST_OffsetCurve`, `ST_Centroid`, `ST_PointOnSurface`, `ST_GeometricMedian`, hulls, bounding/inscribed circles, `ST_LargestEmptyCircle`, simplifiers, `ST_ChaikinSmoothing`, `ST_ReducePrecision`, `ST_LineMerge`, `ST_SharedPaths`, `ST_Polygonize`, `ST_BuildArea`, Delaunay, Voronoi, `ST_TriangulatePolygon`, `ST_GeneratePoints`, `ST_FilterByM` | S, A | ✅ | geo, own | Receive the store's parsed geometry (✅ #70). |
| B.13 Coverages | `ST_CoverageUnion` (A), `ST_CoverageSimplify`, `ST_CoverageInvalidEdges` (W), `ST_CoverageClean` | A, W | ✅ (#40) | own | |
| B.14 Affine | `ST_Affine`, `ST_Rotate*`, `ST_Scale`, `ST_Translate`, `ST_TransScale` | S | ✅ | geo | |
| B.15 Clustering | `ST_ClusterDBSCAN`, `ST_ClusterKMeans`, `ST_ClusterIntersecting(Win)`, `ST_ClusterWithin(Win)` | A, W | ✅ (`st` SERVICE) | own | |
| B.16 Bounding box | `ST_Extent`, `ST_3DExtent`, `ST_EstimatedExtent`, `ST_Expand`, `ST_MakeBox2D`, `ST_3DMakeBox`, `ST_XMin … ST_ZMax`, `Box2D`, `Box3D` | S, A | ✅ | own | |
| B.17 Linear referencing | `ST_LineLocatePoint`, `ST_LineInterpolatePoint(s)`, `ST_3DLineInterpolatePoint`, `ST_LineSubstring`, `ST_LocateAlong`, `ST_LocateBetween`, `ST_LocateBetweenElevations`, `ST_InterpolatePoint`, `ST_AddMeasure` | S, T | ✅ | own | |
| B.17 `rail:` LR | `rail:locate`, `rail:intrinsicAt`, `rail:kmAt`, `rail:intrinsicAtKm`, `rail:geomBetweenKm`, `rail:offsetGeom` | S | ✅ | own | km-range queries on asserted kilometre values use the range index (✅ #71). |
| B.18 Trajectory | `ST_IsValidTrajectory`, `ST_ClosestPointOfApproach`, `ST_DistanceCPA`, `ST_CPAWithin` | S | ✅ | own | |
| B.19 SFCGAL-like | `ST_ApproximateMedialAxis`, `ST_StraightSkeleton`, `ST_3DIntersection`, `ST_3DDifference`, `ST_3DUnion`, `ST_3DArea`, `ST_3DConvexHull`, `ST_Extrude`, `ST_Tesselate`, `ST_Volume`, `ST_MakeSolid`, `ST_IsSolid`, `ST_IsPlanar`, `ST_Orientation`, `ST_ForceLHR`, `ST_MinkowskiSum`, `ST_ConstrainedDelaunayTriangles`, `ST_OptimalAlphaShape`, `ST_AlphaShape` | S | ✅ | own (no SFCGAL) | |
| B.20 Introspection | `st:version()`, `st:fullVersion()` | S | ⬜ | | Not built (`pgr_version` is). No issue. |
| B.20 Out of scope | Raster, PostGIS Topology, table management, GUCs | | out of scope | | Unchanged. |

---

## C. pgRouting

Every routing function takes a pgRouting-shaped edge set: a SPARQL sub-select projecting
`?id ?source ?target ?cost [?reverse_cost] [?x1 ?y1 ?x2 ?y2] [?geom] [?c_*]`, or a named
network built from one. That contract is unchanged in the target state.

### C.1 Shortest-path families

| Functions | Kind | Status | Target state | Issues |
|---|---|---|---|---|
| `pgr_dijkstra`, `pgr_dijkstraCost`, `pgr_dijkstraCostMatrix` | T | ✅ | Many-to-many and cost matrices run their sources in parallel and, when the network has a hierarchy (`pgr:engine "ch"`), as a bucket search. Target: 100 × 100 under 2 s on 750 k edges (10 s today). | ✅ #72 |
| `pgr_dijkstraVia`, `pgr_dijkstraNear`, `pgr_dijkstraNearCost` | T | ✅ | | |
| `pgr_drivingDistance` (+ `rail:isochrone`) | T | ✅ | Several sources in parallel. | ✅ #72 |
| `pgr_KSP` | T | ✅ | | |
| `pgr_bdDijkstra*`, `pgr_aStar*`, `pgr_bdAstar*` (with `CostMatrix`) | T | ✅ | | |
| `pgr_trsp`, `pgr_trspVia`, `pgr_trsp_withPoints`, `pgr_trspVia_withPoints`, `pgr_turnRestrictedPath` | T | ✅ | | |
| `pgr_withPoints`, `…Cost`, `…CostMatrix`, `…KSP`, `…DD`, `…Via` | T | ✅ | | |
| `pgr_floydWarshall`, `pgr_johnson` | T | ✅ | | |
| `pgr_bellmanFord`, `pgr_edwardMoore`, `pgr_dagShortestPath`, `pgr_breadthFirstSearch`, `pgr_binaryBreadthFirstSearch`, `pgr_depthFirstSearch` | T | ✅ | | |
| CH / CCH acceleration (`pgr:engine "ch"`) | | ✅ (CCH, re-customised on cost-only writes) | Hierarchies persisted and reloaded at open. | ✅ #69 |

### C.2 Analysis families

| Functions | Kind | Status | Target state | Issues |
|---|---|---|---|---|
| `pgr_connectedComponents`, `pgr_strongComponents` | T | ✅ | Labelling cached per network; an aggregated form (component sizes) so the service does not return one row per vertex; vertex ids as integers where the IRI is not needed. Target: under 500 ms on 688 k vertices (2.1 s today; pgRouting 1.37 s). | ✅ #72 |
| `pgr_biconnectedComponents`, `pgr_articulationPoints`, `pgr_bridges`, `pgr_degree`, `pgr_extractVertices`, `pgr_findCloseEdges` | T | ✅ | | |
| `pgr_separateCrossing`, `pgr_separateTouching` | T | ✅ | | |
| `pgr_kruskal*`, `pgr_prim*` | T | ✅ | | |
| `pgr_maxFlow`, `pgr_boykovKolmogorov`, `pgr_edmondsKarp`, `pgr_pushRelabel`, `pgr_edgeDisjointPaths`, `pgr_maxCardinalityMatch`, `pgr_maxFlowMinCost`, `pgr_maxFlowMinCost_Cost`, `pgr_stoerWagner` | T | ✅ | | |
| `pgr_TSP`, `pgr_TSPeuclidean` | T | ✅ | | |
| `pgr_contraction`, `pgr_lineGraph`, `pgr_lineGraphFull` | T | ✅ | | |
| `pgr_topologicalSort`, `pgr_transitiveClosure`, `pgr_makeConnected`, `pgr_isPlanar`, `pgr_lengauerTarjanDominatorTree`, `pgr_hawickCircuits`, `pgr_sequentialVertexColoring`, `pgr_bipartite`, `pgr_edgeColoring`, `pgr_bandwidth`, `pgr_kingOrdering`, `pgr_sloanOrdering`, `pgr_chinesePostman`, `pgr_chinesePostmanCost` | T | ✅ | | |
| `pgr_pickDeliver`, `pgr_pickDeliverEuclidean`, `pgr_vrpOneDepot` | T | ✅ | | |
| `pgr_version`, `pgr_full_version` | S | ✅ | | |
| Legacy: `rail:buildTopology` (`pgr_createTopology`), `rail:analyzeGraph`, `pgr_nodeNetwork`, `rail:alphaShape` | T | ✅ | | |

### C.3 Beyond pgRouting (`rail:` and `pgr:` extensions)

| Function | Kind | Status | Target state | Issues |
|---|---|---|---|---|
| Named networks (`pgr:Network` with `pgr:edgeQuery`, kept across unrelated writes, saved with `Engine::with_network_dir`) | | ✅ | The server persists networks and hierarchies with the store and reloads them at open; a `/$/status` ready state. The plan's `CREATE NETWORK` SPARQL Update syntax is not built (networks are RDF resources instead). | ✅ #69 |
| `pgr:networkInfo` | T | ✅ | | |
| `rail:route`, `rail:reachable`, `pgr:constraint` on every path function | T | ✅ | | |
| `pgr:shapes`, `pgr:vehicle` + `pgr:rules` (`compat:`), `rail:compatibilityShapes`, `rail:shaclRules` | T | ✅ | | |
| `rail:routeHierarchical`, `rail:refine` | T | ✅ | | |
| `rail:routeGeometry`, `rail:routeAsList` | S | ✅ | | |
| `rail:routeAt` (time-dependent) | T | ✅ | | |
| `rail:paretoRoutes` | T | ✅ | | |
| `rail:checkTopology`, `rail:conflate` | T | ✅ | | |
| `rail:materializeRelations` (write `geo:sf*` triples for a whole graph) | Update | ⬜ | Not built; no issue. | none |
| Route results joined back to the data (`pgr:node`/`pgr:edge` → triples → spatial FILTER) | | ✅ | Service rows look up their triples laterally, so "route, then signals within 50 m" runs in 0.82 s on the large set (17 s before; PostGIS 1.16 s). | ✅ #75 (#64) |
| Profiles: `era-micro`, `era-micro-reversing`, `era-sol`, `generic-triples`, `rinf-compatibility.ttl` | config | ✅ | | |
| Profiles: RailTopoModel, OSM railway, OpenTNF | config | ⬜ | Data, not code; no issue. | none |

---

## D. Not in the plan, added since

| Feature | Status | Notes |
|---|---|---|
| `SERVICE <http://w3id.org/tripletrail/st>` for set-returning and window `ST_*` (`ST_Dump*`, grids, clustering, coverages, `ST_AsMVT` with attributes) | ✅ | |
| Z/M and 3-D across the `st:` set | ✅ | |
| Box2D / Box3D values, `st:srs`, `st:srsSearch` | ✅ | |
| `tripletrail serve` (Fuseki URL layout, Graph Store Protocol, `/explain`, `--timeout`) | ✅ | Timeout also covers planning: ✅ #75 (#56). |
| tripletrail (Python, pyoxigraph's classes built in) | ✅ | Drop-in gaps ✅ #55, speed ✅ #54. |
| SHACL (see E) | ✅ (`sh:detail` ✅ #109) | |
| XPath and Jena function libraries (`fn:`, `math:`, `afn:`), so Jena and Fuseki queries run unchanged | ✅ | See D.1. |

### D.1 XPath and Jena functions (#231)

`fn:` = `http://www.w3.org/2005/xpath-functions#`, `math:` =
`http://www.w3.org/2005/xpath-functions/math#`, `afn:` = `http://jena.apache.org/ARQ/function#`
(the old `http://jena.hpl.hp.com/ARQ/function#` works too). The set follows Jena's
`StandardFunctions`. Calls a SPARQL built-in already computes are rewritten to it before
evaluation, so they behave exactly as the built-in (a constant pattern of `fn:matches` is
compiled once, like `REGEX`). Functions on sequences (`fn:tokenize`, `fn:string-to-codepoints`)
need rows: use the property function `apf:strSplit` (D.3).

| Functions | Status | Notes |
|---|---|---|
| `fn:concat`, `substring`, `string-length`, `upper-case`, `lower-case`, `contains`, `starts-with`, `ends-with`, `substring-before`, `substring-after`, `matches`, `replace`, `encode-for-uri`, `string`, `abs`, `ceiling`, `floor`, `round` (1 argument), `not`, `boolean`, `current-dateTime`, `year-`/`month-`/`day-from-date(Time)` (and Jena's `years-from-*`), `hours-`/`minutes-`/`seconds-from-dateTime`/`-time`, `timezone-from-*` | ✅ | Rewritten to the SPARQL built-in. |
| `fn:normalize-space`, `translate`, `codepoints-to-string`, `compare`, `escape-html-uri`, `iri-to-uri`, `string-join` (strings, then the separator, as in Jena) | ✅ | Code-point collation only. |
| `fn:round` with a precision, `round-half-to-even`, `format-number`, `format-integer`, `min`, `max`, `numeric-mod`, `numeric-integer-divide`, `error` | ✅ | `format-number`: default decimal format, `;` negative sub-picture, `%` and `‰`, no exponent. `format-integer`: digit pictures with grouping, `a`/`A`, `i`/`I`; no words. |
| `fn:years-`/`months-`/`days-`/`hours-`/`minutes-`/`seconds-from-duration`, `adjust-dateTime-`/`date-`/`time-to-timezone`, `dateTime`, `implicit-timezone` | ✅ | The implicit timezone is UTC. |
| `fn:format-dateTime`, `format-date`, `format-time` (picture strings) | ✅ | English names, ISO calendar; components `Y M D d F H h P m s f Z z` with digit, name (`N`, `n`, `Nn`) and width (`,*-3`) modifiers. Written with #233 (D.2). |
| `math:pi`, `exp`, `exp10`, `log`, `log10`, `pow`, `sqrt`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2` | ✅ | `xsd:double` results. |
| `afn:localname`, `namespace`, `bnode`, `sprintf`, `strjoin`, `substr`, `substring`, `min`, `max`, `pi`, `e`, `sqrt`, `now`, `sha1sum` | ✅ | `localname` is the longest XML name at the end of the IRI, as in Jena. `sprintf`: Java's `%s %d %f %e %x %c %%`, flags, width, precision, `%2$s`. |
| Casts `xsd:long`, `int`, `short`, `byte`, `unsignedLong`/`Int`/`Short`/`Byte`, `nonNegativeInteger`, `positiveInteger`, `nonPositiveInteger`, `negativeInteger`, `anyURI` | ✅ | Range-checked. The evaluator keeps computed integers as `xsd:integer`. |
| `fn:normalize-unicode`, collations, `afn:adjust-to-timezone`, Jena aggregates `agg:stdev` etc. | ⬜ | The aggregates belong to #190. |

### D.2 ETL helpers (#233)

`tt:` = `http://w3id.org/tripletrail/fn#`. Vendor aliases where the meaning is the same:
`spif:` = `http://spinrdf.org/spif#` (TopBraid SPIF, as in GraphDB), `ofn:` =
`http://www.ontotext.com/sparql/functions/` (GraphDB).

| Function | Status | Notes |
|---|---|---|
| `tt:skolem(prefix, args...)` | ✅ | As RDFox `SKOLEM`: the prefix followed by 32 hex digits of the SHA-256 of the arguments' terms (kind, lexical form, datatype or language), so the same values always give the same IRI and `3` differs from `"3"`. A relative prefix is resolved against the query's base. |
| `tt:iri(template, args...)` | ✅ | `{0}`, `{1}` (or `{}` in turn) filled with the arguments, percent-encoded; `{{` / `}}` are braces. |
| `tt:encodeForIri(s)` | ✅ | R2RML's IRI-safe form of a value: every character but the `iunreserved` ones (ASCII letters, digits, `-._~` and letters outside ASCII) percent-encoded; `ENCODE_FOR_URI` encodes letters outside ASCII too. Used by `tripletrail lift --as-sparql`. |
| `tt:format(template, args...)`, `spif:buildString`, `spif:buildURI` | ✅ | The same templates, not encoded; SPIF's `{?1}` placeholders work too. `afn:sprintf` is in D.1. |
| `tt:parseDate(text, pattern [, timezone])`, `spif:parseDate` | ✅ | Java patterns (`dd.MM.yyyy`, `yyyy-MM-dd HH:mm`, `d MMM yyyy h:mm a`, `yyyyMMdd'T'HHmmssX`). `xsd:date` without time fields, `xsd:dateTime` with them. English month and day names; the timezone is an offset (`+01:00`, `Z`, `UTC`). |
| `tt:formatDate(value, pattern)`, `spif:dateFormat` | ✅ | Java patterns on `xsd:date`, `xsd:dateTime` and `xsd:time`. |
| `tt:daysBetween`, `monthsBetween`, `yearsBetween` (and `ofn:`) | ✅ | Whole days, months, years from the first date to the second (negative when earlier). |
| `tt:levenshtein`, `jaroWinkler`, `ngramSimilarity(a, b [, n])` | ✅ | Code points; n-gram similarity is the Jaccard index of the n-gram sets (n = 2 by default). |
| `tt:colognePhonetic` | ✅ | Kölner Phonetik, as Apache Commons Codec. |
| `tt:doubleMetaphone` | ⬜ | Not yet. |
| `tt:leftPad`, `rightPad`, `titleCase` (and `spif:`), `camelCase`, `snakeCase`, `trim`, `normalizeWhitespace` | ✅ | |

### D.2b Vectors (#236)

Embedding vectors are literals of the datatype `tt:vector` (`http://w3id.org/tripletrail/fn#vector`):
a JSON array of finite numbers, at most 65,536 of them (`"[0.12, -0.5, 3e-2]"^^tt:vector`). A
plain string in the same form is read as a vector too. The functions give an `xsd:double`,
computed in double precision; they give no value (an error, as for a bad cast) when either
argument is not a vector or the lengths differ.

| Function | Status | Notes |
|---|---|---|
| `tt:cosine(a, b)` | ✅ | Cosine similarity, from -1 to 1; no value when a vector is all zeros. As Stardog's `metric:cosineSimilarity`. |
| `tt:dot(a, b)` | ✅ | Dot product. |
| `tt:l2(a, b)` | ✅ | Euclidean distance. |
| `?s tt:nearest (property query k ?score ?vector)` | ✅ | The `k` resources nearest by cosine, from an in-memory copy kept up to date after writes: exact up to 20,000 vectors of a length, then from a graph (HNSW) built in the background, approximate. See [vector-search.md](vector-search.md). |
| `ORDER BY DESC(tt:cosine(…)) LIMIT k` pushed down to the vector index | ✅ | Exact: the query is evaluated with `?v` kept to the vectors most similar, comparing each one, until it has `k` rows. #236. |

### D.2a Statistical aggregates (#190)

`agg:` = `http://jena.apache.org/ARQ/function/aggregate#` (Jena's names). Integers and
decimals are computed exactly and give a decimal; a float or double among the values gives a
float or double, as AVG. A value that is not a number, or an unbound one, makes the aggregate
unbound, as SUM. `DISTINCT` works as for the built-in aggregates.

| Aggregate | State | Notes |
|---|---|---|
| `tt:median(x)`, `agg:median` | ✅ | The continuous median: the mean of the two middle values of an even count. |
| `tt:percentile(x, p)` | ✅ | `p` from 0 to 1, a constant; linear interpolation between the closest ranks, as SQL's `PERCENTILE_CONT`. Unbound outside 0..1. |
| `tt:variance(x)`, `agg:variance`, `agg:var_samp` | ✅ | Sample variance (divided by n - 1); unbound for one value. |
| `tt:varPop(x)`, `agg:var_pop` | ✅ | Population variance (divided by n). |
| `tt:stddev(x)`, `agg:stdev`, `agg:stdev_samp` | ✅ | Sample standard deviation, an `xsd:double`. |
| `tt:stddevPop(x)`, `agg:stdev_pop` | ✅ | Population standard deviation, an `xsd:double`. |
| `tt:mode(x)`, `agg:mode` | ✅ | The most frequent value, of any kind; of equally frequent values the first one met. |
| `tt:first(x)`, `tt:last(x)` | ✅ | The first and last value in the order the rows reach the aggregate. |
| `tt:topK(x, k)` | ✅ | The `k` largest numbers, largest first, as one string separated by spaces (as `GROUP_CONCAT`). |

### D.2b Window functions (#237)

`SERVICE <http://w3id.org/tripletrail/window>` computes SQL window functions over the rows of a
sub-select; `win:` = `http://w3id.org/tripletrail/window#`. Every row of the sub-select comes
out once with its columns and one column per function:

```sparql
SELECT ?signal ?km ?gap WHERE {
  SERVICE <http://w3id.org/tripletrail/window> {
    { SELECT ?line ?signal ?km WHERE { ?signal ex:line ?line ; ex:km ?km } }
    [] win:partitionBy ?line ; win:orderBy ?km ; win:lead (?km ?nextKm) .
  }
  BIND(?nextKm - ?km AS ?gap)
}
```

`win:partitionBy` (variables or a list), `win:orderBy` and `win:orderByDesc` (keys in the
order given, ordered as SPARQL's ORDER BY), and `win:rows (start end)` (offsets from the row,
`-1` the row before, or `"unbounded"`) set the window. Without `win:rows` the frame is SQL's
default: from the partition's start to the row's last peer with an order, the whole
partition without one. The results match PostgreSQL's for the same rows.

| Function | State | Notes |
|---|---|---|
| `win:rowNumber ?n`, `win:rank`, `win:denseRank`, `win:percentRank`, `win:cumeDist` | ✅ | As SQL; ranks count peers (equal order keys). |
| `win:ntile (n ?q)` | ✅ | Buckets 1 to n, the first ones one row larger when the rows do not divide evenly. |
| `win:lag (?x [offset [default]] ?out)`, `win:lead` | ✅ | The value `offset` rows (1 by default) before or after in the partition, else the default (unbound by default). |
| `win:firstValue (?x ?out)`, `win:lastValue` | ✅ | Over the frame. |
| `win:sum`, `win:avg`, `win:min`, `win:max`, `win:count` `(?x ?out)` | ✅ | Over the frame, skipping unbound values as SQL skips NULL. Exact for integers and decimals; a value that is not a number makes the result unbound. Linear in the rows for any frame. |

### D.3 Property functions (#232)

`list:` = `http://jena.apache.org/ARQ/list#`, `apf:` = `http://jena.apache.org/ARQ/property#`.
A property function is a triple pattern with a registered predicate; `( a b )` in the
subject or object passes several arguments. It is evaluated once per row of what comes
before it in its group (and, in an `OPTIONAL`, once per row of the left side, as Jena does),
so its arguments can come from earlier patterns and `BIND`s.

| Pattern | Status | Notes |
|---|---|---|
| `?list list:member ?m` (`tt:listMember`) | ✅ | A property path `rdf:rest*/rdf:first`. |
| `?list list:index (?i ?m)` (`tt:listIndex`) | ✅ | Index from 0. Counts the nodes before each member: quadratic in the list's length. |
| `?list list:length ?n` (`tt:listLength`) | ✅ | `rdf:nil` has length 0. |
| `?c tt:member ?m`; `apf:bag`, `apf:seq`, `apf:alt` | ✅ | Container members (`rdf:_1`, `rdf:_2`, ...) plus asserted `rdfs:member` triples; `apf:bag` etc. also require the container's type. `rdfs:member` itself stays a plain triple pattern (Jena's container reading of it would scan every quad when the subject is unbound). |
| `?part apf:strSplit (?string "regex")` (`tt:strSplit`) | ✅ | Splits at a regular expression; empty trailing parts are dropped, as Java's `String.split` does. Up to 32,768 parts per string. Replaces `fn:tokenize`, which returns a sequence. |
| `?iri apf:splitIRI (?ns ?local)` (also `apf:splitURI`, `tt:splitIRI`) | ✅ | As `afn:namespace` / `afn:localname`. |
| `?x apf:concat (args...)`, `?x apf:assign ?y` | ✅ | |
| `UNNEST(expr) AS ?x` (Stardog) | ⬜ | Needs parser support. |
| `?s tt:search ("query" ?score ?literal limit)` | ✅ (#188) | Full-text search, ranked with BM25, over the literals of `rdfs:label`, `rdfs:comment`, `skos:prefLabel`, `skos:altLabel`, `dct:title`, `dct:description` and `dc:title` in every graph. `?s tt:search "query"` alone gives the matching resources. Everything after the query is optional: the score (`xsd:double`), the literal matched, and the number of best matches kept. Words are runs of letters and digits, lower-cased, with Latin accents taken off; `word*` matches every word it starts; a literal matches if it has any word of the query, and more and rarer words score higher. One row per (resource, literal). Unlike the functions above, it is answered from an index before the query runs. The query string must be written in the query; a variable for it, or a variable given twice, is an error. The search covers every graph, also in a query on the default graph (the protocol's dataset narrows it to its graphs); a caller limited to some graphs or rows searches only what it can read, ranked as a store of only that data would (#203). It is refused inside `GRAPH`, `SERVICE` and `FILTER (NOT) EXISTS`. With a constant subject, the limit counts that subject's matches. Only on the TripleTrail store. The index is built on the first search (0.8 s for the 560,000 labels of the rail KG), and the first search after a write patches in the labels added and removed (about 40 ms on the rail KG); a search then takes milliseconds. |
| `(?s ?score ?literal) text:query (property "query" limit)` | ✅ (#188) | Jena's full-text property function, answered from the `tt:search` index. The property and the limit are optional, and the subject may be `?s` alone. A property must be one of the indexed ones: only its literals match, and the limit counts those. A limit of -1 is no limit. The query string is read as Lucene's (#830): words are OR'ed, `OR` and `||` say so, `+word`, `AND` and `&&` make words required, `-word`, `!word` and `NOT` exclude them, `"a phrase"` matches the words in order, and a trailing `*` matches the words a word starts. The `lang:` option keeps one language (and its subtags), and in a language with a Snowball stemmer (18 of them, Norwegian, Dutch, English, French and German among them) matches the other forms of its words, as Jena's multilingual index does and `highlight:` marks the matches in `?literal` with Jena's markers and fragments (`s:`, `e:`, `z:`, `f:`). Other Lucene syntax is refused with an error rather than read differently: `field:value`, `?`, `~`, `^`, groups, ranges, escapes, a `*` not at the end or in a phrase, and the `?graph` and `?prop` subject items. Intended deviations from Jena: without a property, every indexed property is searched, not only the default field of the Jena index (usually `rdfs:label`), so write `rdfs:label` for Jena's usual answer; scores are TripleTrail's BM25 and differ from Lucene's. |

## E. SHACL

| Feature | Status | Target state | Issues |
|---|---|---|---|
| SHACL Core: every target, path and constraint component | ✅ | | |
| SHACL-SPARQL: `sh:sparql` constraints, SPARQL constraint components, `$this`/`$PATH`/`$currentShape`/`$shapesGraph`, `sh:prefixes` | ✅ (#52) | | #58 |
| SHACL-AF SPARQL targets and target types | ✅ (#52) | | #58 |
| Compile option: skip unsupported constraints with a warning | ⬜ | Not built; #58 closed without it. | none |
| Severities (`sh:Warning`, `sh:Info`) | ✅ | Each result carries its shape's severity; `sh:conforms` is false for a result of any severity, as SHACL defines it. | #59 |
| W3C validation report (`sh:ValidationResult` with focus node, path, value, source shape, source constraint component, message, severity; `sh:conforms`; streamed) | ✅ | Written as a graph or as rows, streamed for ~1–2 M results. | #59 |
| Validation from Python and the CLI | ✅ | `Engine.validate(shapes_graph=…, report_graph=…)`, `tripletrail validate --data … --shapes … --report …`. | #59 |
| SHACL-AF rules: `sh:TripleRule`, `sh:SPARQLRule`, `sh:order`, `sh:condition`, fixpoint (`Inference`) | ✅ (#52), Rust only | | |
| SHACL-AF functions: `sh:SPARQLFunction` with `sh:select`/`sh:ask` bodies, `sh:parameter` (`sh:order`, `sh:optional`), `sh:prefixes` | ✅ | Callable in queries, updates, constraints and rules; one-expression bodies are inlined; recursion refused. See [validation and reasoning](validation-and-reasoning.md#user-defined-functions). | #235 |
| Rules from Python and the CLI, into an inference graph | ✅ | `Engine.infer(shapes_graph, into=…, max_iterations=…)`. | #61 |
| Rules evaluated per focus node (`$this` pre-bound or used as a correlated join key) | ✅ | The 11 RINF rules that never finish run in seconds. | #61, #57 |

## F. Store and Python API

| Feature | Status | Target state | Issues |
|---|---|---|---|
| Writes on the TripleTrail store | ✅ | Removes and inserts cost the same after 100 k earlier writes (indexed delta or compaction); today N removes are O(N²). | ✅ #53 |
| `Engine.quads_for_pattern` in Python | ✅ | At pyoxigraph's speed: term classes built natively (#108). | ✅ #54 |
| `bulk_load`, `bulk_extend` | ✅ | Aliases of `load`/`extend`. | #55 |
| `query(…, default_graph=…, named_graphs=…, use_default_graph_as_union=…)` | ✅ | | #55 |
| `QuerySolution.get(name)` | ✅ | | #55 |
| `named_graphs()`, `add_graph()`, `remove_graph()`; usable as an rdflib store | ✅ | | #55 |
| Canonical `xsd:decimal` output (`"100.0"` → `"100"`) | ✅ | Canonicalise as Oxigraph does, or document. | #55 |

## G. Index-pushable patterns (the planner)

What the planner answers from an index rather than row by row, in the target state.

| Pattern | Today | Target state | Issues |
|---|---|---|---|
| Geometry literals indexed | `geo:asWKT` objects only | Every `wktLiteral`, `geoJSONLiteral`, `gmlLiteral`, `kmlLiteral` and H3 literal on any predicate; allow/deny list (`--index-predicate`). | ✅ #74 (#66) |
| `FILTER(geof:sfX(?w, CONST))` and `st:` equivalents | ✅ R-tree candidates as `VALUES`, FILTER kept | Candidates tested in parallel on parsed geometries; the FILTER dropped when the `VALUES` block is exact. Target < 5 ms for ~50 rows on 700 k geometries (66 ms before, 2–3 ms in #73). | ✅ #73 (#68) |
| `FILTER(distance(?w, CONST) < d)`, `ST_DWithin` | ✅ | As above. | ✅ #73 |
| `ORDER BY distance(?w, CONST) LIMIT k` (KNN) | ✅ growing radius | Candidates pinned before the class triple. Target < 5 ms (11 ms in #73). KNN still needs `geo:asWKT` after #74. | ✅ #73, #104, #121 |
| Two-variable join `FILTER(geof:sfX(?w1, ?w2))`, `?a geo:sfX ?b` | ✅ index nested loop or sweep | Each pair tested once on parsed geometries, in parallel on both paths; prepared polygon per distinct polygon. `join_op_region` large 34 s → 0.78 s (PostGIS 0.06 s). | ✅ #73 (#63) |
| Distance join `FILTER(distance(?w1, ?w2) <= d)` | ✅ sweep | Ellipsoidal distance; target ~2 s for 315 k pairs (6.9 s before, 5.7 s in #73, the rest in evaluating pinned LATERAL rows). | ✅ #73 (#67), #62 |
| One side bound only through a `SERVICE`, `BIND` or sub-select | evaluated as a nested loop | Service rows look up their triples once each, with `LATERAL`. | ✅ #75 (#64) |
| Spatial planning under a request timeout | not cancellable | The rewrite checks the cancellation token; 503 at the deadline. | ✅ #75 (#56) |
| `FILTER EXISTS / NOT EXISTS` sharing outer variables | ✅ planned from bound variables (#24) | Also: inner patterns after `BIND`/`OPTIONAL` as LATERAL, correlated UNION, LATERAL triples after `VALUES`. | ✅ #57 |
| Numeric range `FILTER(?km >= a && ?km <= b)` on a variable bound by a triple | evaluated row by row | Range scan over a value-ordered object index for numeric literals, intersected with the rest; target a few ms (50 ms today, PostgreSQL 0.2 ms). | ✅ #71 |
| `SUM(geof:metricLength(?w))` over many rows | row by row, single-threaded | Metric value read from index entries; aggregated in parallel chunks. | ✅ #70 |
| Spatial index at start | built at the first spatial query (9 s on 700 k geometries) | Persisted with the store checkpoint and loaded at open; otherwise built in the background at start; kept across bulk loads. | ✅ #69, #105 |
