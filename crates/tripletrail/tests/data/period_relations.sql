-- The truth table of the period relations (#300), from PostgreSQL's tstzrange operators.
-- Regenerate: psql -qAF $'\t' -P footer=off -f period_relations.sql > period_relations.tsv
-- Each period is written as tt:period's canonical form ('..' for an open side) and as the
-- half-open range PostgreSQL reads.
SET TIME ZONE 'UTC';
WITH p(name) AS (VALUES
  ('2020-01-01T00:00:00Z/2021-01-01T00:00:00Z'),
  ('2021-01-01T00:00:00Z/2022-01-01T00:00:00Z'),
  ('2020-06-01T00:00:00Z/2021-06-01T00:00:00Z'),
  ('2019-01-01T00:00:00Z/2022-01-01T00:00:00Z'),
  ('2020-01-01T00:00:00Z/2020-06-01T00:00:00Z'),
  ('2020-06-01T00:00:00Z/2021-01-01T00:00:00Z'),
  ('2020-03-01T00:00:00Z/2020-04-01T00:00:00Z'),
  ('2020-01-01T00:00:00Z/2020-01-01T00:00:00.001Z'),
  ('2020-12-31T23:59:59.999Z/2021-01-01T00:00:00Z'),
  ('2021-01-01T00:00:00Z/2021-01-01T00:00:00.001Z'),
  ('../2021-01-01T00:00:00Z'),
  ('../2020-01-01T00:00:00Z'),
  ('2020-01-01T00:00:00Z/..'),
  ('2021-01-01T00:00:00Z/..'),
  ('../..'),
  ('1999-12-31T23:00:00Z/2000-01-01T23:00:00Z')
), r AS (
  SELECT name, tstzrange(
    NULLIF(split_part(name, '/', 1), '..')::timestamptz,
    NULLIF(split_part(name, '/', 2), '..')::timestamptz, '[)') AS v
  FROM p
), pairs AS (
  SELECT a.name AS an, b.name AS bn, a.v AS a, b.v AS b,
    tstzrange(lower(a.v), NULL) = tstzrange(lower(b.v), NULL) AS lower_eq,
    tstzrange(NULL, upper(a.v)) = tstzrange(NULL, upper(b.v)) AS upper_eq,
    NOT (a.v &> b.v) AS as_lt_bs, NOT (b.v &> a.v) AS bs_lt_as,
    NOT (b.v &< a.v) AS ae_lt_be, NOT (a.v &< b.v) AS be_lt_ae
  FROM r a CROSS JOIN r b
)
SELECT an AS a, bn AS b,
  (a << b AND NOT a -|- b)::int AS "periodBefore",
  (a >> b AND NOT a -|- b)::int AS "periodAfter",
  (a -|- b AND a << b)::int AS "periodMeets",
  (a -|- b AND a >> b)::int AS "periodMetBy",
  (as_lt_bs AND a && b AND ae_lt_be)::int AS "periodOverlaps",
  (bs_lt_as AND a && b AND be_lt_ae)::int AS "periodOverlappedBy",
  (lower_eq AND ae_lt_be)::int AS "periodStarts",
  (lower_eq AND be_lt_ae)::int AS "periodStartedBy",
  (bs_lt_as AND ae_lt_be)::int AS "periodDuring",
  (as_lt_bs AND be_lt_ae)::int AS "periodStrictlyContains",
  (upper_eq AND bs_lt_as)::int AS "periodFinishes",
  (upper_eq AND as_lt_bs)::int AS "periodFinishedBy",
  (a = b)::int AS "periodEquals",
  (a && b)::int AS "periodIntersects",
  (a @> b)::int AS "periodContains"
FROM pairs ORDER BY an, bn;
