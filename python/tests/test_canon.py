"""RDFC-1.0 canonicalization and release diffs (#191)."""

import tripletrail as ox
from tripletrail import Engine

A = b"""
<http://ex/s> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/C> .
<http://ex/s> <http://ex/p> "1" .
<http://ex/s> <http://ex/geo> _:x .
_:x <http://ex/wkt> "POINT(1 2)" .
"""


def engine(text):
    e = Engine()
    e.load(text, ox.RdfFormat.N_TRIPLES)
    return e


def test_isomorphic_stores_have_one_digest():
    a = engine(A)
    b = engine(A.replace(b"_:x", b"_:other"))
    assert a.canonical_digest() == b.canonical_digest()
    assert len(a.canonical_digest()) == 64
    assert b"<http://ex/s> <http://ex/geo> _:c14n0 .\n" in a.canonicalize()


def test_diff_counts_changes():
    a = engine(A)
    b = engine(A.replace(b'"1"', b'"2"'))
    d = ox.diff(a, b)
    assert d["added"] == ['<http://ex/s> <http://ex/p> "2" .\n']
    assert d["removed"] == ['<http://ex/s> <http://ex/p> "1" .\n']
    assert d["predicates"]["http://ex/p"] == (1, 1)
    assert d["classes"]["http://ex/C"] == (1, 1)
    assert d["sha256"][0] == a.canonical_digest()
