//! Tables and queries of PostgreSQL databases as sources ([`DatabaseLoader`]).
//!
//! A mapping names its database with a `d2rq:Database` (RML, YARRRML) or names none (R2RML):
//! then the loader's default database is read. Values come as their natural RDF literals
//! (R2RML 10.2): integers as `xsd:integer`, `numeric` as `xsd:decimal`, `real` and `double
//! precision` as `xsd:double`, `boolean`, `date`, `time`, `timestamp` (with or without time
//! zone) as `xsd:dateTime`, `bytea` as `xsd:hexBinary`, PostGIS `geometry` and `geography`
//! as `geo:wktLiteral` (with their SRID's CRS), and everything else as text.
//!
//! The loader also serves Facade-X `table=` and `sql=` sources ([`tripletrail_fx::Databases`]):
//! from the default database or one of its named connections, in a read-only transaction,
//! through a cursor so that at most the row cap is read.

use crate::RmlError;
use crate::mapping::{Database, LogicalSource, SourceRef};
use crate::run::{Cell, Loader, Table};
use crate::tls;
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, NamedNodeRef};
use postgres::types::Type;
use postgres::{Client, SimpleQueryMessage};
use std::collections::HashMap;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use tripletrail_fx::{Condition, DatabaseQuery, Databases, FxError, Op, Rows, Select};

/// A [`Loader`] that reads files with `files` and tables and queries from PostgreSQL. It
/// keeps one connection per database for mappings, and up to [`Self::sessions`] for the
/// Facade-X sources of each. Connections use TLS as libpq's `sslmode` in the
/// connection string says (`prefer` by default; `verify-full` checks the certificate and
/// host name against the Mozilla roots or the PEM file of `sslrootcert`).
pub struct DatabaseLoader<L> {
    pub files: L,
    /// The database of mappings that name none (R2RML), as a PostgreSQL connection string:
    /// `postgresql://user:password@host:port/name` or `host=… user=… dbname=…`.
    pub default: Option<String>,
    /// Connections Facade-X sources name with `database=<name>`, as connection strings.
    pub named: HashMap<String, String>,
    /// Longest a Facade-X query may run, its transfer included; none by default. PostgreSQL's
    /// `statement_timeout` is set to it, and TripleTrail cancels the query when it is up, so SQL
    /// that changes the setting does not run on.
    pub statement_timeout: Option<std::time::Duration>,
    /// Most bytes one Facade-X query may give (in PostgreSQL's text form); none by default.
    pub max_bytes: Option<usize>,
    /// Most sessions open at once per Facade-X connection; calls beyond it wait. 4 by default.
    pub sessions: usize,
    /// How long a Facade-X result is reused for the same connection and statement; none by
    /// default.
    pub cache_ttl: Option<std::time::Duration>,
    results: Mutex<ResultCache>,
    clients: Mutex<HashMap<String, Arc<Mutex<Client>>>>,
    /// Facade-X sessions by connection string; [`Pools::freed`] is notified when one is
    /// given back or closed.
    pools: Pools,
}

impl<L> std::fmt::Debug for DatabaseLoader<L> {
    /// Where the connections go, without their passwords.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shown = |url: &String| {
            parse(url, url)
                .map(|(c, _, _)| location(&c))
                .unwrap_or_else(|_| "(invalid)".into())
        };
        let mut named: Vec<(&String, String)> =
            self.named.iter().map(|(n, u)| (n, shown(u))).collect();
        named.sort();
        f.debug_struct("DatabaseLoader")
            .field("default", &self.default.as_ref().map(shown))
            .field("named", &named)
            .finish_non_exhaustive()
    }
}

impl<L> DatabaseLoader<L> {
    pub fn new(files: L, default: Option<String>) -> Self {
        Self {
            files,
            default,
            named: HashMap::new(),
            statement_timeout: None,
            max_bytes: None,
            sessions: 4,
            cache_ttl: None,
            results: Mutex::default(),
            clients: Mutex::default(),
            pools: Pools::default(),
        }
    }

    /// With a longest time per Facade-X query.
    #[must_use]
    pub fn with_statement_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.statement_timeout = Some(timeout);
        self
    }

    /// With Facade-X results reused for `ttl` (none when zero).
    #[must_use]
    pub fn with_cache(mut self, ttl: std::time::Duration) -> Self {
        self.cache_ttl = (!ttl.is_zero()).then_some(ttl);
        self
    }

    /// With a most sessions open at once per Facade-X connection (at least one).
    #[must_use]
    pub fn with_sessions(mut self, sessions: usize) -> Self {
        self.sessions = sessions.max(1);
        self
    }

    /// With a most bytes per Facade-X query.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = Some(max_bytes);
        self
    }

    /// With the named connections of Facade-X sources.
    #[must_use]
    pub fn with_named(mut self, named: HashMap<String, String>) -> Self {
        self.named = named;
        self
    }

    fn client(&self, database: Option<&Database>) -> Result<Arc<Mutex<Client>>, RmlError> {
        let (config, key) = match database {
            Some(d) => {
                let config = config_of(d)?;
                (config, format!("{d:?}{:?}", d.password))
            }
            None => {
                let url = self.default.as_deref().ok_or_else(|| {
                    RmlError::Mapping(
                        "the mapping reads a table but names no database: give one to connect \
                         to (lift --database)"
                            .into(),
                    )
                })?;
                (parse(url, url)?, url.to_owned())
            }
        };
        self.connect(config, key)
    }

    /// The tables (and views) of `schema` in the default database, with their columns,
    /// primary keys and foreign keys, for the direct mapping ([`crate::direct`]). `only`, if
    /// not empty, keeps those tables (each must exist).
    pub fn direct_tables(
        &self,
        schema: &str,
        only: &[String],
    ) -> Result<Vec<crate::direct::DirectTable>, RmlError> {
        use crate::direct::{DirectTable, ForeignKey};
        let client = self.client(None)?;
        let mut client = client.lock().expect("not poisoned");
        let error = |e: postgres::Error| RmlError::Source {
            location: format!("schema {schema}"),
            message: error_message(&e),
        };
        let rows = client
            .query(
                "SELECT c.oid, c.relname::text, c.relkind IN ('r', 'p', 'm') FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
                 AND NOT c.relispartition ORDER BY c.relname",
                &[&schema],
            )
            .map_err(error)?;
        let mut oids: Vec<(u32, String, bool)> = rows
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect();
        if !only.is_empty() {
            for name in only {
                if !oids.iter().any(|(_, n, _)| n == name) {
                    return Err(RmlError::Mapping(format!(
                        "there is no table {name} in schema {schema}"
                    )));
                }
            }
            oids.retain(|(_, n, _)| only.contains(n));
        }
        let mut tables = Vec::with_capacity(oids.len());
        for (oid, name, stored) in &oids {
            let columns = client
                .query(
                    "SELECT attname::text FROM pg_attribute \
                     WHERE attrelid = $1 AND attnum > 0 AND NOT attisdropped ORDER BY attnum",
                    &[oid],
                )
                .map_err(error)?
                .iter()
                .map(|r| r.get(0))
                .collect();
            let primary_key = client
                .query(
                    "SELECT a.attname::text FROM pg_index i \
                     CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord) \
                     JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                     WHERE i.indrelid = $1 AND i.indisprimary ORDER BY k.ord",
                    &[oid],
                )
                .map_err(error)?
                .iter()
                .map(|r| r.get(0))
                .collect();
            let mut foreign_keys: Vec<(String, ForeignKey)> = Vec::new();
            for r in client
                .query(
                    "SELECT con.conname::text, p.relname::text, a.attname::text, b.attname::text \
                     FROM pg_constraint con \
                     JOIN pg_class p ON p.oid = con.confrelid \
                     CROSS JOIN LATERAL unnest(con.conkey, con.confkey) WITH ORDINALITY \
                       AS k(child, parent, ord) \
                     JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.child \
                     JOIN pg_attribute b ON b.attrelid = con.confrelid AND b.attnum = k.parent \
                     WHERE con.conrelid = $1 AND con.contype = 'f' \
                     AND p.relnamespace = (SELECT relnamespace FROM pg_class WHERE oid = $1) \
                     ORDER BY con.conname, k.ord",
                    &[oid],
                )
                .map_err(error)?
            {
                let constraint: String = r.get(0);
                match foreign_keys.last_mut() {
                    Some((name, fk)) if *name == constraint => {
                        fk.columns.push(r.get(2));
                        fk.referenced.push(r.get(3));
                    }
                    _ => foreign_keys.push((
                        constraint,
                        ForeignKey {
                            table: r.get(1),
                            columns: vec![r.get(2)],
                            referenced: vec![r.get(3)],
                        },
                    )),
                }
            }
            tables.push(DirectTable {
                name: name.clone(),
                schema: schema.to_owned(),
                columns,
                primary_key,
                foreign_keys: foreign_keys.into_iter().map(|(_, fk)| fk).collect(),
                stored: *stored,
            });
        }
        Ok(tables)
    }

    /// The tables and views of `schema` (or only those named) with what the catalogue says
    /// of them, for their ontology and shapes ([`crate::db_schema`], #278): comments, column
    /// types, `NOT NULL`, enum types, domains and their checks, unique keys and `CHECK`s.
    pub fn schema_tables(
        &self,
        schema: &str,
        only: &[String],
    ) -> Result<Vec<crate::db_schema::SchemaTable>, RmlError> {
        use crate::db_schema::{Check, SchemaColumn, SchemaTable};
        let direct = self.direct_tables(schema, only)?;
        let client = self.client(None)?;
        let mut client = client.lock().expect("not poisoned");
        let error = |e: postgres::Error| RmlError::Source {
            location: format!("schema {schema}"),
            message: error_message(&e),
        };
        let mut out = Vec::with_capacity(direct.len());
        for table in direct {
            let row = client
                .query_one(
                    "SELECT c.oid, c.relkind IN ('v', 'm'), obj_description(c.oid, 'pg_class') \
                     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relname = $2",
                    &[&schema, &table.name],
                )
                .map_err(error)?;
            let oid: u32 = row.get(0);
            let view: bool = row.get(1);
            let comment: Option<String> = row.get(2);
            let mut columns = Vec::new();
            for r in client
                .query(
                    "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), \
                     a.attnotnull OR t.typnotnull, col_description(a.attrelid, a.attnum), \
                     t.typtype::text, t.typname::text, t.oid, \
                     b.typtype::text, b.typname::text, b.oid, \
                     CASE WHEN t.typtype = 'd' THEN t.typtypmod ELSE a.atttypmod END \
                     FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
                     LEFT JOIN pg_type b ON b.oid = t.typbasetype AND t.typtype = 'd' \
                     WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
                     ORDER BY a.attnum",
                    &[&oid],
                )
                .map_err(error)?
            {
                let kind: String = r.get(4);
                let base_kind: Option<String> = r.get(7);
                let (base, base_oid, enum_oid) = if kind == "d" {
                    let b: Option<String> = r.get(8);
                    let b_oid: Option<u32> = r.get(9);
                    let enum_oid = (base_kind.as_deref() == Some("e"))
                        .then_some(b_oid)
                        .flatten();
                    (b.unwrap_or_default(), b_oid, enum_oid)
                } else {
                    let t_oid: u32 = r.get(6);
                    (r.get(5), Some(t_oid), (kind == "e").then_some(t_oid))
                };
                let _ = base_oid;
                let typmod: i32 = r.get(10);
                let max_length = (matches!(base.as_str(), "varchar" | "bpchar") && typmod > 4)
                    .then(|| u32::try_from(typmod - 4).ok())
                    .flatten();
                let enum_values = match enum_oid {
                    Some(e) => Some(
                        client
                            .query(
                                "SELECT enumlabel::text FROM pg_enum WHERE enumtypid = $1 \
                                 ORDER BY enumsortorder",
                                &[&e],
                            )
                            .map_err(error)?
                            .iter()
                            .map(|r| r.get(0))
                            .collect(),
                    ),
                    None => None,
                };
                let domain_checks = if kind == "d" {
                    let domain: u32 = r.get(6);
                    client
                        .query(
                            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
                             WHERE contypid = $1 AND contype = 'c' ORDER BY conname",
                            &[&domain],
                        )
                        .map_err(error)?
                        .iter()
                        .map(|r| r.get(0))
                        .collect()
                } else {
                    Vec::new()
                };
                columns.push(SchemaColumn {
                    name: r.get(0),
                    sql_type: r.get(1),
                    base,
                    max_length,
                    not_null: r.get(2),
                    comment: r.get(3),
                    enum_values,
                    domain_checks,
                });
            }
            let checks = client
                .query(
                    "SELECT con.conname::text, pg_get_constraintdef(con.oid), \
                     ARRAY(SELECT a.attname::text FROM unnest(con.conkey) AS k(n) \
                       JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.n \
                       ORDER BY a.attnum) \
                     FROM pg_constraint con WHERE con.conrelid = $1 AND con.contype = 'c' \
                     ORDER BY con.conname",
                    &[&oid],
                )
                .map_err(error)?
                .iter()
                .map(|r| Check {
                    name: r.get(0),
                    definition: r.get(1),
                    columns: r.get(2),
                })
                .collect();
            let uniques = client
                .query(
                    "SELECT ARRAY(SELECT a.attname::text FROM unnest(con.conkey) \
                       WITH ORDINALITY AS k(n, o) \
                       JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.n \
                       ORDER BY k.o) \
                     FROM pg_constraint con WHERE con.conrelid = $1 AND con.contype = 'u' \
                     ORDER BY con.conname",
                    &[&oid],
                )
                .map_err(error)?
                .iter()
                .map(|r| r.get(0))
                .collect();
            out.push(SchemaTable {
                table,
                view,
                comment,
                columns,
                uniques,
                checks,
            });
        }
        Ok(out)
    }

    /// The rows of the code table `table` of `schema`: each row's primary key values and its
    /// label, from `label` or else its first text column outside the key. Rows without a label
    /// are left out.
    pub fn code_list(
        &self,
        schema: &str,
        table: &crate::db_schema::SchemaTable,
        label: Option<&str>,
    ) -> Result<crate::db_schema::CodeList, RmlError> {
        let name = &table.table.name;
        let key = &table.table.primary_key;
        if key.is_empty() {
            return Err(RmlError::Mapping(format!(
                "code table {name} has no primary key to name its concepts"
            )));
        }
        let label = match label {
            Some(l) => {
                if !table.columns.iter().any(|c| c.name == l) {
                    return Err(RmlError::Mapping(format!("{name} has no column {l}")));
                }
                l.to_owned()
            }
            None => table
                .columns
                .iter()
                .find(|c| {
                    !key.contains(&c.name) && matches!(c.base.as_str(), "text" | "varchar" | "bpchar")
                })
                .map(|c| c.name.clone())
                .ok_or_else(|| {
                    RmlError::Mapping(format!(
                        "code table {name} has no text column for labels: name one with {name}=COLUMN"
                    ))
                })?,
        };
        let q = crate::direct::quote;
        let sql = format!(
            "SELECT {}, {}::text FROM {}.{} WHERE {} IS NOT NULL ORDER BY {}",
            key.iter()
                .map(|k| format!("{}::text", q(k)))
                .collect::<Vec<_>>()
                .join(", "),
            q(&label),
            q(schema),
            q(name),
            q(&label),
            key.iter().map(|k| q(k)).collect::<Vec<_>>().join(", ")
        );
        let client = self.client(None)?;
        let mut client = client.lock().expect("not poisoned");
        let rows = client
            .query(sql.as_str(), &[])
            .map_err(|e| RmlError::Source {
                location: format!("{schema}.{name}"),
                message: error_message(&e),
            })?;
        let rows = rows
            .iter()
            .map(|r| {
                let values: Vec<String> = (0..key.len()).map(|i| r.get(i)).collect();
                (values, r.get(key.len()))
            })
            .collect();
        Ok(crate::db_schema::CodeList {
            table: name.clone(),
            label,
            rows,
        })
    }

    fn connect(&self, config: Target, key: String) -> Result<Arc<Mutex<Client>>, RmlError> {
        let mut clients = self.clients.lock().expect("not poisoned");
        // A connection the database closed (a restart, an idle timeout) is opened again; one
        // in use is not checked, so a long query does not hold up the others.
        if let Some(c) = clients.get(&key)
            && !c.try_lock().is_ok_and(|c| c.is_closed())
        {
            return Ok(c.clone());
        }
        let client = Arc::new(Mutex::new(open(config)?));
        clients.insert(key, client.clone());
        Ok(client)
    }

    /// An idle Facade-X session of `url`, or a new one while fewer than [`Self::sessions`]
    /// are open; else waits for one, as long as the statement timeout.
    fn session(&self, url: &str) -> Result<Client, RmlError> {
        let until = self
            .statement_timeout
            .map(|t| std::time::Instant::now() + t);
        let mut pools = self.pools.pools.lock().expect("not poisoned");
        loop {
            let pool = pools.entry(url.to_owned()).or_default();
            while let Some(client) = pool.idle.pop() {
                if client.is_closed() {
                    pool.open -= 1;
                } else {
                    return Ok(client);
                }
            }
            if pool.open < self.sessions {
                pool.open += 1;
                drop(pools);
                let opened = parse(url, url).and_then(open);
                if opened.is_err() {
                    self.retire(url, false);
                }
                return opened;
            }
            pools = match until {
                None => self.pools.freed.wait(pools).expect("not poisoned"),
                Some(until) => {
                    let left = until.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return Err(RmlError::Source {
                            location: parse(url, url)
                                .map(|(c, _, _)| location(&c))
                                .unwrap_or_default(),
                            message: format!(
                                "no free session within the statement timeout \
                                 (--fx-sessions {})",
                                self.sessions
                            ),
                        });
                    }
                    self.pools
                        .freed
                        .wait_timeout(pools, left)
                        .expect("not poisoned")
                        .0
                }
            };
        }
    }

    /// Gives a session back for the next call.
    fn give_back(&self, url: &str, client: Client) {
        let mut pools = self.pools.pools.lock().expect("not poisoned");
        pools.entry(url.to_owned()).or_default().idle.push(client);
        self.pools.freed.notify_one();
    }

    /// Counts a session of `url` as closed (it was dropped, or never opened). With `idle`,
    /// the idle ones are closed too: the database closed one, so likely all (a restart).
    fn retire(&self, url: &str, idle: bool) {
        let mut pools = self.pools.pools.lock().expect("not poisoned");
        if let Some(pool) = pools.get_mut(url) {
            pool.open = pool.open.saturating_sub(1);
            if idle {
                pool.open = pool.open.saturating_sub(pool.idle.len());
                pool.idle.clear();
            }
        }
        self.pools.freed.notify_all();
    }
}

impl<L: Loader> Loader for DatabaseLoader<L> {
    fn load(&self, source: &LogicalSource) -> Result<Vec<u8>, RmlError> {
        self.files.load(source)
    }

    fn table(&self, source: &LogicalSource) -> Result<Table, RmlError> {
        let sql = match &source.source {
            SourceRef::Table(t) => format!("SELECT * FROM {t}"),
            SourceRef::Query(q) => q.trim().trim_end_matches(';').to_owned(),
            _ => return self.files.table(source),
        };
        let client = self.client(source.database.as_ref())?;
        let mut client = client.lock().expect("not poisoned");
        let error = |e: postgres::Error| RmlError::Source {
            location: sql.clone(),
            message: match e.as_db_error() {
                Some(db) => db.message().to_owned(),
                None => e.to_string(),
            },
        };
        // The column types, then the rows as text (one round trip, any type).
        let statement = client.prepare(&sql).map_err(error)?;
        let columns: Vec<(String, Type)> = statement
            .columns()
            .iter()
            .map(|c| (c.name().to_owned(), c.type_().clone()))
            .collect();
        let mut rows = Vec::new();
        for message in client.simple_query(&sql).map_err(error)? {
            if let SimpleQueryMessage::Row(row) = message {
                rows.push(
                    columns
                        .iter()
                        .enumerate()
                        .map(|(i, (_, t))| row.get(i).map(|v| natural(v, t)))
                        .collect(),
                );
            }
        }
        Ok(Table {
            columns: columns.into_iter().map(|(n, _)| n).collect(),
            rows,
        })
    }
}

/// A condition as SQL on the column `column` of type `t`, when PostgreSQL compares as
/// SPARQL does: numbers with numbers, booleans and text for equality (a deterministic
/// collation is equal only on equal text). Text goes in `params`, as `$n`.
/// `srid` is the SRID a geometry column is declared with (`geometry(Point, 25833)`), if any.
fn sql_condition(
    column: &str,
    t: &Type,
    srid: Option<i32>,
    c: &Condition,
    params: &mut Vec<String>,
) -> Option<String> {
    let dt = c.value.datatype();
    let value = c.value.value();
    if c.op == Op::Intersects {
        return (t.name() == "geometry" && dt.as_str() == WKT_LITERAL)
            .then(|| box_condition(column, t.schema(), srid, value))
            .flatten();
    }
    // As SPARQL compares: integers and decimals exactly with each other, and with a double
    // as doubles. (`real` is left out: its text is read as a double, while PostgreSQL
    // compares its single-precision value.)
    let exact = matches!(*t, Type::INT2 | Type::INT4 | Type::INT8 | Type::NUMERIC)
        && (dt == xsd::INTEGER || dt == xsd::DECIMAL);
    // PostgreSQL refuses a double constant out of range (`1e400`, `1e-400`), where SPARQL
    // reads infinity or zero: only constants it reads as the same double.
    let double = *t == Type::FLOAT8
        && [xsd::INTEGER, xsd::DECIMAL, xsd::DOUBLE].contains(&dt)
        && is_plain_double(value);
    if (exact || double) && is_sql_number(value) {
        return Some(format!("{column} {} {value}", c.op.sql()));
    }
    if c.op != Op::Eq {
        return None;
    }
    if *t == Type::BOOL && dt == xsd::BOOLEAN {
        let b = match value {
            "true" | "1" => "true",
            "false" | "0" => "false",
            _ => return None,
        };
        return Some(format!("{column} = {b}"));
    }
    // PostgreSQL text holds no NUL: such a constant equals no value, and is refused.
    if matches!(*t, Type::TEXT | Type::VARCHAR) && dt == xsd::STRING && !value.contains('\0') {
        params.push(value.to_owned());
        return Some(format!("{column} = {PARAM}{}{PARAM}", params.len() - 1));
    }
    None
}

/// `column` equals one of `values` (as [`sql_condition`]'s `=`), as one `IN` list, which
/// PostgreSQL hashes (an `OR` of equalities is checked term by term on every row), or `None`
/// when one of them cannot be checked that way: then the rows are not narrowed, and `params`
/// is as it was. Values the column can never hold are left out (none left: `false`).
fn one_of_condition(
    column: &str,
    t: &Type,
    values: &[Literal],
    params: &mut Vec<String>,
) -> Option<String> {
    let before = params.len();
    let mut terms = Vec::with_capacity(values.len());
    let equals = format!("{column} = ");
    for value in values {
        if never_joins(t, value) {
            continue;
        }
        let c = Condition {
            key: String::new(),
            op: Op::Eq,
            value: value.clone(),
        };
        match sql_condition(column, t, None, &c, params)
            .and_then(|w| w.strip_prefix(&equals).map(str::to_owned))
        {
            // A constant of the column's own type, so the list does not turn numeric
            // (which would compare otherwise and leave an index unused).
            Some(term) if *t == Type::FLOAT8 => terms.push(format!("{term}::float8")),
            Some(term) => terms.push(term),
            None => {
                params.truncate(before);
                return None;
            }
        }
    }
    Some(if terms.is_empty() {
        "false".to_owned()
    } else {
        format!("{column} IN ({})", terms.join(", "))
    })
}

/// A value of this column type is never this literal: [`natural`] gives it another datatype.
fn never_joins(t: &Type, value: &Literal) -> bool {
    let dt = value.datatype();
    match *t {
        // Out of the column's range, a value would also make the list numeric.
        Type::INT2 | Type::INT4 | Type::INT8 => {
            let range = match *t {
                Type::INT2 => i128::from(i16::MIN)..=i128::from(i16::MAX),
                Type::INT4 => i128::from(i32::MIN)..=i128::from(i32::MAX),
                _ => i128::from(i64::MIN)..=i128::from(i64::MAX),
            };
            dt != xsd::INTEGER
                || !value
                    .value()
                    .trim_start_matches('+')
                    .parse::<i128>()
                    .is_ok_and(|n| range.contains(&n))
        }
        // NaN and infinities are strings.
        Type::NUMERIC => dt != xsd::DECIMAL && dt != xsd::STRING,
        Type::FLOAT4 | Type::FLOAT8 => dt != xsd::DOUBLE,
        Type::BOOL => dt != xsd::BOOLEAN,
        // PostgreSQL text holds no NUL.
        Type::TEXT | Type::VARCHAR => dt != xsd::STRING || value.value().contains('\0'),
        _ => false,
    }
}

/// `column` may intersect the WKT literal `wkt`: its bounding box overlaps the literal's,
/// for the rows whose SRID is the literal's CRS as TripleTrail reads the column (SRID 0 and
/// 4326 as CRS84, 4258 latitude first, others as written). Rows with another SRID are all
/// kept, since TripleTrail may compare across CRSs; so is any row when `wkt` cannot be read.
/// With the column's declared SRID (`srid`), the test is a plain `&&` a GiST index serves.
/// PostGIS names are qualified with `schema`, the geometry type's, which need not be on the
/// search path.
fn box_condition(column: &str, schema: &str, srid: Option<i32>, wkt: &str) -> Option<String> {
    let schema = format!("\"{}\"", schema.replace('"', "\"\""));
    let wkt = wkt.trim_start();
    let (srids, body, swap) = match wkt.strip_prefix('<') {
        Some(rest) => {
            let (iri, body) = rest.split_once('>')?;
            match iri.strip_prefix("http://www.opengis.net/def/crs/EPSG/0/") {
                Some("4326") => (vec![0, 4326], body, true),
                Some("4258") => (vec![4258], body, true),
                Some(code) if !code.is_empty() && code.bytes().all(|b| b.is_ascii_digit()) => {
                    (vec![code.parse::<i32>().ok()?], body, false)
                }
                _ if iri == "http://www.opengis.net/def/crs/OGC/1.3/CRS84" => {
                    (vec![0, 4326], body, false)
                }
                _ => return None,
            }
        }
        None => (vec![0, 4326], wkt, false),
    };
    let [mut x0, mut y0, mut x1, mut y1] = wkt_envelope(body)?;
    if swap {
        (x0, y0, x1, y1) = (y0, x0, y1, x1);
    }
    match srid {
        // Every value has this SRID.
        Some(srid) if srid != 0 => srids.contains(&srid).then(|| {
            format!(
                "{column} OPERATOR({schema}.&&) \
                     {schema}.ST_MakeEnvelope({x0}, {y0}, {x1}, {y1}, {srid})"
            )
        }),
        _ => {
            let srids: Vec<String> = srids.iter().map(i32::to_string).collect();
            Some(format!(
                "(CASE WHEN {schema}.ST_SRID({column}) IN ({}) THEN {column} \
                 OPERATOR({schema}.&&) {schema}.ST_MakeEnvelope({x0}, {y0}, {x1}, {y1}, \
                 {schema}.ST_SRID({column})) ELSE true END)",
                srids.join(", ")
            ))
        }
    }
}

/// The SRID in a PostGIS geometry type modifier (as PostGIS's `TYPMOD_GET_SRID`: 21 bits,
/// signed, above the type and dimension bits).
fn typmod_srid(typmod: i32) -> i32 {
    ((typmod & 0x0FFF_FF00) - (typmod & 0x1000_0000)) >> 8
}

/// The bounding box (min x, min y, max x, max y) of a WKT geometry, from its coordinates
/// (the first two of each, after `Z`, `M` or `ZM` tags), or `None` when it has none or
/// reads oddly.
fn wkt_envelope(wkt: &str) -> Option<[f64; 4]> {
    let mut dims = 2;
    let mut coord: Vec<f64> = Vec::new();
    let mut env = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    let mut found = false;
    let mut flush = |coord: &mut Vec<f64>, dims: usize| -> Option<()> {
        if coord.is_empty() {
            return Some(());
        }
        if coord.len() != dims {
            return None;
        }
        let (x, y) = (coord[0], coord[1]);
        env = [env[0].min(x), env[1].min(y), env[2].max(x), env[3].max(y)];
        found = true;
        coord.clear();
        Some(())
    };
    let bytes = wkt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            match wkt[start..i].to_ascii_uppercase().as_str() {
                "Z" | "M" => dims = 3,
                "ZM" => dims = 4,
                "EMPTY" => {}
                // A geometry type: its own dimensions follow.
                _ => dims = 2,
            }
            continue;
        }
        match b {
            b'(' | b')' | b',' => flush(&mut coord, dims)?,
            b' ' | b'\t' | b'\n' | b'\r' => {}
            _ => {
                let start = i;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E')
                {
                    i += 1;
                }
                let v: f64 = wkt.get(start..i)?.parse().ok()?;
                if !v.is_finite() {
                    return None;
                }
                coord.push(v);
                if i == start {
                    return None;
                }
                continue;
            }
        }
        i += 1;
    }
    flush(&mut coord, dims)?;
    found.then_some(env)
}

/// A number that is a finite double, and zero only when its digits are.
fn is_plain_double(s: &str) -> bool {
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() && f != 0.0 => true,
        Ok(0.0) => {
            let mantissa = s.split(['e', 'E']).next().unwrap_or(s);
            mantissa.bytes().all(|b| !(b'1'..=b'9').contains(&b))
        }
        _ => false,
    }
}

/// A decimal or scientific number, as both SPARQL and SQL read it (no NaN or infinities).
fn is_sql_number(s: &str) -> bool {
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (mantissa, exponent) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e.strip_prefix(['+', '-']).unwrap_or(e))),
        None => (s, None),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |d: &str| d.bytes().all(|b| b.is_ascii_digit());
    (!int.is_empty() || !frac.is_empty())
        && digits(int)
        && digits(frac)
        && exponent.is_none_or(|e| !e.is_empty() && digits(e))
}

const WKT_LITERAL: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";

/// Marks a parameter in conditions: `\0`, its index, `\0` (no identifier or constant holds NUL).
const PARAM: char = '\0';

impl<L: Send + Sync> Databases for DatabaseLoader<L> {
    fn rows(
        &self,
        name: Option<&str>,
        query: &DatabaseQuery,
        select: &Select<'_>,
        max_rows: usize,
    ) -> Result<Rows, FxError> {
        let fx = |e: RmlError| FxError::Backend(e.to_string().into());
        let url = match name {
            Some(name) => self.named.get(name).ok_or_else(|| {
                FxError::Option(format!(
                    "database {name:?}: no such connection (lift --connection {name}=<url>)"
                ))
            })?,
            None => self.default.as_ref().ok_or_else(|| {
                FxError::Option(
                    "a table or sql source, but no database to read it from (lift --database)"
                        .into(),
                )
            })?,
        };
        let mut retried = false;
        loop {
            let mut session = self.session(url).map_err(fx)?;
            // Retires the session's slot unless it is given back or retired below, so a
            // panic does not keep it taken.
            let mut slot = Slot {
                retire: Some(|| self.retire(url, false)),
            };
            let cancelled = Arc::new(AtomicBool::new(false));
            let cancel = canceller(&session, url, cancelled.clone()).map_err(fx)?;
            let result = self.read(&mut session, url, &cancel, query, select, max_rows);
            // What a query left on the session (advisory locks, settings, LISTEN) goes, so
            // the next caller starts clean; a session that cannot be reset is not used again.
            // A session a cancel request was sent to is not used again either: the request
            // reaches the backend, not a statement, and one still in flight would cancel
            // whatever runs next.
            let closed = session.is_closed();
            if closed
                || cancelled.load(Ordering::SeqCst)
                || session.batch_execute("DISCARD ALL").is_err()
            {
                drop(session);
                slot.retire = None;
                self.retire(url, closed);
            } else {
                slot.retire = None;
                self.give_back(url, session);
            }
            match result {
                Ok(rows) => return Ok(rows),
                // The database closed the connection (a restart, an idle timeout): once
                // more, on a new one (the query is read-only, so running it again is safe).
                Err(_) if closed && !retried => retried = true,
                Err(e) => return Err(e),
            }
        }
    }
}

/// Runs `retire` when dropped, unless it was taken.
struct Slot<F: FnOnce()> {
    retire: Option<F>,
}

impl<F: FnOnce()> Drop for Slot<F> {
    fn drop(&mut self) {
        if let Some(retire) = self.retire.take() {
            retire();
        }
    }
}

/// The Facade-X sessions of each connection string.
#[derive(Default)]
struct Pools {
    pools: Mutex<HashMap<String, Pool>>,
    freed: std::sync::Condvar,
}

#[derive(Default)]
struct Pool {
    idle: Vec<Client>,
    /// Sessions open: idle or in use.
    open: usize,
}

/// A new session.
fn open(target: Target) -> Result<Client, RmlError> {
    let (config, mode, root_cert) = target;
    // With its cause: "error performing TLS handshake: invalid peer certificate: …".
    let error = |e: postgres::Error| RmlError::Source {
        location: location(&config),
        message: match std::error::Error::source(&e) {
            Some(cause) => format!("{e}: {cause}"),
            None => e.to_string(),
        },
    };
    if mode == tls::Mode::Disable {
        config.connect(postgres::NoTls).map_err(error)
    } else {
        config
            .connect(tls::connector(mode, root_cert.as_deref())?)
            .map_err(error)
    }
}

/// Cancels whatever the session runs, from another thread (its own connection to the server).
type Cancel = Arc<dyn Fn() + Send + Sync>;

/// `sent` is set before the first request goes out.
fn canceller(client: &Client, url: &str, sent: Arc<AtomicBool>) -> Result<Cancel, RmlError> {
    let token = client.cancel_token();
    let (_, mode, root_cert) = parse(url, url)?;
    Ok(Arc::new(move || {
        sent.store(true, Ordering::SeqCst);
        // Best effort: if the cancel request cannot be sent, the query still ends at the
        // statement timeout, and the reader stops at the deadline.
        let _ = if mode == tls::Mode::Disable {
            token.cancel_query(postgres::NoTls)
        } else {
            match tls::connector(mode, root_cert.as_deref()) {
                Ok(c) => token.cancel_query(c),
                Err(_) => return,
            }
        };
    }))
}

/// Cancels the session's query when the time is up, and again every quarter second until
/// the read ends, so SQL that switches `statement_timeout` off still stops. Dropping it ends
/// the watch, and waits for it, so no cancel request is sent after.
struct Deadline {
    fired: Arc<AtomicBool>,
    stop: Option<mpsc::Sender<()>>,
    watch: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(watch) = self.watch.take() {
            let _ = watch.join();
        }
    }
}

impl Deadline {
    fn new(timeout: std::time::Duration, cancel: Cancel) -> Self {
        let fired = Arc::new(AtomicBool::new(false));
        let (stop, stopped) = mpsc::channel::<()>();
        let flag = fired.clone();
        let watch = std::thread::spawn(move || {
            use mpsc::RecvTimeoutError::Timeout;
            if let Err(Timeout) = stopped.recv_timeout(timeout) {
                flag.store(true, Ordering::SeqCst);
                loop {
                    cancel();
                    if !matches!(
                        stopped.recv_timeout(std::time::Duration::from_millis(250)),
                        Err(Timeout)
                    ) {
                        break;
                    }
                }
            }
        });
        Self {
            fired,
            stop: Some(stop),
            watch: Some(watch),
        }
    }

    fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

impl<L> DatabaseLoader<L> {
    /// Reads the rows of one query on `client`, in a read-only transaction that is rolled
    /// back. The rows stream in PostgreSQL's `COPY` text format, so the row and byte caps and
    /// the deadline are checked as they arrive, not after a whole batch is in memory.
    fn read(
        &self,
        client: &mut Client,
        url: &str,
        cancel: &Cancel,
        query: &DatabaseQuery,
        select: &Select<'_>,
        max_rows: usize,
    ) -> Result<Rows, FxError> {
        let sql = match query {
            DatabaseQuery::Table(t) => format!("TABLE {t}"),
            DatabaseQuery::Sql(q) => q.clone(),
        };
        let deadline = self
            .statement_timeout
            .map(|t| (t, Deadline::new(t, cancel.clone())));
        let timed_out = || deadline.as_ref().is_some_and(|(_, d)| d.fired());
        let failed = |message: String| {
            FxError::Backend(
                match &deadline {
                    Some((t, d)) if d.fired() => format!(
                        "database query {sql:?}: canceled at the statement timeout ({} ms)",
                        t.as_millis()
                    ),
                    _ => format!("database query {sql:?}: {message}"),
                }
                .into(),
            )
        };
        let error = |e: postgres::Error| {
            failed(match e.as_db_error() {
                Some(db) => db.message().to_owned(),
                None => e.to_string(),
            })
        };
        // Read-only, so the query cannot change the database; preparing it first refuses
        // more than one statement, and gives the column types.
        let mut tx = client
            .build_transaction()
            .read_only(true)
            .start()
            .map_err(error)?;
        if let Some((timeout, _)) = &deadline {
            tx.batch_execute(&format!(
                "SET LOCAL statement_timeout = {}",
                timeout.as_millis().max(1)
            ))
            .map_err(error)?;
        }
        let statement = tx.prepare(&sql).map_err(error)?;
        let mut columns: Vec<(String, Type)> = statement
            .columns()
            .iter()
            .map(|c| (c.name().to_owned(), c.type_().clone()))
            .collect();
        // The SRIDs geometry columns read straight from a table are declared with, when a
        // spatial condition may use them.
        let mut srids: Vec<Option<i32>> = vec![None; columns.len()];
        if select.conditions.iter().any(|c| c.op == Op::Intersects) {
            for (i, c) in statement.columns().iter().enumerate() {
                if c.type_().name() == "geometry"
                    && let (Some(table), Some(id)) = (c.table_oid(), c.column_id())
                {
                    // The catalog only, so this works wherever PostGIS is installed.
                    srids[i] = tx
                        .query_opt(
                            "SELECT atttypmod FROM pg_catalog.pg_attribute \
                             WHERE attrelid = $1 AND attnum = $2 AND atttypmod >= 0",
                            &[&table, &id],
                        )
                        .map_err(error)?
                        .map(|row| typmod_srid(row.get(0)));
                }
            }
        }
        drop(statement);
        let quote = |n: &str| format!("\"{}\"", n.replace('"', "\"\""));
        // A name given twice cannot be selected: then every column and row is read.
        let mut seen = std::collections::HashSet::new();
        let unique = columns.iter().all(|(n, _)| seen.insert(n.as_str()));
        // The conditions PostgreSQL checks as SPARQL would (or more loosely).
        let mut params: Vec<String> = Vec::new();
        let mut wheres = Vec::new();
        if unique {
            for c in select.conditions {
                if let Some(i) = columns.iter().position(|(n, _)| (select.key)(n) == c.key)
                    && let (n, t) = &columns[i]
                    && let Some(w) = sql_condition(&quote(n), t, srids[i], c, &mut params)
                {
                    wheres.push(w);
                }
            }
            // A bind join's values: each one's equality, any of them (all or none).
            for o in select.one_of {
                if let Some((n, t)) = columns.iter().find(|(n, _)| (select.key)(n) == o.key)
                    && let Some(w) = one_of_condition(&quote(n), t, &o.values, &mut params)
                {
                    wheres.push(w);
                }
            }
        }
        let prune = unique && columns.iter().any(|(n, _)| !(select.wanted)(n));
        if prune {
            columns.retain(|(n, _)| (select.wanted)(n));
        }
        let list = if prune {
            columns
                .iter()
                .map(|(n, _)| quote(n))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            "*".into()
        };
        // A new line before the parenthesis, so a line comment ending the query does not
        // swallow it.
        let mut wrapped = format!("SELECT {list} FROM ({sql}\n) AS tripletrail_fx_rows");
        if !wheres.is_empty() {
            wrapped.push_str(" WHERE ");
            wrapped.push_str(&wheres.join(" AND "));
        }
        if let Some(limit) = select.limit {
            wrapped.push_str(&format!(" LIMIT {limit}"));
        }
        // What explain shows: the query as written when nothing was added to it.
        let shown = if prune || !wheres.is_empty() || select.limit.is_some() {
            placeholders(&wrapped, |i| format!("${}", i + 1))
        } else {
            sql.clone()
        };
        // COPY takes no parameters: the text constants go in as escaped literals (they hold
        // no NUL, see `sql_condition`). The extended protocol runs one statement only, and
        // the query was prepared alone above, so it is one whole query inside the parentheses.
        let copy = format!(
            "COPY ({}) TO STDOUT",
            placeholders(&wrapped, |i| literal(&params[i]))
        );
        // The same statement on the same connection within the TTL: the rows read then.
        let key = (url.to_owned(), copy.clone(), max_rows);
        let hit = self
            .cache_ttl
            .and_then(|ttl| self.results.lock().expect("not poisoned").get(&key, ttl));
        if let Some(rows) = hit {
            tx.rollback().map_err(error)?;
            // Copied outside the cache's lock.
            return Ok(Rows {
                columns: rows.columns.clone(),
                rows: rows.rows.clone(),
                statement: Some(shown),
                params,
            });
        }
        let (columns, types): (Vec<String>, Vec<Type>) = columns.into_iter().unzip();
        let mut reader = tx.copy_out(&copy).map_err(error)?;
        let io = |e: std::io::Error| match e
            .get_ref()
            .and_then(|e| e.downcast_ref::<postgres::Error>())
        {
            Some(e) => error_message(e),
            None => e.to_string(),
        };
        let mut rows = Vec::new();
        let mut line = Vec::new();
        let mut bytes = 0usize;
        loop {
            if timed_out() {
                return Err(failed(String::new()));
            }
            let chunk = reader.fill_buf().map_err(|e| failed(io(e)))?;
            if chunk.is_empty() {
                break;
            }
            let n = chunk.len();
            bytes += n;
            if self.max_bytes.is_some_and(|max| bytes > max) {
                cancel();
                return Err(FxError::Option(format!(
                    "database query {sql:?} gives more than {} bytes",
                    self.max_bytes.unwrap_or_default()
                )));
            }
            for piece in chunk.split_inclusive(|b| *b == b'\n') {
                line.extend_from_slice(piece);
                if line.last() != Some(&b'\n') {
                    continue;
                }
                line.pop();
                if rows.len() == max_rows {
                    cancel();
                    return Err(FxError::Option(format!(
                        "database query {sql:?} gives more than {max_rows} rows \
                         (database.max-rows)"
                    )));
                }
                let values = copy_fields(&line, types.len()).map_err(&failed)?;
                rows.push(
                    values
                        .iter()
                        .zip(&types)
                        .map(|(v, t)| {
                            v.as_deref().map(|v| {
                                let c = natural(v, t);
                                Literal::new_typed_literal(c.lexical, c.datatype)
                            })
                        })
                        .collect(),
                );
                line.clear();
            }
            reader.consume(n);
        }
        drop(reader);
        // Read-only: nothing to keep.
        tx.rollback().map_err(error)?;
        let rows = Rows {
            columns,
            rows,
            statement: Some(shown),
            params,
        };
        if let Some(ttl) = self.cache_ttl {
            let kept = Arc::new(rows.clone());
            let size = footprint(&kept);
            self.results
                .lock()
                .expect("not poisoned")
                .put(key, kept, size, ttl);
        }
        Ok(rows)
    }
}

/// About the memory `rows` takes: per cell the `Option<Literal>` and its value and datatype
/// strings (with what the allocator adds to each), per row its vector.
fn footprint(rows: &Rows) -> usize {
    const ALLOCATION: usize = 16;
    let cell = std::mem::size_of::<Option<Literal>>();
    let row = std::mem::size_of::<Vec<Option<Literal>>>() + ALLOCATION;
    rows.rows
        .iter()
        .map(|r| {
            row + r
                .iter()
                .map(|v| {
                    cell + v.as_ref().map_or(0, |l| {
                        l.value().len() + l.datatype().as_str().len() + 2 * ALLOCATION
                    })
                })
                .sum::<usize>()
        })
        .sum::<usize>()
        + rows
            .columns
            .iter()
            .map(|c| c.len() + ALLOCATION)
            .sum::<usize>()
}

/// A cached result's connection string, statement and row cap.
type CacheKey = (String, String, usize);

/// Facade-X results by connection, statement and row cap, with when they were read and about
/// the memory they take ([`footprint`]).
#[derive(Default)]
struct ResultCache {
    entries: HashMap<CacheKey, (std::time::Instant, Arc<Rows>, usize)>,
    bytes: usize,
}

impl ResultCache {
    /// Bytes of results kept at most: a result larger than a quarter of it is not kept.
    const BYTES: usize = 256 << 20;
    /// Results kept at most.
    const ENTRIES: usize = 256;

    fn get(&mut self, key: &CacheKey, ttl: std::time::Duration) -> Option<Arc<Rows>> {
        match self.entries.get(key) {
            Some((at, rows, _)) if at.elapsed() < ttl => Some(rows.clone()),
            Some(_) => {
                if let Some((_, _, size)) = self.entries.remove(key) {
                    self.bytes -= size;
                }
                None
            }
            None => None,
        }
    }

    fn put(&mut self, key: CacheKey, rows: Arc<Rows>, size: usize, ttl: std::time::Duration) {
        if size > Self::BYTES / 4 {
            return;
        }
        if let Some((_, _, old)) = self.entries.remove(&key) {
            self.bytes -= old;
        }
        if self.entries.len() >= Self::ENTRIES || self.bytes + size > Self::BYTES {
            self.entries.retain(|_, (at, _, _)| at.elapsed() < ttl);
            self.bytes = self.entries.values().map(|(_, _, s)| s).sum();
            // Still full: the oldest go first.
            while self.entries.len() >= Self::ENTRIES || self.bytes + size > Self::BYTES {
                let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, (at, _, _))| *at)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                if let Some((_, _, s)) = self.entries.remove(&oldest) {
                    self.bytes -= s;
                }
            }
        }
        self.bytes += size;
        self.entries
            .insert(key, (std::time::Instant::now(), rows, size));
    }
}

fn error_message(e: &postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => db.message().to_owned(),
        None => e.to_string(),
    }
}

/// `text` with each parameter marker `sql_condition` wrote replaced by `with(index)`.
fn placeholders(text: &str, with: impl Fn(usize) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut parts = text.split(PARAM);
    out.push_str(parts.next().unwrap_or_default());
    while let (Some(index), Some(rest)) = (parts.next(), parts.next()) {
        out.push_str(&with(index.parse().expect("a marker holds its index")));
        out.push_str(rest);
    }
    out
}

/// A text constant as an SQL literal, read the same whatever `standard_conforming_strings` says.
fn literal(value: &str) -> String {
    format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

/// The fields of one line of `COPY … TO STDOUT` text: tab-separated, `\N` for NULL, and
/// backslash escapes.
fn copy_fields(line: &[u8], columns: usize) -> Result<Vec<Option<String>>, String> {
    if columns == 0 {
        return Ok(Vec::new());
    }
    let fields: Vec<Option<String>> = line
        .split(|b| *b == b'\t')
        .map(|field| {
            if field == b"\\N" {
                return Ok(None);
            }
            let mut out = Vec::with_capacity(field.len());
            let mut i = 0;
            while i < field.len() {
                let b = field[i];
                i += 1;
                if b != b'\\' || i == field.len() {
                    out.push(b);
                    continue;
                }
                let e = field[i];
                i += 1;
                out.push(match e {
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => 11,
                    b'0'..=b'7' => {
                        let mut v = u32::from(e - b'0');
                        for _ in 0..2 {
                            match field.get(i) {
                                Some(d @ b'0'..=b'7') => {
                                    v = v * 8 + u32::from(d - b'0');
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                        v as u8
                    }
                    b'x' if field.get(i).is_some_and(u8::is_ascii_hexdigit) => {
                        let mut v = 0u8;
                        for _ in 0..2 {
                            match field.get(i).and_then(|d| (*d as char).to_digit(16)) {
                                Some(d) => {
                                    v = v * 16 + d as u8;
                                    i += 1;
                                }
                                None => break,
                            }
                        }
                        v
                    }
                    other => other,
                });
            }
            String::from_utf8(out)
                .map(Some)
                .map_err(|_| "a value is not UTF-8".to_owned())
        })
        .collect::<Result<_, _>>()?;
    if fields.len() != columns {
        return Err(format!(
            "a row of {} values, where {columns} columns were described",
            fields.len()
        ));
    }
    Ok(fields)
}

/// The connection of a `d2rq:Database`: a `jdbc:postgresql:` DSN (or a PostgreSQL URL) with
/// the database's user name and password.
fn config_of(d: &Database) -> Result<Target, RmlError> {
    if let Some(driver) = &d.driver
        && !driver.to_ascii_lowercase().contains("postgres")
    {
        return Err(RmlError::Unsupported(format!(
            "the database driver {driver} (only PostgreSQL is read)"
        )));
    }
    let dsn = d.dsn.strip_prefix("jdbc:").unwrap_or(&d.dsn);
    let url = match dsn.split_once("://") {
        Some(("postgres" | "postgresql", _)) => dsn.to_owned(),
        Some((scheme, _)) if scheme.contains("mysql") || scheme.contains("sqlserver") => {
            return Err(RmlError::Unsupported(format!(
                "the database {} (only PostgreSQL is read)",
                d.dsn
            )));
        }
        // `http://localhost:5432/db` (YARRRML's access) and the like: the host and database.
        Some((_, rest)) => format!("postgresql://{rest}"),
        None => dsn.to_owned(),
    };
    let (mut config, mode, root_cert) = parse(&url, &d.dsn)?;
    if let Some(u) = &d.username {
        config.user(u);
    }
    if let Some(p) = &d.password {
        config.password(p);
    }
    Ok((config, mode, root_cert))
}

/// A connection: its configuration, its TLS mode and the PEM file of its trusted roots.
type Target = (postgres::Config, tls::Mode, Option<String>);

fn parse(url: &str, shown: &str) -> Result<Target, RmlError> {
    let split = tls::split(url)?;
    let config = split
        .connection
        .parse()
        .map_err(|e: postgres::Error| RmlError::Source {
            location: shown.to_owned(),
            message: format!("not a PostgreSQL connection string: {e}"),
        })?;
    Ok((config, split.mode, split.root_cert))
}

/// Where a connection goes, without its password.
fn location(config: &postgres::Config) -> String {
    let hosts: Vec<String> = config
        .get_hosts()
        .iter()
        .map(|h| match h {
            postgres::config::Host::Tcp(h) => h.clone(),
            #[cfg(unix)]
            postgres::config::Host::Unix(p) => p.display().to_string(),
        })
        .collect();
    format!(
        "postgresql://{}/{}",
        hosts.join(","),
        config.get_dbname().unwrap_or_default()
    )
}

/// The natural RDF literal of a value PostgreSQL gives as `text` (in its default `ISO`
/// date style).
fn natural(text: &str, t: &Type) -> Cell {
    let typed = |lexical: String, datatype: NamedNodeRef<'_>| Cell::new(lexical, datatype);
    if matches!(t.name(), "geometry" | "geography")
        && let Some(wkt) = crate::wkb::wkt_of_hex(text)
    {
        return typed(wkt, NamedNodeRef::new_unchecked(WKT_LITERAL));
    }
    match *t {
        Type::BOOL => typed(
            if text == "t" { "true" } else { "false" }.to_owned(),
            xsd::BOOLEAN,
        ),
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => typed(text.to_owned(), xsd::INTEGER),
        // NaN and (PostgreSQL 14+) infinities have no xsd:decimal: they stay text.
        Type::NUMERIC if !matches!(text, "NaN" | "Infinity" | "-Infinity") => {
            typed(decimal(text), xsd::DECIMAL)
        }
        Type::FLOAT4 | Type::FLOAT8 => typed(double(text), xsd::DOUBLE),
        // `infinity` and `-infinity` have no XSD form: they stay text,
        // nor dates before the common era (`0044-03-15 BC`).
        Type::DATE | Type::TIMESTAMP | Type::TIMESTAMPTZ
            if matches!(text, "infinity" | "-infinity") || text.ends_with(" BC") =>
        {
            typed(text.to_owned(), xsd::STRING)
        }
        Type::DATE => typed(text.to_owned(), xsd::DATE),
        Type::TIME => typed(text.to_owned(), xsd::TIME),
        Type::TIMESTAMP => typed(text.replacen(' ', "T", 1), xsd::DATE_TIME),
        Type::TIMESTAMPTZ => typed(zone(&text.replacen(' ', "T", 1)), xsd::DATE_TIME),
        Type::BYTEA => typed(
            text.strip_prefix("\\x")
                .unwrap_or(text)
                .to_ascii_uppercase(),
            xsd::HEX_BINARY,
        ),
        _ => typed(text.to_owned(), xsd::STRING),
    }
}

/// The canonical `xsd:decimal` form: no trailing zeros, a digit after the point.
fn decimal(text: &str) -> String {
    match text.split_once('.') {
        None => format!("{text}.0"),
        Some((int, frac)) => {
            let frac = frac.trim_end_matches('0');
            format!("{int}.{}", if frac.is_empty() { "0" } else { frac })
        }
    }
}

/// The canonical `xsd:double` form: `1.5E0`, `1.0E3`, `INF`, `NaN`.
fn double(text: &str) -> String {
    match text {
        "Infinity" => return "INF".into(),
        "-Infinity" => return "-INF".into(),
        "NaN" => return "NaN".into(),
        _ => {}
    }
    let Ok(v) = text.parse::<f64>() else {
        return text.to_owned();
    };
    let s = format!("{v:E}");
    match s.split_once('E') {
        Some((m, e)) if !m.contains('.') => format!("{m}.0E{e}"),
        _ => s,
    }
}

/// A `+02` zone offset written as `+02:00`.
fn zone(text: &str) -> String {
    let bytes = text.as_bytes();
    let n = bytes.len();
    if n >= 3
        && matches!(bytes[n - 3], b'+' | b'-')
        && bytes[n - 2..].iter().all(u8::is_ascii_digit)
    {
        format!("{text}:00")
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_result_cache_keeps_to_its_size() {
        let ttl = std::time::Duration::from_secs(60);
        let key = |i: usize| ("u".to_owned(), format!("q{i}"), 10);
        let mut cache = ResultCache::default();
        // Too large to keep.
        cache.put(key(0), Arc::default(), ResultCache::BYTES / 4 + 1, ttl);
        assert!(cache.get(&key(0), ttl).is_none());
        // Six results of a fifth of the budget: the oldest goes.
        for i in 1..=6 {
            cache.put(key(i), Arc::default(), ResultCache::BYTES / 5, ttl);
        }
        assert!(cache.get(&key(1), ttl).is_none());
        assert!((2..=6).all(|i| cache.get(&key(i), ttl).is_some()));
        assert!(cache.bytes <= ResultCache::BYTES);
        // Past the TTL, nothing.
        assert!(cache.get(&key(6), std::time::Duration::ZERO).is_none());
        assert_eq!(cache.bytes, 4 * (ResultCache::BYTES / 5));
        // Many small cells count for what they hold, not their few bytes of text: 100,000
        // integers of 6 digits are about 0.7 MB as text, several times that as literals.
        let int = |i: usize| {
            Some(Literal::new_typed_literal(
                (100_000 + i).to_string(),
                oxigraph::model::vocab::xsd::INTEGER,
            ))
        };
        let rows = Rows {
            columns: vec!["n".into()],
            rows: (0..100_000).map(|i| vec![int(i)]).collect(),
            ..Rows::default()
        };
        let text = 100_000 * 7;
        assert!(footprint(&rows) > 10 * text, "{}", footprint(&rows));
    }

    #[test]
    fn copy_text_is_read_back() {
        assert_eq!(literal(r"it's \x"), r"E'it''s \\x'");
        // Escaped tab, backslash and new line; NULL; bytea's escaped backslash; octal, hex.
        assert_eq!(
            copy_fields(b"a\\tb\\\\c\\n\t\\N\t\\\\x0102\t\\101\\x42", 4).unwrap(),
            [
                Some("a\tb\\c\n".to_owned()),
                None,
                Some(r"\x0102".to_owned()),
                Some("AB".to_owned())
            ]
        );
        assert_eq!(copy_fields(b"", 1).unwrap(), [Some(String::new())]);
        assert!(copy_fields(b"a\tb", 1).is_err());
        assert!(copy_fields(b"", 0).unwrap().is_empty());
    }

    #[test]
    fn values_are_written_in_canonical_form() {
        assert_eq!(decimal("1.50"), "1.5");
        assert_eq!(decimal("2.000"), "2.0");
        assert_eq!(decimal("-3"), "-3.0");
        for special in ["NaN", "Infinity", "-Infinity"] {
            assert_eq!(
                natural(special, &Type::NUMERIC).datatype.as_ref(),
                xsd::STRING
            );
        }
        assert_eq!(double("1.5"), "1.5E0");
        assert_eq!(double("1000"), "1.0E3");
        assert_eq!(double("-Infinity"), "-INF");
        for (t, ty) in [
            ("infinity", Type::DATE),
            ("-infinity", Type::TIMESTAMPTZ),
            ("0044-03-15 BC", Type::DATE),
            ("0044-03-15 12:00:00+00 BC", Type::TIMESTAMPTZ),
        ] {
            assert_eq!(natural(t, &ty).datatype.as_ref(), xsd::STRING);
        }
        assert_eq!(zone("2011-08-23T22:17:00+02"), "2011-08-23T22:17:00+02:00");
        assert_eq!(
            zone("2011-08-23T22:17:00+05:30"),
            "2011-08-23T22:17:00+05:30"
        );
        let c = natural("\\x48656c6c6f", &Type::BYTEA);
        assert_eq!(
            (c.lexical.as_str(), c.datatype.as_ref()),
            ("48656C6C6F", xsd::HEX_BINARY)
        );
    }

    #[test]
    fn one_of_lists() {
        let int = |v: &str| Literal::new_typed_literal(v, xsd::INTEGER);
        let mut params = Vec::new();
        assert_eq!(
            one_of_condition(
                "c",
                &Type::INT4,
                &[
                    int("1"),
                    int("99999999999"),
                    Literal::new_simple_literal("2"),
                    int("+3")
                ],
                &mut params
            )
            .as_deref(),
            Some("c IN (1, +3)")
        );
        assert_eq!(
            one_of_condition("c", &Type::INT2, &[int("40000")], &mut params).as_deref(),
            Some("false")
        );
        let double = Literal::new_typed_literal("1.075E1", xsd::DOUBLE);
        assert_eq!(
            one_of_condition("c", &Type::FLOAT8, &[double], &mut params).as_deref(),
            Some("c IN (1.075E1::float8)")
        );
        let texts = [
            Literal::new_simple_literal("a"),
            Literal::new_simple_literal("b"),
        ];
        let mut params = Vec::new();
        let w = one_of_condition("c", &Type::TEXT, &texts, &mut params).unwrap();
        assert_eq!(placeholders(&w, |i| format!("${}", i + 1)), "c IN ($1, $2)");
        assert_eq!(
            placeholders(&w, |i| literal(&params[i])),
            "c IN (E'a', E'b')"
        );
        // A string for a numeric column may be NaN: nothing is passed, and no parameter.
        let mut params = Vec::new();
        let nan = Literal::new_simple_literal("NaN");
        assert_eq!(
            one_of_condition("c", &Type::NUMERIC, &[nan], &mut params),
            None
        );
        assert!(params.is_empty());
    }

    #[test]
    fn wkt_boxes() {
        assert_eq!(wkt_envelope("POINT(1 2)"), Some([1.0, 2.0, 1.0, 2.0]));
        assert_eq!(
            wkt_envelope("POLYGON Z((0 0 9, 4 0 9, 4 3 9, 0 0 9))"),
            Some([0.0, 0.0, 4.0, 3.0])
        );
        assert_eq!(
            wkt_envelope("GEOMETRYCOLLECTION(POINT M(5 -1 7), LINESTRING(1 1, 2 8))"),
            Some([1.0, -1.0, 5.0, 8.0])
        );
        for bad in [
            "POINT EMPTY",
            "POINT(1)",
            "POINT(1 2 3)",
            "POINT(a b)",
            "POINT(1e999 1)",
            "",
        ] {
            assert_eq!(wkt_envelope(bad), None, "{bad}");
        }
        let sql = box_condition(
            "g",
            "public",
            None,
            "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(60 10)",
        )
        .unwrap();
        assert!(
            sql.contains("IN (0, 4326)") && sql.contains("ST_MakeEnvelope(10, 60, 10, 60"),
            "{sql}"
        );
        let sql = box_condition(
            "g",
            "public",
            None,
            "<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(1 2)",
        )
        .unwrap();
        assert!(sql.contains("IN (25833)"), "{sql}");
        assert_eq!(
            box_condition("g", "public", None, "<http://example.org/crs> POINT(1 2)"),
            None
        );
        // A declared SRID: a plain `&&`, or nothing for another CRS.
        assert_eq!(
            box_condition("g", "public", Some(4326), "POINT(1 2)").as_deref(),
            Some(r#"g OPERATOR("public".&&) "public".ST_MakeEnvelope(1, 2, 1, 2, 4326)"#)
        );
        // geometry(Point, 4326) and geometry(Geometry, 25833)
        assert_eq!(typmod_srid(1107460), 4326);
        assert_eq!(typmod_srid(25833 << 8), 25833);
        assert_eq!(
            box_condition("g", "public", Some(25833), "POINT(1 2)"),
            None
        );
    }

    #[test]
    fn numbers_for_sql() {
        for good in ["1", "-1.5", "+.5", "1.", "1E3", "2.5e-7"] {
            assert!(is_sql_number(good), "{good}");
        }
        for bad in ["", ".", "1e", "NaN", "INF", "1;", "1 OR 1=1", "0x10", "1e+"] {
            assert!(!is_sql_number(bad), "{bad}");
        }
        for plain in ["1", "-0.0", "0e5", "1.5E300", "2.5e-300"] {
            assert!(is_plain_double(plain), "{plain}");
        }
        for out in ["1e400", "-1E400", "1e-400", "0.1e-400"] {
            assert!(!is_plain_double(out), "{out}");
        }
        let condition = |op, value| Condition {
            key: String::new(),
            op,
            value,
        };
        let double = |v: &str| Literal::new_typed_literal(v, xsd::DOUBLE);
        let mut params = Vec::new();
        assert_eq!(
            sql_condition(
                "c",
                &Type::FLOAT8,
                None,
                &condition(Op::Lt, double("1e400")),
                &mut params
            ),
            None
        );
        assert_eq!(
            sql_condition(
                "c",
                &Type::FLOAT8,
                None,
                &condition(Op::Lt, double("1e30")),
                &mut params
            )
            .as_deref(),
            Some("c < 1e30")
        );
        let nul = Literal::new_simple_literal("a\0b");
        assert_eq!(
            sql_condition("c", &Type::TEXT, None, &condition(Op::Eq, nul), &mut params),
            None
        );
        assert!(params.is_empty());
    }

    #[test]
    fn jdbc_names_become_connections() {
        let d = Database {
            dsn: "jdbc:postgresql://db.example:5433/rail".into(),
            driver: Some("org.postgresql.Driver".into()),
            username: Some("ada".into()),
            password: Some("secret".into()),
        };
        let (c, mode, _) = config_of(&d).unwrap();
        assert_eq!(mode, tls::Mode::Prefer);
        assert_eq!(c.get_dbname(), Some("rail"));
        assert_eq!(c.get_ports(), &[5433]);
        assert_eq!(c.get_user(), Some("ada"));
        assert_eq!(location(&c), "postgresql://db.example/rail");
        let mysql = Database {
            dsn: "jdbc:mysql://localhost/rail".into(),
            driver: None,
            username: None,
            password: None,
        };
        assert!(matches!(config_of(&mysql), Err(RmlError::Unsupported(_))));
    }
}
