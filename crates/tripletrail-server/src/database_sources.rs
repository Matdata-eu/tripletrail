//! Facade-X database sources on a server (#216): `SERVICE <x-sparql-anything:table=…,
//! database=<name>>` (or `sql=`) reads PostgreSQL live, on connections the administrator
//! names. What a query may read is what the connection's role may read: queries run
//! read-only, one statement, within a timeout (which TripleTrail enforces, whatever the SQL
//! sets) and caps on rows and bytes, and the session is reset after each. Off unless
//! [`crate::Options::database_sources`] names some. Files and URLs are never read: a
//! server's Facade-X reads databases (and inline `content=`) only.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tripletrail::fx::{DatabaseQuery, Databases, FxError, Rows, Select};

/// The database connections a server's queries may read through Facade-X.
#[derive(Clone, Debug)]
pub struct DatabaseSources {
    /// Connections by the name queries give them (`database=<name>`).
    pub connections: HashMap<String, DatabaseSource>,
    /// Longest one database query may run.
    pub timeout: Duration,
    /// Most rows one database query may give (`database.max-rows` may ask for fewer).
    pub max_rows: usize,
    /// Most bytes one database query may give, in PostgreSQL's text form.
    pub max_bytes: usize,
    /// Most PostgreSQL sessions open at once per connection; more calls wait for one.
    pub sessions: usize,
    /// How long a result is reused for the same connection and statement (zero: never).
    pub cache_ttl: Duration,
    /// Named virtual graphs: `GRAPH <iri>` in a query reads the Facade-X SERVICE with this
    /// IRI (`x-sparql-anything:table=…,database=…`).
    pub graphs: Vec<(
        tripletrail::oxigraph::model::NamedNode,
        tripletrail::oxigraph::model::NamedNode,
    )>,
}

impl Default for DatabaseSources {
    fn default() -> Self {
        Self {
            connections: HashMap::new(),
            timeout: Duration::from_secs(30),
            max_rows: 100_000,
            max_bytes: 100_000_000,
            sessions: 4,
            cache_ttl: Duration::ZERO,
            graphs: Vec::new(),
        }
    }
}

/// One connection.
#[derive(Clone, Default)]
pub struct DatabaseSource {
    /// A PostgreSQL connection string (`postgresql://user:password@host/db`). Give it a
    /// role that may read only what queries may read: the database checks that, not TripleTrail.
    pub url: String,
}

impl std::fmt::Debug for DatabaseSource {
    /// Without the connection string, which may hold a password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseSource").finish_non_exhaustive()
    }
}

impl DatabaseSources {
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// The Facade-X configuration of a server with these sources: no files, no URLs.
    pub(crate) fn fx_config(&self) -> tripletrail::fx::FxConfig {
        let named = self
            .connections
            .iter()
            .map(|(name, c)| (name.clone(), c.url.clone()))
            .collect();
        let loader = tripletrail_rml::DatabaseLoader::new((), None)
            .with_named(named)
            .with_statement_timeout(self.timeout)
            .with_max_bytes(self.max_bytes)
            .with_sessions(self.sessions)
            .with_cache(self.cache_ttl);
        tripletrail::fx::FxConfig {
            fixed_limits: true,
            base: std::env::temp_dir(),
            // No directory: every file is outside.
            allowed: Some(Vec::new()),
            remote: Default::default(),
            databases: Some(Arc::new(Shared {
                sources: self.clone(),
                loader,
            })),
        }
    }
}

/// The administrator's connections, read within [`DatabaseSources`]'s limits.
#[derive(Debug)]
struct Shared {
    sources: DatabaseSources,
    loader: tripletrail_rml::DatabaseLoader<()>,
}

impl Databases for Shared {
    fn rows(
        &self,
        name: Option<&str>,
        query: &DatabaseQuery,
        select: &Select<'_>,
        max_rows: usize,
    ) -> Result<Rows, FxError> {
        let denied = |m: String| Err(FxError::Denied(m));
        let Some(name) = name else {
            return denied("name the server's database connection with database=<name>".into());
        };
        if !self.sources.connections.contains_key(name) {
            return denied(format!(
                "{name} is not one of the server's database connections"
            ));
        }
        self.loader.rows(
            Some(name),
            query,
            select,
            max_rows.min(self.sources.max_rows),
        )
    }
}
