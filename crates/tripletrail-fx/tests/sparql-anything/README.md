# Test files from SPARQL Anything

The files in this folder come from SPARQL Anything
(<https://github.com/SPARQL-Anything/sparql.anything>, commit `22b918f`, November 2026),
Copyright the SPARQL Anything contributors, under the Apache License 2.0 (see `LICENSE`).
They are copied unchanged from the test resources of its modules:

- `spreadsheet/`: `sparql-anything-spreadsheet/src/test/resources` (`1`, `2`, `NullString`,
  `NoNullString`, `FormulaEvaluator`, `HeaderRow` `.xls` with their `.nq`/`.trig`, and
  `Book1.xlsx` with `Book1.nq`).
- `yaml/`: `sparql-anything-yaml/src/test/resources` (`Foo`, `Map`, `DuplicateKeys` `.yaml`
  with their `.ttl`, and `test.yaml`).
- `archive/`: `sparql-anything-archive/src/test/resources` (`test.tar`, `test.tgz`,
  `test.zip`, and the folder `test` as `folder`). `test.tar.bz2` and `test.tbz2` are not
  from SPARQL Anything: they are its `test.tar` compressed here with `bzip2 -9`.
  `two-streams.tar.bz2` is the same tar as two bzip2 streams (its first 4,096 bytes, then
  the rest), and `incompressible.tar.bz2` a tar of 150,000 random bytes (`photo.bin`) and
  `test/test.csv` (as `test.csv`), compressed with `bzip2 -1`.
- `markdown/`: `sparql-anything-markdown/src/test/resources` (every `.md` with its `.ttl`).
- `html/`: `sparql-anything-html/src/test/resources` (`HTML1`, `HTML2`, `DIRTY1`, `Base`
  `.html` with their `.ttl`, `BN.html` with `BN.nq`, `Microdata1.html` and `RDFa.html` with
  their `.nq`, and `XML.xml` with `XML.ttl`).

Only the files are reused; TripleTrail's Facade-X code is written independently.
