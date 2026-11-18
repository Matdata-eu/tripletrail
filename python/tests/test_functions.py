import tripletrail

FUNCTIONS = """
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.com/> .
ex:double a sh:SPARQLFunction ; sh:parameter [ sh:path ex:x ] ;
    sh:select "SELECT ($x * 2 AS ?r) WHERE {}" .
ex:name a sh:SPARQLFunction ; sh:parameter [ sh:path ex:x ] ;
    sh:select "SELECT ?n WHERE { $x <http://example.com/name> ?n }" .
"""


def test_user_defined_functions():
    engine = tripletrail.Engine()
    engine.load('<http://example.com/a> <http://example.com/name> "A" .', "nt")
    with_functions = engine.with_functions(FUNCTIONS, "ttl")
    rows = list(
        with_functions.query(
            "PREFIX ex: <http://example.com/> "
            "SELECT (ex:double(21) AS ?d) (ex:name(ex:a) AS ?n) {}"
        )
    )
    assert rows[0]["d"].value == "42"
    assert rows[0]["n"].value == "A"
    # The engine it came from has no such functions, but the same store.
    engine.update('INSERT DATA { <http://example.com/b> <http://example.com/name> "B" }')
    rows = list(
        with_functions.query("SELECT (<http://example.com/name>(<http://example.com/b>) AS ?n) {}")
    )
    assert rows[0]["n"].value == "B"


def test_bad_functions_are_refused():
    import pytest

    with pytest.raises(ValueError, match="sh:select"):
        tripletrail.Engine().with_functions(
            "<http://example.com/f> a <http://www.w3.org/ns/shacl#SPARQLFunction> .", "ttl"
        )
