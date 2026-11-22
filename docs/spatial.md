# Spatial functions

## GeoSPARQL (`geof:`)

`geof:` (`http://www.opengis.net/def/function/geosparql/`), the full GeoSPARQL 1.1 function set:

- Relations: `sfEquals`, `sfDisjoint`, `sfIntersects`, `sfTouches`, `sfCrosses`, `sfWithin`,
  `sfContains`, `sfOverlaps`, the Egenhofer `ehEquals`, `ehDisjoint`, `ehMeet`, `ehOverlap`,
  `ehCovers`, `ehCoveredBy`, `ehInside`, `ehContains`, the RCC8 `rcc8eq`, `rcc8dc`, `rcc8ec`,
  `rcc8po`, `rcc8tppi`, `rcc8tpp`, `rcc8ntpp`, `rcc8ntppi`, and `relate`.
- Measures and accessors: `distance`, `metricLength`, `length`, `metricArea`, `area`, `metricPerimeter`,
  `perimeter`, `metricSize`, `size`,
  `dimension`, `coordinateDimension`, `spatialDimension`, `isEmpty`, `isSimple`, `is3D`,
  `isMeasured`, `getSRID`, `geometryType`, `numGeometries`, `geometryN`, `minX`, `minY`, `minZ`,
  `maxX`, `maxY`, `maxZ`.
- Constructions: `envelope`, `boundary`, `boundingCircle`, `centroid`, `convexHull`,
  `concaveHull`, `buffer`, `intersection`, `union`, `difference`, `symDifference`, `transform`.
  The four set operations take points, lines and polygons in any combination, with the
  PostGIS semantics: the intersection of a line and a polygon is the part of the line inside
  it, a difference keeps the parts of the first geometry outside the second, and a union of
  geometries of different dimensions is a collection.
- Serialisation: `asWKT`, `asGeoJSON`, `asGML` (GML 3.2), `asKML`, `asDGGS` (H3 cells, below).
- Aggregates: `aggBoundingBox`, `aggBoundingCircle`, `aggCentroid`, `aggConcaveHull`,
  `aggConvexHull`, `aggUnion`.

The CRS argument of `geof:transform` may be an IRI, or an `xsd:anyURI` or string literal holding
one (as Jena accepts).

Geometry literals can be `geo:wktLiteral`, `geo:geoJSONLiteral`, `geo:gmlLiteral` (GML 3.2 simple
features and GML 2 coordinates, CRS from `srsName`, which must be an IRI or `EPSG:n`), `geo:kmlLiteral` or an H3 cell list. The
spatial index covers them all, on any predicate.

### DGGS

`geof:asDGGS(?geom, <http://w3id.org/tripletrail/dggs#h3Literal> [, resolution])` returns the
H3 cells covering a geometry as `"CELLLIST ((89... 89...))"`. A point gives one cell, a line the
chain of cells it crosses, and a polygon every cell it touches. The default resolution is 9, cells
of about 0.1 km². Projected geometries are transformed to CRS84 first. `st:ST_H3Cells(?geom
[, resolution])` is the same without the datatype argument. Cell lists are geometries too: every
function reads them as the outline of their cells, so `geof:sfContains(?cells, ?point)` works.
A geometry whose cover would hold more than 100,000 cells, or with a latitude past ±90°, gives an
error (unbound) instead.

The GeoSPARQL relation properties work as triple patterns too (the Query Rewrite Extension):
`?a geo:sfIntersects ?b` holds between features, through their default geometry or any
geometry if they have none, and between geometries. Asserted relation triples count as well.
They work the same in the sub-selects of TripleTrail's own services (`st:`, window, `pgr`,
`path`), which the spatial rewrite and its index reach as they reach the main query (#857),
and inside `FILTER EXISTS`, `FILTER NOT EXISTS`, `BIND(EXISTS { … } AS ?v)` and `HAVING(EXISTS …)` (#56),
where they are looked up for each row with its values bound. An `EXISTS` in an `ORDER BY` or
aggregate expression sees asserted relation triples only.

Geometry properties are virtual too: `geo:hasMetricLength`, `hasLength`, `hasMetricArea`,
`hasArea`, `hasMetricPerimeterLength`, `hasPerimeterLength`, `hasMetricSize`, `hasSize`,
`dimension`, `coordinateDimension`, `spatialDimension`, `isEmpty` and `isSimple` are computed
from the geometry of a geometry or feature (`?track geo:hasMetricLength ?m`). Where the data
asserts one of them for a subject, the asserted value is used instead.

## PostGIS (`st:`)

`st:` (`http://w3id.org/tripletrail/st#`, PostGIS names and planar semantics): `ST_Equals`,
`ST_Disjoint`, `ST_Intersects`, `ST_Touches`, `ST_Crosses`, `ST_Within`, `ST_Contains`,
`ST_Overlaps`, `ST_Covers`, `ST_CoveredBy`, `ST_EnvelopesIntersect` (the bounding boxes meet, as PostGIS's `&&`), `ST_Distance`, `ST_DistanceSphere`, `ST_DWithin`,
`ST_DWithinSphere`, `ST_Length`, `ST_LengthSpheroid`, `ST_Area`, `ST_X`, `ST_Y`, `ST_XMin`,
`ST_YMin`, `ST_XMax`, `ST_YMax`, `ST_NPoints`, `ST_StartPoint`, `ST_EndPoint`, `ST_MakePoint`,
`ST_AsText`, `ST_GeomFromText`, `ST_Envelope`, `ST_Centroid`, `ST_ConvexHull`, `ST_Buffer`,
`ST_Intersection`, `ST_Union`, `ST_Difference`, `ST_SymDifference`, `ST_Simplify`, `ST_Reverse`,
`ST_ClosestPoint`, `ST_LineLocatePoint`, `ST_LineInterpolatePoint`, `ST_LineSubstring`.

More PostGIS functions:

- Accessors: `ST_GeometryType`, `GeometryType`, `ST_SRID`, `ST_NumPoints`, `ST_PointN`,
  `ST_ExteriorRing`, `ST_InteriorRingN`, `ST_NumInteriorRings`, `ST_NumGeometries`,
  `ST_GeometryN`, `ST_Dimension`, `ST_CoordDim`, `ST_NDims`, `ST_ZMin`, `ST_ZMax`,
  `ST_IsClosed`, `ST_IsRing`, `ST_IsSimple`, `ST_IsEmpty`, `ST_IsValid`, `ST_IsValidReason`,
  `ST_IsCollection`, `ST_OrderingEquals`.
- Constructors: `ST_Point`, `ST_MakeLine` (two geometries, or as an aggregate: see below),
  `ST_LineFromMultiPoint`, `ST_MakePolygon`,
  `ST_MakeEnvelope`, `ST_Collect`, `ST_Multi`, `ST_CollectionExtract`, `ST_Points`,
  `ST_GeomFromGeoJSON`, `ST_GeomFromGML`, `ST_GeomFromKML`, `ST_AsGeoJSON`, `ST_AsGML`,
  `ST_AsKML`.
- Processing: `ST_Boundary`, `ST_ConcaveHull`, `ST_MinimumBoundingCircle`, `ST_PointOnSurface`,
  `ST_BoundingDiagonal`, `ST_LineMerge`, `ST_ShortestLine`, `ST_ClipByBox2D`, `ST_Segmentize`,
  `ST_SimplifyVW`, `ST_ChaikinSmoothing`, `ST_ForcePolygonCW`, `ST_ForcePolygonCCW`,
  `ST_Force2D`, `ST_FlipCoordinates`.
- Editing and affine transforms: `ST_AddPoint`, `ST_RemovePoint`, `ST_SetPoint`, `ST_Translate`,
  `ST_Scale`, `ST_Rotate` (radians, counter-clockwise), `ST_Expand`.
- Measures: `ST_Perimeter`, `ST_Azimuth`, `ST_Project` (metres along the geodesic on
  geographic data), `ST_HausdorffDistance`, `ST_FrechetDistance`, `ST_MaxDistance`,
  `ST_DFullyWithin`, `ST_DistanceSpheroid`, `ST_Relate` (the DE-9IM string, or a pattern test).
- CRS: `ST_SetSRID`, `ST_Transform` (SRIDs are EPSG codes; 4326 means CRS84 order).
- Formats: `ST_AsEWKT`, `ST_GeomFromEWKT`, `ST_AsBinary`/`ST_AsWKB`/`ST_AsEWKB` (WKB as
  `xsd:hexBinary`, Z and M kept), `ST_AsHEXEWKB`, `ST_GeomFromWKB`/`ST_GeomFromEWKB`/
  `ST_WKBToSQL` (hex), `ST_GeometryFromText`, `ST_WKTToSQL` and the typed `ST_PointFromText`,
  `ST_LineFromText`, `ST_PolygonFromText`, `ST_MPointFromText`, `ST_MLineFromText`,
  `ST_MPolyFromText`, `ST_GeomCollFromText` (optional SRID), `ST_GeoHash`,
  `ST_PointFromGeoHash`, `ST_GeomFromGeoHash`, `ST_Box2dFromGeoHash`, `ST_AsEncodedPolyline`,
  `ST_LineFromEncodedPolyline`, `ST_AsSVG`, `ST_AsLatLonText`, `ST_MakeBox2D`, `ST_Polygon`,
  `ST_TileEnvelope` (EPSG:3857).
- More accessors and relations: `ST_Z`, `ST_M`, `ST_Zmflag`, `ST_HasArc`, `ST_NRings`,
  `ST_IsPolygonCW`, `ST_IsPolygonCCW`, `ST_ContainsProperly`, `ST_RelateMatch`,
  `ST_PointInsideCircle`, `ST_Angle`, `ST_LongestLine`, `ST_Length2D`, `ST_Perimeter2D`.
- More editors: `ST_ForceCollection`, `ST_ForceRHR`, `ST_SwapOrdinates` (x and y),
  `ST_RemoveRepeatedPoints`, `ST_SnapToGrid`, `ST_Snap` (to vertices, else to segments),
  `ST_Affine` (the 2-D part of the 3-D form), `ST_TransScale`, `ST_ShiftLongitude`.
- Validation and processing: `ST_MakeValid`, `ST_SimplifyPreserveTopology` (Douglas-Peucker,
  keeping a part as it is where simplifying would make it invalid), `ST_OrientedEnvelope`,
  `ST_DelaunayTriangles`, `ST_VoronoiPolygons`, `ST_VoronoiLines`, `ST_OffsetCurve` (mitred
  joins; loops on the inner side of bends tighter than the offset are not removed),
  `ST_LineInterpolatePoints`, `ST_GeneratePoints` (repeatable with a seed),
  `ST_GeometricMedian`.
- Overlay: `ST_Split` (lines by points or lines; polygons are not split yet), `ST_Node`,
  `ST_UnaryUnion`, `ST_Subdivide` (one collection instead of a set of rows).
- Aggregate: `ST_MakeLine(?geom)` (also `ST_MakeLineAgg`) joins the points and lines of a group
  into one line, in the order the rows come, dropping a vertex repeated where one part ends at
  the start of the next. SPARQL aggregates have no `ORDER BY`; order the rows in a sub-select:
  `SELECT ?loc (st:ST_MakeLine(?seg) AS ?line) WHERE { { SELECT ?loc ?seg WHERE { ... } ORDER
  BY ?loc ?seq } } GROUP BY ?loc`. In one query `st:ST_MakeLine` is either the aggregate or the
  two-argument function; `ST_MakeLineAgg` is always the aggregate.
- More aggregates: `ST_Extent` (the bounding box, as a polygon), `ST_UnionAgg` and
  `ST_CollectAgg` (`ST_Union` and `ST_Collect` over a group; the plain names are the
  two-argument functions).

- Z and M: `ST_MakePoint` with 3 or 4 ordinates, `ST_MakePointM`, `ST_PointZ`, `ST_PointM`,
  `ST_PointZM`, `ST_HasZ`, `ST_HasM`, `ST_Force3D`/`ST_Force3DZ`/`ST_Force3DM`/`ST_Force4D`,
  `ST_RotateX`/`ST_RotateY`/`ST_RotateZ`, `ST_FilterByM`, `ST_SetEffectiveArea` (the
  Visvalingam-Whyatt area as M).
- Measures along lines: `ST_AddMeasure`, `ST_InterpolatePoint`, `ST_LocateAlong`,
  `ST_LocateBetween` (with an optional offset), `ST_LocateBetweenElevations`; trajectories
  (lines with increasing M): `ST_IsValidTrajectory`, `ST_ClosestPointOfApproach`,
  `ST_DistanceCPA`, `ST_CPAWithin`.
- 3-D: `ST_3DDistance`, `ST_3DIntersects`, `ST_3DDWithin`, `ST_3DDFullyWithin`,
  `ST_3DClosestPoint`, `ST_3DShortestLine`, `ST_3DLongestLine`, `ST_3DMaxDistance`,
  `ST_3DLength`, `ST_3DPerimeter`, `ST_3DLineInterpolatePoint`, `ST_3DMakeBox` (as its 2-D
  envelope). Polygons are planes: a point above one is as far from it as its distance to the
  plane.
- Boxes: `st:Box2D(geom)` is PostGIS's `box2d` cast, the bounding box as a polygon (as
  `ST_MakeBox2D` and `ST_Extent` write it, 2-D); `st:Box3D(geom)` is the `box3d` cast, the line
  from the lowest to the highest corner (`LINESTRING Z`, as `ST_3DExtent`; Z 0 for 2-D input).
  Both also read PostGIS's text forms, `"BOX(xmin ymin,xmax ymax)"` and
  `"BOX3D(xmin ymin zmin,xmax ymax zmax)"` (as CRS84).
- Bounding-box operators (PostGIS's `&&`, `~`, `@`, `~=`, `=`, `<<`, `&<`, `>>`, `&>`, `<<|`,
  `&<|`, `|>>`, `|&>`, `&&&`, `<#>`, `<->` and `|=|`): `st:bboxIntersects`, `bboxContains`,
  `bboxWithin`, `bboxEquals`, `geomEquals`, `bboxLeftOf`, `bboxOverLeft`, `bboxRightOf`,
  `bboxOverRight`, `bboxBelow`, `bboxOverBelow`, `bboxAbove`, `bboxOverAbove`,
  `bboxIntersects3D`, `bboxDistance`, `distanceKnn` and `cpaDistance`.
- Shapes: `ST_BuildArea`, `ST_Polygonize` (a collection of the faces; also the aggregate
  `ST_PolygonizeAgg`), `ST_BdPolyFromText`, `ST_BdMPolyFromText`, `ST_SharedPaths`,
  `ST_LineCrossingDirection`, `ST_MinimumClearance`, `ST_MinimumClearanceLine`,
  `ST_MaximumInscribedCircle` and `ST_LargestEmptyCircle` (the centre point),
  `ST_MinimumBoundingRadius` (the radius), `ST_SimplifyPolygonHull`, `ST_TriangulatePolygon`
  (constrained Delaunay), `ST_Hexagon`, `ST_Square`.
- More editors: `ST_CollectionHomogenize`, `ST_ForceSFS`, `ST_Scroll`, `ST_Normalize`,
  `ST_LineExtend`, `ST_RemoveSmallParts`, `ST_RemoveIrrelevantPointsForView`,
  `ST_QuantizeCoordinates`, `ST_ReducePrecision`, `ST_WrapX`.
- More formats and accessors: `ST_AsTWKB`, `ST_GeomFromTWKB`, `ST_PointFromWKB`,
  `ST_LineFromWKB`, `ST_LinestringFromWKB`, `ST_GeogFromWKB`, `ST_GeogFromText`,
  `ST_GeographyFromText`, `ST_NumInteriorRing`, `ST_MemSize`, `ST_Summary`, and `geof:latitude`,
  `geof:longitude`, `geof:metricDistance`, `geof:metricBuffer` (QLever and GeoSPARQL draft
  names).
- More aggregates: `ST_MemUnion` (as `ST_UnionAgg`) and `ST_ClusterIntersecting` (a collection of
  collections, one per group of geometries connected by intersection).
- `ST_3DExtent` (aggregate): PostGIS's `box3d` as the line from its lowest to its highest corner,
  `LINESTRING Z(xmin ymin zmin,xmax ymax zmax)`, Z 0 when no input has Z. `ST_XMin`, `ST_ZMax`,
  `ST_Envelope` and the like read it.

- SFCGAL-style and P2: `ST_AlphaShape` and `ST_OptimalAlphaShape` (CGAL's convention: alpha is a
  squared radius), `ST_MinkowskiSum`, `ST_ConstrainedDelaunayTriangles`, `ST_Tesselate` (a
  `TIN`, as in PostGIS), `ST_ApproximateMedialAxis` (Voronoi edges of the densified boundary
  inside the polygon), `ST_StraightSkeleton`, `ST_IsPlanar`, `ST_3DArea`, `ST_Orientation`,
  `ST_ForceLHR`, `ST_AsX3D`, and the curve functions `ST_CurveToLine`, `ST_LineToCurve`,
  `ST_ForceCurve`, `ST_NumCurves`, `ST_CurveN` (geometries here are always linear, so a line is
  one linear curve). `ST_CoverageUnion` is an aggregate; `ST_CoverageInvalidEdges`,
  `ST_CoverageSimplify` and `ST_CoverageClean` are window functions in the `st` SERVICE (below).
  Polyhedral surfaces, TINs and solids are described below.
- Feature formats, binary as `xsd:hexBinary` like `ST_AsBinary`, encoders written by hand:
  `ST_AsFlatGeobuf` (FlatGeobuf 3, columns typed from the values, optional packed Hilbert
  R-tree; read back by the reference `flatgeobuf` crate in the tests) and `ST_AsGeobuf` (a
  feature collection, up to six decimals, Z kept) are aggregates over geometries, and SERVICE
  functions where the sub-select's other columns become properties, as PostGIS's row argument;
  `ST_FromFlatGeobuf` is a SERVICE function returning a row per feature. There are no tables,
  so `ST_FromFlatGeobufToTable` has no counterpart: an `INSERT ... WHERE { SERVICE ... }` turns
  the rows into triples. `ST_AsMARC21(geom [, format])` writes a MARC21/XML record with a
  field 034 (bounding box) per part, in PostGIS's formats (`hdddmmss`, `hddd.dddddd`,
  `dddmm.mmmm`, `hdddmmss.sss`, ..., a comma as decimal sign allowed; without `h` the sign is
  `+` or `-`); `ST_GeomFromMARC21` reads those fields back as points, boxes or multi-geometries.
- `ST_Letters(text [, font])`: text as a multipolygon, 100 units high from the origin, in a
  built-in stroke font (A-Z, a-z, 0-9 and `.,:;!?'"-+=()/_#*`), or in a JSON font mapping each
  character to its outline as hex WKB or WKT (used at its own size).
- `ST_IsValidDetail(geom [, flags])` returns the reason (unbound when valid); the whole
  `(valid, reason, location)` row is in the `st` SERVICE. Locations are a crossing or overlap
  of the offending rings or polygons, else a vertex of the offending part. The flags change
  nothing.
- `ST_GMLToSQL` is `ST_GeomFromGML`. `ST_EstimatedExtent` is an aggregate equal to
  `ST_Extent`: there are no table statistics to estimate from, so the extent is exact.

### Polyhedral surfaces, TINs and solids

`POLYHEDRALSURFACE`, `TIN` and `TRIANGLE` literals
(2-D or `Z`; not `M`) are read from and written to WKT and (E)WKB, and kept with true 3-D
coordinates: a list of 3-D polygon patches plus PostGIS's solid flag. `ST_NumPatches`,
`ST_PatchN` (a `POLYGON Z`, or a `TRIANGLE Z` for a TIN), `ST_NumGeometries`, `ST_GeometryN`,
`ST_GeometryType`/`GeometryType`/`geof:geometryType`, `ST_Dimension` (3 for a solid), `ST_IsClosed`,
`ST_3DArea`, `ST_ZMin`/`ST_ZMax`, `ST_AsText`, `ST_AsEWKT`, `ST_AsBinary`, `ST_AsX3D` and
`geof:transform` read them as such; other (2-D) functions see their footprint, the union of the
patches projected on the XY plane. `ST_MakeSolid` sets the solid flag (unchecked, as in PostGIS)
and `ST_IsSolid` reads it; `ST_Volume` is the volume of a solid (by the divergence theorem) and 0
for a surface, as in PostGIS. A solid literal is written as SFCGAL's
`SOLID Z((((...)),((...))))` so that the flag survives from one function to the next;
`ST_AsText`, `ST_AsEWKT` and WKB write it as PostGIS does, as a `POLYHEDRALSURFACE` (WKB, like
PostGIS's, drops the flag). `ST_Extrude` sweeps a point into a line, a line into a surface and a
polygon (or a surface) into a solid, oriented outward. A point, line or polygon literal with two
vertices at one (x, y) but different Z (a vertical edge or face, points above each other) is
also kept in 3-D, since the (x, y)-keyed Z below cannot hold it; this is how `ST_PatchN` of a
vertical face, the vertical line `ST_Extrude` makes of a point, or a stack of points for
`ST_3DConvexHull` round-trip.

`ST_3DConvexHull` builds the hull incrementally with Shewchuk's exact `orient3d` and `orient2d`
predicates (the `robust` crate) for every visibility, coplanarity and collinearity test, so it
is exact for the input coordinates; the result is a `POLYHEDRALSURFACE Z` of triangles over the
extreme points only (a `POLYGON Z` for coplanar points, a `LINESTRING Z` for collinear ones), not
flagged as a solid, as in PostGIS. `ST_3DUnion`, `ST_3DIntersection` and `ST_3DDifference` of two
solids use BSP-tree CSG (the csg.js algorithm), after which coplanar fragments are merged back into
faces; the result is a solid. A surface, line or point set is clipped by a solid
(`ST_3DIntersection`, and `ST_3DDifference` with the solid second): lines are cut where they
cross the solid's faces and each piece, like each point, is classified by its generalized winding
number; points on the boundary count as inside. A solid minus a lower-dimensional geometry is
the solid. Two polygonal inputs with neither a solid nor a 3-D surface are combined in 2-D, as
`ST_Intersection` and the like do. Not supported (unbound): the union of a solid with something
other than a solid, the set operations of two non-solid 3-D surfaces (SFCGAL intersects their
triangles), and those of non-polygonal inputs without a solid. Precision: the CSG predicates are epsilon-based, not exact. A vertex is on a
plane when within `1e-9` times the largest absolute ordinate of the operands (at least `1e-9`),
and new vertices where an edge crosses a plane are rounded to `f64`, so results are exact for
axis-aligned input on a coarse grid and otherwise accurate to about `1e-9` relative. Inputs
flagged as solids must be closed and consistently oriented, which is not checked. The merged
faces may have T-junctions where a face was split, so a CSG result is closed geometrically but
not always edge for edge (`ST_IsClosed` compares edges exactly).

`ST_StraightSkeleton` (of polygons with holes; a `MULTILINESTRING` of the arcs) simulates the
shrinking wavefront with edge and split events (the Felkel–Obdržálek event model), choosing the
next event by scanning all candidates rather than with a priority queue: O(n²) per event, fine
for the polygons of a knowledge graph. Its tolerances are relative (`1e-9` of the polygon's
size); simultaneous events (a square, a rectangle) are handled by closing off a front once it
has collapsed to zero area.

### Z and M

Computations are 2-D, but Z and M ordinates are kept: a result vertex takes them
from the input vertex at the same position, or interpolates them along the input segment it lies
on, so `geof:transform`/`ST_Transform`, `ST_LineSubstring`, `ST_LineInterpolatePoint`,
`ST_Reverse`, `ST_Simplify` and the like return `LINESTRING Z (...)` for 3-D input. Z and M are matched by (x, y) position, so two
vertices at the same position share them. A result with a
vertex found on no input (a buffer, for example) is 2-D, as are `ST_Centroid`, `ST_Envelope` and
`ST_Force2D`, as in PostGIS. `nan` ordinates are read (GeoPackage writes an unknown M that way); a
geometry with a NaN X or Y (`nan`, `+nan` or `-nan`) is invalid.

## Jena (`spatialf:`)

`spatialf:` (`http://jena.apache.org/function/spatial#`, Apache Jena's names, so Jena queries run
as they are): `transformSRS`, `transform`, `transformDatatype`, `convertLatLon`,
`convertLatLonBox`, `distance`, `greatCircle`, `greatCircleGeom`, `azimuth`, `azimuthDeg`. Units
and CRSs may be IRIs or strings. `spatialf:distance` is Jena's distance: on a geographic
geometry, the haversine distance (earth radius 6,371,008.77 m) between the two geometries'
nearest points in longitude and latitude; on a projected one, the planar distance.

CRS: CRS84 (default) and EPSG:4326 (axis order handled) are geographic; metric functions on them
are geodesic for points and lengths, and use a local projection for distance and buffer between
other geometries. Any other CRS gets planar operations, and mixing CRSs is an error.
`geof:transform` converts between CRS84, EPSG:4326, EPSG:3857 (Web Mercator), UTM (EPSG:326zz,
327zz and ETRS89 258zz), EPSG:3006 (SWEREF99 TM) and EPSG:3035 (ETRS89-LAEA), computed in
TripleTrail, and 65 national and regional CRSs through `proj4rs` (pure Rust, no PROJ library):
Belgian Lambert 72 and 2008 (EPSG:31370, 3812), Lambert-93 and the CC zones (2154, 3942-3950),
NTF Lambert II (27572), RD New (28992), Luxembourg (2169), Gauss-Krüger (31466-31469, 4647, 5650),
LV95 and LV03 (2056, 21781), Austria (31287, 31254-31259, 3416), British National Grid (27700),
Irish grids (2157, 29903), SWEREF99 local zones (3007-3018), TM35FIN (3067), Poland (2180,
2176-2179), S-JTSK Krovak (5514), Portugal (3763), Monte Mario (3003, 3004), EOV (23700),
Slovenia (3794), Croatia (3765), Stereo70 (3844), the Baltic grids (3301, 3059, 3346), the Greek
Grid (2100) and ETRS89-LCC (3034). They agree with PROJ to within 1 cm. CRSs on another datum
(BD72, Amersfoort, DHDN, CH1903, MGI, OSGB36…) are shifted to WGS 84 with the 7-parameter
transformation PROJ uses without grids, good to about a metre; ETRS89 is treated as WGS84 (they
differ by under a metre).

The projected CRSs the [`crs-definitions`](https://crates.io/crates/crs-definitions) crate (CC0)
has convert too (about 4,500 codes; the crate does not have every current EPSG code), with the
datum shift its PROJ.4 definition gives: `st:ST_Transform(?wkt, 2193)` gives New Zealand TM metres,
`st:ST_Transform(?wkt, 2263)` New York Long Island US feet. Where a definition has no shift for a
datum other than WGS 84, positions can be off by up to a few hundred metres. NAD27 CRSs use the
mean NAD27 shift for the contiguous US (good to about 10 m there) instead of the NADCON grids.
Left out: longitude/latitude CRSs other than WGS 84 and ETRS89 (the definitions don't give their
axis order), CRSs whose axes point south or west (such as Krovak 2065 and 5513; Krovak East North
5514 works), geocentric CRSs, those that need grid files, and two that `proj4rs` gets wrong (3167,
3303). Longitudes come back from -180 to 180. These codes are in `st:srs` (name, WKT,
PROJ.4 text) but not in the catalogue listing or `postgis_srs_search`, as the registry gives no
area of use.

These CRSs (and EPSG:4258) are also the SRS catalogue, PostGIS's `spatial_ref_sys` in small:
`st:srs(code [, field])` gives a CRS's OGC WKT 1 (`srtext`), or the field named: `srname`,
`proj4text`, `auth_name`, `auth_srid`, `point_sw`, `point_ne` (the area of use's corners, CRS84
points). The code may be `"EPSG:25832"`, `25832`, `"CRS84"`, a CRS IRI or an
`urn:ogc:def:crs:EPSG::25832` URN; unknown codes leave the result unbound. The set-returning
`postgis_srs`, `postgis_srs_all` and `postgis_srs_search` are `st:srs` and `st:srsSearch` in the
`st` SERVICE (below). There is no PROJ database, so other EPSG codes are not known; the areas of
use are the EPSG dataset's bounding boxes, rounded to 0.01°, and for the ETRS89 UTM zones (258zz) the zone's
longitudes within the ETRS89 extent (EPSG area 1298) rather than EPSG's per-country polygons.

`ST_TransformPipeline(geom, pipeline [, to_srid])` and `ST_InverseTransformPipeline` run PROJ
pipeline strings with these projections: `+proj=pipeline` with `+step`s (global parameters,
`+inv`, `+omit_fwd`, `+omit_inv`) or a single operation, made of `unitconvert` (`xy_in`/`xy_out`
in `deg`, `rad`, `grad`, `m`, `km`, `ft`, `us-ft`, ...), `axisswap`, `noop`, `longlat`, `utm`,
`tmerc`/`etmerc` (with `lat_0=0`), `webmerc`, spherical `merc` (Web Mercator) and `laea`, on the
WGS 84 or GRS 80 ellipsoid; and EPSG conversions by URN (`urn:ogc:def:coordinateOperation:EPSG::`
16001-16060 and 16101-16160 for the UTM zones, 3856, 19986 and 17333), which take longitude and
latitude in degrees, as in PostGIS. The result has the SRID `to_srid`, or, without one, that of
the CRS the pipeline ends in when it is recognisable (a UTM zone, Web Mercator, EPSG:3035,
degrees). Missing without PROJ's database and grids: datum transformations (`helmert`, `cart`,
`molodensky`, grid shifts, EPSG transformations such as 1618), concatenated operation URNs, other
ellipsoids and other projections; those pipelines leave the result unbound.

### Jena's spatial search (`spatial:`)

The property functions of jena-geosparql (`spatial:` = `http://jena.apache.org/spatial#`) run
as in Fuseki, without a spatial index to configure (#827). Each finds the features near or in
an area, once each:

| Property function | Arguments | Finds the features |
|---|---|---|
| `spatial:nearby`, `spatial:withinCircle` | `(lat lon radius [units [limit]])` | less than `radius` away (kilometres by default) |
| `spatial:nearbyGeom`, `spatial:withinCircleGeom` | `(geometry radius [units [limit]])` | less than `radius` from the geometry |
| `spatial:withinBox` | `(latMin lonMin latMax lonMax [limit])` | whose bounding box meets the box; a bound subject: within the box |
| `spatial:withinBoxGeom` | `(geometry [limit])` | the same, with the geometry's bounding box |
| `spatial:intersectBox` | `(latMin lonMin latMax lonMax [limit])` | meeting the box |
| `spatial:intersectBoxGeom` | `(geometry [limit])` | meeting the geometry's bounding box |
| `spatial:north`, `south`, `east`, `west` | `(lat lon [limit])` | whose bounding box reaches beyond that latitude or longitude (east and west: within half the globe, wrapped at 180°) |
| `spatial:northGeom`, … `westGeom` | `(geometry [limit])` | the same, beyond the edge of the geometry's bounding box |

```sparql
PREFIX spatial: <http://jena.apache.org/spatial#>
PREFIX units: <http://www.opengis.net/def/uom/OGC/1.0/>
SELECT ?station WHERE { ?station spatial:nearby (50.8456 4.3571 5 units:kilometre) }
```

A feature's geometries are the `geo:asWKT`, `geo:asGML` or `geo:hasSerialization` literals of its
`geo:hasGeometry` nodes; a feature without `geo:hasGeometry` is the point of its
`wgs84_pos:lat` and `wgs84_pos:long`. With the subject a variable, the call finds the
features; with the subject a resource or a variable bound before it, it tests that one.
Arguments may be variables bound before the call. Distances are Jena's: the haversine distance
between the nearest points of the two geometries in longitude and latitude, the same as
`spatialf:distance`. Units are IRIs or strings, and `units:degree` and `units:radian` count arc
on the earth's mean radius (Jena refuses them). The searches are answered from the spatial
index, so a nearby search among 200,000 features takes a few milliseconds.

`spatial:withinBox` and `withinBoxGeom` give Jena's answers in both of its cases (#838). When
they search for features (an unbound subject), they test only each feature's bounding box, so
a feature that crosses the edge of the box is found too. A bound subject must lie within the
box. One difference remains: Jena's spatial index reads every graph, while TripleTrail reads the
query's default graph or the graph of the enclosing `GRAPH`.

## Set-returning and window functions

PostGIS functions that return rows (`ST_Dump`, `ST_DumpPoints`, `ST_DumpSegments`,
`ST_DumpRings`, `ST_Subdivide`, `ST_HexagonGrid`, `ST_SquareGrid`, `ST_IsValidDetail`,
`ST_FromFlatGeobuf`) or one value per row of a
set (`ST_ClusterDBSCAN`, `ST_ClusterKMeans`, `ST_ClusterIntersectingWin`,
`ST_ClusterWithinWin`, `ST_CoverageInvalidEdges`, `ST_CoverageSimplify`, `ST_CoverageClean`), the
set forms of `ST_ClusterIntersecting` and `ST_ClusterWithin`, the feature encoders
`ST_AsFlatGeobuf` and `ST_AsGeobuf`, and `ST_AsMVT` and `rail:conflate` (below) run in
`SERVICE <http://w3id.org/tripletrail/st>`, the same way as the routing functions:

```sparql
SELECT ?f ?path ?part WHERE {
  SERVICE <http://w3id.org/tripletrail/st> {
    [] st:function st:ST_Dump ; st:path ?path ; st:geom ?part .
    { SELECT ?f ?geom WHERE { ?f geo:hasGeometry/geo:asWKT ?geom } }
  }
}
```

The input is a constant (`st:geom "..."^^geo:wktLiteral`, or `st:bounds` for the grids) or a
sub-select projecting `?geom`; its other variables are passed through to the rows each input
produces. Parameters keep PostGIS's names (`st:size`, `st:eps`, `st:minpoints`, `st:k`,
`st:max_radius`, `st:distance`, `st:max_vertices`, `st:tolerance`, `st:gap_width`), and results are `st:path` (PostGIS's array
syntax, `{1,2}`), `st:geom`, `st:i`, `st:j` and `st:cluster_id` (unbound for DBSCAN noise). Window
functions run over all input rows, or per value of `?partition` when the sub-select projects one
(PostGIS's `PARTITION BY`). `ST_ClusterKMeans` starts from a fixed seed, so it is repeatable.

`ST_IsValidDetail` gives `st:valid`, `st:reason` and `st:location` (unbound when valid).
`ST_AsFlatGeobuf` (`st:index true` for a spatial index) and `ST_AsGeobuf` give one row,
`st:flatgeobuf` or `st:geobuf`, for all input rows, with every column of the sub-select but
`?geom` (or the one `st:geom_name` names) as a property. `ST_FromFlatGeobuf` reads `st:data`
(hex, or `?data` of a sub-select) and gives `st:fid` (from 0), `st:geom` and a column per
property, named after it (`st:name ?name`):

```sparql
SELECT ?fgb WHERE {
  SERVICE <http://w3id.org/tripletrail/st> {
    [] st:function st:ST_AsFlatGeobuf ; st:flatgeobuf ?fgb .
    { SELECT ?geom ?name WHERE { ?f rdfs:label ?name ; geo:hasGeometry/geo:asWKT ?geom } }
  }
}
```

`st:srs` is `postgis_srs`: `st:code` (as for the scalar `st:srs` above), or `st:auth_name` and
`st:auth_srid`, gives one row with `st:auth_name`, `st:auth_srid`, `st:srname`, `st:srtext`,
`st:proj4text`, `st:point_sw`, `st:point_ne` and `st:crs` (the CRS IRI); without a code it is
`postgis_srs_all`, every CRS of the catalogue (of one authority with `st:auth_name`).
`st:srsSearch` is `postgis_srs_search`: for `st:bounds` (or `?geom` of a sub-select, in any
supported CRS) the projected CRSs whose area of use contains the bounds' box, the smallest area
of use first, with the same columns (`st:auth_name "EPSG"` restricts the authority).

```sparql
SELECT ?code ?name WHERE {
  SERVICE <http://w3id.org/tripletrail/st> {
    [] st:function st:srsSearch ; st:bounds "POLYGON((7 50,8 50,8 51,7 51,7 50))"^^geo:wktLiteral ;
       st:auth_srid ?code ; st:srname ?name .
  }
}
```


## Vector tiles

`st:ST_AsMVTGeom(?geom, ?bounds [, extent [, buffer [, clip_geom]]])` puts a geometry in the
coordinate space of a Mapbox Vector Tile covering the box of `?bounds` (for example
`st:ST_TileEnvelope(z, x, y)`): `extent` units wide (4096), y pointing down, clipped to the tile
plus `buffer` units (256) unless `clip_geom` is false, snapped to integers, with parts that
collapse dropped, and only the parts of the highest dimension kept. Exterior rings come out
clockwise as drawn in tile coordinates (positive area by the surveyor's formula), as the MVT spec
wants. A geometry in another CRS than the bounds is transformed first. The result has no CRS (it
is written as plain WKT); it is meant for `ST_AsMVT`.

`st:ST_AsMVT(?tilegeom)` is an aggregate that encodes a group of such geometries as a
Mapbox Vector Tile 2.1 with one layer named `default`, extent 4096 and no attributes. Tiles are
`xsd:hexBinary` (as `ST_AsBinary`); an empty group gives an empty tile. For layer names and
attributes, use `ST_AsMVT` in the `st` SERVICE (below): every variable of the sub-select other
than `?geom` becomes an attribute (integers, doubles, booleans and strings keep their type, IRIs
are strings, unbound values are left out).

```sparql
SELECT ?mvt WHERE {
  SERVICE <http://w3id.org/tripletrail/st> {
    [] st:function st:ST_AsMVT ; st:name "tracks" ; st:feature_id_name "n" ;
       st:bounds "<http://www.opengis.net/def/crs/EPSG/0/3857> POLYGON((...))"^^geo:wktLiteral ;
       st:mvt ?mvt .
    { SELECT ?geom ?n ?speed WHERE { ... } }
  }
}
```

Parameters: `st:name` (the layer, `default`), `st:extent` (4096), `st:feature_id_name` (the
variable, without `?`, whose integer value is the feature id; it is not an attribute), and
`st:bounds`: when given, the geometries are passed through `ST_AsMVTGeom` with it, `st:extent`,
`st:buffer` (256) and `st:clip_geom` (true); without it they must be in tile coordinates already.
A tile with several layers is the concatenation of one-layer tiles:
`STRDT(CONCAT(STR(?a), STR(?b)), xsd:hexBinary)`.

## Linear referencing

`rail:` (`http://w3id.org/tripletrail/rail#`) functions work on lines and numbers given by the query,
whatever the data model. A line is a `LINESTRING`, or a `MULTILINESTRING` whose parts are elements
in order and direction, measured as one line (a gap between parts adds no length). Positions are
intrinsic coordinates from 0 at the start to 1 at the end, in proportion to the geodesic length on
CRS84 and EPSG:4326 and to the planar length otherwise.

- `rail:locate(?line, ?intrinsic)`: the point at a position.
- `rail:intrinsicAt(?line, ?point)`: the position of the point of the line nearest to a point.
- `rail:kmAt(?line, ?intrinsic [, ?kmTable])`: the kilometre at a position.
- `rail:intrinsicAtKm(?line, ?km [, ?kmTable])`: the position of a kilometre.
- `rail:geomBetweenKm(?line, ?km1, ?km2 [, ?kmTable])`: the part of the line between two
  kilometres, in the direction of the line; one `LINESTRING` across touching elements, a
  `MULTILINESTRING` when elements do not touch.
- `rail:offsetGeom(?line, ?from, ?to, ?offset)`: the part between two positions, moved `?offset`
  to its left (right when negative), in metres on geographic coordinates (through a local
  projection) and CRS units otherwise. With `?from = ?to`, the point beside that position.

A kilometre table is a string of `intrinsic:km` anchors, `"0:12.300 0.5:12.950 1:13.600"`
(separated by spaces, commas or semicolons; intrinsic coordinates must not decrease), or a line
with M ordinates whose vertices are anchors with their M as kilometre; without a table, the M
ordinates of `?line` itself are the kilometres. Kilometres are interpolated linearly between
anchors and extrapolated beyond the first and last, and may decrease along the line. Two anchors
at the same position are a jump: there, `kmAt` gives the kilometre after it, and a kilometre
inside the jump has no position. A kilometre that occurs twice (an overlap) gives its first
position. Z and M are interpolated onto results that lie on the line.

`rail:alphaShape(?geom, ?alpha)` is pgRouting's `pgr_alphaShape`: the union of the Delaunay
triangles of the vertices of `?geom` whose circumradius is at most `sqrt(?alpha)` (planar, in CRS
units), as a polygon or multipolygon. `?alpha` must be positive; pgRouting's search for an
optimal alpha is not done.

`rail:conflate` runs in the `st` SERVICE with two sub-selects, the source lines and then the
target lines, each projecting `?id` and `?geom`. It returns a row per source and target line that
come within `st:tolerance` of each other (metres on geographic coordinates): `st:source`,
`st:target` (the ids), `st:distance` (discrete Fréchet distance, or Hausdorff with
`st:method "hausdorff"`, between the lines with vertices added every half tolerance, in metres on
geographic coordinates) and `st:overlap` (the fraction of the
source line's length within the tolerance of the target, sampled every half tolerance).
Candidates come from an R-tree of the targets' boxes; `st:min_overlap` drops pairs below a
fraction.

```sparql
SERVICE <http://w3id.org/tripletrail/st> {
  [] st:function rail:conflate ; st:tolerance 15 ;
     st:source ?osm ; st:target ?track ; st:distance ?d ; st:overlap ?o .
  { SELECT ?id ?geom WHERE { ?id a ex:OsmWay ; geo:hasGeometry/geo:asWKT ?geom } }
  { SELECT ?id ?geom WHERE { ?id a ex:Track ; geo:hasGeometry/geo:asWKT ?geom } }
}
```

## Spatial index

`Engine::query` rewrites spatial FILTERs that compare a geometry literal (of `geo:asWKT` or any
other predicate, such as `geonames:asWKT`) with a constant geometry, so they read candidates from an R-tree instead of testing every geometry:

```sparql
?f geo:hasGeometry/geo:asWKT ?w
FILTER(geof:sfIntersects(?w, "POLYGON((...))"^^geo:wktLiteral))   # also sfWithin, sfContains, ...
FILTER(geof:distance(?w, "POINT(5.7 58.4)"^^geo:wktLiteral) < 2000)   # also st:ST_DWithin(Sphere)
```

Spatial joins between two variables, as a FILTER or as a relation triple, are rewritten too:

```sparql
?a geo:hasGeometry/geo:asWKT ?w1 . ?b geo:hasGeometry/geo:asWKT ?w2
FILTER(geof:sfIntersects(?w1, ?w2))
?a geo:sfWithin ?zone . ?zone a ex:Zone
```

Nearest-neighbour queries (PostGIS's `ORDER BY geom <-> point LIMIT k`) use the index too:

```sparql
SELECT ?f WHERE { ?f geo:hasGeometry/geo:asWKT ?w }
ORDER BY (geof:distance(?w, "POINT(5.7 58.4)"^^geo:wktLiteral)) LIMIT 10
```

The FILTER is still evaluated on the candidates, so results are identical to a full scan. The
same applies to the `WHERE` of `Engine::update` (`INSERT { ... } WHERE { ... FILTER(geof:...) }`;
for the first operation of a request, since later ones may read what earlier ones wrote). The
index is built on the first spatial query after a write. A cost model decides from triple counts
and a histogram of the index whether a constant test is worth its candidates, which side of a
join probes the index, and whether the candidates go before the triples that bind them.
`Engine::set_spatial_rewrite(false)` turns the rewrite off, `Engine::set_cost_model(false)` the
cost model, and `Engine::last_spatial_stats()` says what they did. Details and numbers are in
[spatial-index.md](spatial-index.md).
