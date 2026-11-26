//! Who is calling (#199): API keys, trusted headers from an authenticating proxy, or
//! anonymous access, with the identity model and configuration of
//! [sparql-guard](https://matdata-eu.github.io/sparql-guard/guide/authentication), so the
//! `auth` object of a sparql-guard configuration works here unchanged.
//!
//! The methods are tried in order: an API key if the request carries one, the trusted headers
//! if configured and present, then anonymous if allowed. A key that matches nothing is a 401,
//! never a fall-through to anonymous. The result is a [`Caller`]: an agent IRI (none for
//! anonymous) and its groups.
//!
//! What a caller may do with a dataset (#200) comes from sparql-guard policies
//! ([`crate::policy`]) when the configuration lists any: the file's `policies` (as in
//! sparql-guard) for every dataset, or `datasets.<name>.policies` for one; a dataset without
//! policies is then closed to all but admins. Without policies, every identified caller may
//! read. Either way the group `tripletrail:writer` may write everywhere and `tripletrail:admin` may
//! do everything, and `datasets.<name>.admins` (agents or groups) may administer one dataset.
//! `--update-token` and `--admin-token` become two built-in keys with those groups.

use crate::HttpError;
use crate::policy::{self, Policy};
use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tripletrail::ReadableGraphs;
use tripletrail::oxigraph::model::NamedNode;

/// The group that may write to every dataset.
pub const WRITER: &str = "tripletrail:writer";
/// The group that may administer the server (and write).
pub const ADMIN: &str = "tripletrail:admin";
/// The group that may read the server-wide pages under `/$/` (metrics, status, server
/// information, tasks) when policies are configured.
pub const MONITOR: &str = "tripletrail:monitor";
/// The agent of the built-in token keys.
pub(crate) const ADMIN_AGENT: &str = "urn:tripletrail:admin";
/// `sg:requestingAgent` for anonymous callers, as in sparql-guard.
pub const ANONYMOUS_AGENT: &str = "urn:sparql-guard:anonymous";
const DEFAULT_AGENT_PREFIX: &str = "urn:sparql-guard:user:";

/// How long a checked password is remembered, so that a client sending it with every request
/// does not pay for the slow hash each time (#825).
const PASSWORD_REMEMBERED: Duration = Duration::from_secs(15 * 60);

/// Failed keys allowed per client address within [`FAILURE_WINDOW`] before it gets 429.
const MAX_FAILURES: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// How often the configuration file is checked for changes.
const RELOAD_CHECK: Duration = Duration::from_secs(1);

/// How a caller was identified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    ApiKey,
    /// A user name and password, sent with HTTP Basic (#825).
    Password,
    TrustedHeaders,
    /// A JWT from a trusted OpenID Connect issuer (#198, TripleTrail Enterprise).
    Oidc,
    /// A browser session opened by signing in with an OpenID Connect issuer (#198): the
    /// session cookie. Changes need a request from the server's own origin.
    Session,
    Anonymous,
}

/// The identity a request runs as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The agent IRI; `None` for anonymous callers.
    pub agent: Option<String>,
    pub groups: Vec<String>,
    pub method: Method,
    /// The datasets this caller may use at all (from the key's `datasets`); `None` is all.
    pub datasets: Option<Vec<String>>,
}

impl Caller {
    pub fn in_group(&self, group: &str) -> bool {
        self.groups.iter().any(|g| g == group)
    }

    pub fn may_write(&self) -> bool {
        self.in_group(WRITER) || self.in_group(ADMIN)
    }

    pub fn is_admin(&self) -> bool {
        self.in_group(ADMIN)
    }

    /// Whether the caller may use the dataset (by its name, or an alias's).
    pub fn may_use(&self, names: &[&str]) -> bool {
        self.datasets
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|a| names.contains(&a.as_str())))
    }
}

#[derive(Clone, Debug)]
struct ApiKey {
    sha256: [u8; 32],
    agent: String,
    groups: Vec<String>,
    datasets: Option<Vec<String>>,
    expires: Option<SystemTime>,
    /// The key's `label` and `expires` as written, for the access pages (#205).
    label: Option<String>,
    expires_text: Option<String>,
}

/// A user who signs in with HTTP Basic, as Fuseki's Shiro users do (#825).
#[derive(Clone, Debug)]
struct User {
    name: String,
    password: crate::password::PasswordHash,
    agent: String,
    groups: Vec<String>,
    datasets: Option<Vec<String>>,
    expires: Option<SystemTime>,
    expires_text: Option<String>,
}

#[derive(Clone, Debug)]
struct TrustedHeaders {
    agent: String,
    groups: Option<String>,
    agent_prefix: String,
    /// Addresses the headers are believed from. Without `trustedProxies` only loopback, and
    /// then the headers never grant the `tripletrail:` groups.
    trusted_proxies: Vec<Cidr>,
    /// Whether `trustedProxies` was given.
    proxies_listed: bool,
}

#[derive(Clone, Debug, Default)]
struct Config {
    keys: Vec<ApiKey>,
    users: Vec<User>,
    headers: Option<TrustedHeaders>,
    /// Single sign-on (#198): the trusted token issuers, with their keys.
    #[cfg(feature = "enterprise")]
    oidc: Option<Arc<tripletrail_enterprise::oidc::Verifier>>,
    /// Browser sign-in with those issuers, and its sessions.
    #[cfg(feature = "enterprise")]
    sign_in: Option<Arc<tripletrail_enterprise::sso::SignIn>>,
    anonymous: bool,
    /// Changes made through `/$/access` wait for a second admin (#207).
    review: bool,
    /// Whether any policies are configured: then a dataset without any is closed.
    policies_on: bool,
    /// The file's `policies`, for datasets without their own.
    default_policies: Vec<Policy>,
    /// The files they come from.
    default_files: Vec<PathBuf>,
    datasets: HashMap<String, DatasetAccess>,
    /// The policy files, watched for changes with the configuration itself.
    files: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default)]
struct DatasetAccess {
    /// `None`: the file's `policies` apply.
    policies: Option<Vec<Policy>>,
    /// The files of `policies`.
    files: Vec<PathBuf>,
    /// Agents (IRIs) or groups that may administer the dataset.
    admins: Vec<String>,
    /// Agents (IRIs) or groups that may propose changes to the dataset's shapes (#145).
    shape_editors: Vec<String>,
}

/// What a caller may do with one dataset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Access {
    pub read: bool,
    pub write: bool,
    /// Detach, swap, snapshot and back up the dataset.
    pub admin: bool,
    /// Without `read`: some graphs may be read (`sg:readGraph`, #201); which ones,
    /// [`Auth::readable_graphs`] tells.
    pub partial: bool,
    /// Without `write`: some graphs may be written (`sg:writeGraph`, #201); which ones,
    /// [`Auth::writable_graphs`] tells.
    pub partial_write: bool,
}

struct Loaded {
    config: Config,
    /// Modification times of the configuration and its policy files.
    modified: Vec<Option<SystemTime>>,
    checked: Instant,
}

/// The server's authentication: a configuration file (reloaded when it changes) plus the
/// built-in token keys.
pub(crate) struct Auth {
    path: PathBuf,
    builtin: Vec<ApiKey>,
    loaded: RwLock<Loaded>,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    /// When each key was last used, since the server started (for the access pages).
    last_used: Mutex<HashMap<[u8; 32], SystemTime>>,
    /// Passwords checked lately, by a keyed hash of the user, the password and its stored
    /// hash (so a changed password or configuration is checked again), and when.
    passwords: Mutex<HashMap<[u8; 32], Instant>>,
    /// A random key for those hashes, new with every start.
    password_key: [u8; 32],
}

impl Auth {
    /// Reads the configuration at `path`. `update_token` and `admin_token` become keys of the
    /// groups `tripletrail:writer` and `tripletrail:admin`.
    pub(crate) fn load(
        path: &Path,
        update_token: Option<&str>,
        admin_token: Option<&str>,
    ) -> Result<Self, String> {
        let config = read_config(path)?;
        let modified = modified_all(path, &config.files);
        let builtin = [(update_token, WRITER), (admin_token, ADMIN)]
            .into_iter()
            .filter_map(|(token, group)| {
                Some(ApiKey {
                    sha256: sha256(token?.as_bytes()),
                    agent: ADMIN_AGENT.to_owned(),
                    groups: vec![group.to_owned()],
                    datasets: None,
                    expires: None,
                    label: Some("built-in token".to_owned()),
                    expires_text: None,
                })
            })
            .collect();
        Ok(Self {
            path: path.to_owned(),
            builtin,
            loaded: RwLock::new(Loaded {
                config,
                modified,
                checked: Instant::now(),
            }),
            failures: Mutex::default(),
            last_used: Mutex::default(),
            passwords: Mutex::default(),
            password_key: {
                let mut key = [0u8; 32];
                getrandom::fill(&mut key).map_err(|e| e.to_string())?;
                key
            },
        })
    }

    /// Re-reads the file when it changed; a broken file keeps the previous configuration.
    fn reload_if_changed(&self) {
        {
            let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
            if loaded.checked.elapsed() < RELOAD_CHECK {
                return;
            }
        }
        let mut loaded = self.loaded.write().unwrap_or_else(|e| e.into_inner());
        if loaded.checked.elapsed() < RELOAD_CHECK {
            return;
        }
        loaded.checked = Instant::now();
        let now = modified_all(&self.path, &loaded.config.files);
        if now == loaded.modified {
            return;
        }
        match read_config(&self.path) {
            Ok(config) => {
                // The new file may list other policy files.
                loaded.modified = modified_all(&self.path, &config.files);
                #[cfg(feature = "enterprise")]
                let config = {
                    let mut config = config;
                    keep_sign_in(&loaded.config, &mut config);
                    config
                };
                loaded.config = config;
                eprintln!("tripletrail: reloaded {}", self.path.display());
            }
            Err(e) => {
                // Keep the old configuration, and retry once the file changes again.
                loaded.modified = now;
                eprintln!(
                    "tripletrail: {} is invalid, keeping the previous one: {e}",
                    self.path.display()
                );
            }
        }
    }

    /// Re-reads the configuration now, whatever the modification times say.
    fn reload_now(&self) -> Result<(), String> {
        #[allow(unused_mut)]
        let mut config = read_config(&self.path)?;
        let mut loaded = self.loaded.write().unwrap_or_else(|e| e.into_inner());
        #[cfg(feature = "enterprise")]
        keep_sign_in(&loaded.config, &mut config);
        loaded.modified = modified_all(&self.path, &config.files);
        loaded.config = config;
        loaded.checked = Instant::now();
        Ok(())
    }

    /// What the access pages show (#205): who may read and write each of `datasets` (and of
    /// those the configuration names), from which policy files, the API keys (without their
    /// hashes) and the groups in use.
    pub(crate) fn overview(&self, datasets: &[String]) -> serde_json::Value {
        use serde_json::json;
        self.reload_if_changed();
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let config = &loaded.config;
        let shown = |files: &[PathBuf]| -> Vec<String> {
            files.iter().map(|f| f.display().to_string()).collect()
        };
        let specs = |policies: &[Policy]| -> Vec<serde_json::Value> {
            policies
                .iter()
                .map(|p| match p.spec() {
                    Some(s) => s.to_json(),
                    None => json!({ "id": null, "blankNode": p.id }),
                })
                .collect()
        };
        let editable = |files: &[PathBuf]| {
            files
                .iter()
                .any(|f| f.extension().and_then(|e| e.to_str()) == Some("ttl"))
        };
        let mut names: Vec<String> = datasets.to_vec();
        names.extend(config.datasets.keys().cloned());
        names.sort();
        names.dedup();
        let datasets: Vec<serde_json::Value> = names
            .iter()
            .map(|name| {
                let own = config.datasets.get(name);
                let policies = own.and_then(|d| d.policies.as_ref());
                json!({
                    "name": name,
                    "attached": datasets.contains(name),
                    "ownPolicies": policies.is_some(),
                    "files": own.map(|d| shown(&d.files)).unwrap_or_default(),
                    "editable": match policies {
                        Some(_) => own.is_some_and(|d| editable(&d.files)),
                        None => config.default_files.iter().all(|f| editable(std::slice::from_ref(f))),
                    },
                    "admins": own.map(|d| d.admins.clone()).unwrap_or_default(),
                    "shapeEditors": own.map(|d| d.shape_editors.clone()).unwrap_or_default(),
                    "policies": specs(policies.unwrap_or(&config.default_policies)),
                    "closed": config.policies_on
                        && policies.is_none()
                        && config.default_policies.is_empty(),
                })
            })
            .collect();
        let keys: Vec<&ApiKey> = self.builtin.iter().chain(&config.keys).collect();
        let last_used = self
            .last_used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut groups: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
        for g in [WRITER, ADMIN, MONITOR] {
            groups.entry(g.to_owned()).or_default();
        }
        for k in &keys {
            for g in &k.groups {
                groups.entry(g.clone()).or_default().0 += 1;
            }
        }
        let all_policies = config.default_policies.iter().chain(
            config
                .datasets
                .values()
                .filter_map(|d| d.policies.as_ref())
                .flatten(),
        );
        for p in all_policies {
            for g in &p.spec().map(|s| s.groups).unwrap_or_default() {
                let entry = groups.entry(g.clone()).or_default();
                if !entry.1.contains(&p.id) {
                    entry.1.push(p.id.clone());
                }
            }
        }
        json!({
            "configFile": self.path.display().to_string(),
            "anonymous": config.anonymous,
            "review": config.review,
            "trustedHeaders": config.headers.as_ref().map(|h| json!({
                "agent": h.agent,
                "groups": h.groups,
                "agentPrefix": h.agent_prefix,
                "proxiesListed": h.proxies_listed,
            })),
            "policiesOn": config.policies_on,
            "defaultPolicies": {
                "files": shown(&config.default_files),
                "policies": specs(&config.default_policies),
            },
            "datasets": datasets,
            "users": config.users.iter().map(|u| json!({
                "name": u.name,
                "agent": u.agent,
                "groups": u.groups,
                "datasets": u.datasets,
                "expires": u.expires_text,
                "expired": u.expires.is_some_and(|e| e < SystemTime::now()),
            })).collect::<Vec<_>>(),
            "keys": keys.iter().map(|k| json!({
                "id": to_hex(&k.sha256[..4]),
                "label": k.label,
                "agent": k.agent,
                "groups": k.groups,
                "datasets": k.datasets,
                "expires": k.expires_text,
                "expired": k.expires.is_some_and(|e| e < SystemTime::now()),
                "builtIn": k.label.as_deref() == Some("built-in token"),
                "lastUsed": last_used.get(&k.sha256).map(|t| crate::admin::rfc3339(*t)),
            })).collect::<Vec<_>>(),
            "groups": groups.into_iter().map(|(name, (keys, policies))| json!({
                "name": name,
                "server": ([WRITER, ADMIN, MONITOR].contains(&name.as_str())),
                "keys": keys,
                "policies": (policies.iter().map(|p| p.trim_start_matches('<').trim_end_matches('>')).collect::<Vec<_>>()),
            })).collect::<Vec<_>>(),
        })
    }

    /// Whether the configuration names the dataset `name`.
    pub(crate) fn configures(&self, name: &str) -> bool {
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .datasets
            .contains_key(name)
    }

    /// Changes the policy `id` of dataset `name` with `edit` (which gets and returns the text
    /// of a Turtle policy file), in the file that holds it or the dataset's first Turtle file.
    /// A dataset without policies of its own first gets a file `policies/<name>.ttl` next to
    /// the configuration, holding a copy of the file's default policies (so nothing changes
    /// for it until the edit), and the configuration names it. When the result does not load,
    /// every file is put back as it was.
    /// The policy files of a dataset as [`Self::edit_policy`] would edit them for the policy
    /// or shape `id`: the texts of its files (a dataset without its own gets one, the default
    /// files together, as its first edit does), and which of them an edit changes. Nothing is
    /// written (enterprise#8: the shapes and preview of the access pages).
    pub(crate) fn policy_texts(
        &self,
        name: &str,
        id: &str,
    ) -> Result<(Vec<(tripletrail::oxigraph::io::RdfFormat, String)>, usize), String> {
        use tripletrail::oxigraph::io::RdfFormat;
        let config = self
            .loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .clone();
        let is_turtle = |f: &PathBuf| f.extension().and_then(|e| e.to_str()) == Some("ttl");
        let read = |f: &PathBuf| std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()));
        match config.datasets.get(name).filter(|d| d.policies.is_some()) {
            Some(d) => {
                let holds = |f: &PathBuf| {
                    policy::read(std::slice::from_ref(f))
                        .is_ok_and(|ps| ps.iter().any(|p| p.id == format!("<{id}>")))
                };
                let chosen = d
                    .files
                    .iter()
                    .position(|f| is_turtle(f) && holds(f))
                    .or_else(|| d.files.iter().position(is_turtle))
                    .ok_or_else(|| format!("The policies of {name} are not in a Turtle (.ttl) file"))?;
                let texts = d
                    .files
                    .iter()
                    .map(|f| Ok((policy::format_of(f)?, read(f)?)))
                    .collect::<Result<Vec<_>, String>>()?;
                Ok((texts, chosen))
            }
            None => {
                if !config.default_files.iter().all(is_turtle) {
                    return Err("The default policies are not in Turtle (.ttl) files".to_owned());
                }
                let mut seed = String::new();
                for f in &config.default_files {
                    seed.push_str(&read(f)?);
                    seed.push('\n');
                }
                Ok((vec![(RdfFormat::Turtle, seed)], 0))
            }
        }
    }

    pub(crate) fn edit_policy(
        &self,
        name: &str,
        id: &str,
        edit: impl FnOnce(&str) -> Result<String, String>,
    ) -> Result<(), String> {
        let _one_at_a_time = EDITS.lock().unwrap_or_else(|e| e.into_inner());
        self.reload_now()?;
        let config = self
            .loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .clone();
        let is_turtle = |f: &PathBuf| f.extension().and_then(|e| e.to_str()) == Some("ttl");
        // The policies/ folder, when this edit made it.
        let mut new_dir: Option<PathBuf> = None;
        let own = config.datasets.get(name).filter(|d| d.policies.is_some());
        // (file, its text before, whether it is new); the configuration's text when changed.
        let (file, before, created, config_before) = match own {
            Some(d) => {
                let holds = |f: &PathBuf| {
                    policy::read(std::slice::from_ref(f))
                        .is_ok_and(|ps| ps.iter().any(|p| p.id == format!("<{id}>")))
                };
                let file = d
                    .files
                    .iter()
                    .filter(|f| is_turtle(f))
                    .find(|f| holds(f))
                    .or_else(|| d.files.iter().find(|f| is_turtle(f)))
                    .ok_or_else(|| {
                        format!("The policies of {name} are not in a Turtle (.ttl) file")
                    })?
                    .clone();
                let text = std::fs::read_to_string(&file)
                    .map_err(|e| format!("{}: {e}", file.display()))?;
                (file, text, false, None)
            }
            None => {
                if !config.default_files.iter().all(is_turtle) {
                    return Err("The default policies are not in Turtle (.ttl) files".to_owned());
                }
                let base = self.path.parent().unwrap_or(Path::new("."));
                let relative = format!("policies/{name}.ttl");
                let file = base.join(&relative);
                if file.exists() {
                    return Err(format!(
                        "{} exists but the configuration does not name it",
                        file.display()
                    ));
                }
                let mut seed = format!("# Access to the dataset {name} (#205).\n");
                for f in &config.default_files {
                    seed.push_str(&format!("\n# From {}:\n", f.display()));
                    seed.push_str(
                        &std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?,
                    );
                }
                let text = std::fs::read_to_string(&self.path)
                    .map_err(|e| format!("{}: {e}", self.path.display()))?;
                let mut root: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| e.to_string())?;
                let datasets = root
                    .as_object_mut()
                    .ok_or("The configuration is not a JSON object")?
                    .entry("datasets")
                    .or_insert_with(|| serde_json::json!({}));
                let entry = datasets
                    .as_object_mut()
                    .ok_or("datasets must be an object")?
                    .entry(name)
                    .or_insert_with(|| serde_json::json!({}));
                entry
                    .as_object_mut()
                    .ok_or_else(|| format!("datasets.{name} must be an object"))?
                    .insert("policies".to_owned(), serde_json::json!([relative]));
                if let Some(dir) = file.parent()
                    && !dir.exists()
                {
                    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                    new_dir = Some(dir.to_owned());
                }
                write_atomic(&file, &seed, &self.path)?;
                let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())? + "\n";
                if let Err(e) = write_atomic(&self.path, &pretty, &self.path) {
                    let _ = std::fs::remove_file(&file);
                    if let Some(dir) = &new_dir {
                        let _ = std::fs::remove_dir(dir);
                    }
                    return Err(e);
                }
                (file, seed, true, Some(text))
            }
        };
        let undo = || {
            if created {
                let _ = std::fs::remove_file(&file);
                if let Some(dir) = &new_dir {
                    let _ = std::fs::remove_dir(dir);
                }
            } else {
                let _ = write_atomic(&file, &before, &self.path);
            }
            if let Some(text) = &config_before {
                let _ = write_atomic(&self.path, text, &self.path);
            }
        };
        let after = match edit(&before) {
            Ok(a) => a,
            Err(e) => {
                undo();
                return Err(e);
            }
        };
        if let Err(e) = write_atomic(&file, &after, &self.path).and_then(|()| self.reload_now()) {
            undo();
            let _ = self.reload_now();
            return Err(e);
        }
        Ok(())
    }

    /// Creates an API key (#205) from `spec` (`agent`, and optionally `label`, `groups`,
    /// `datasets` and `expires`, as in the configuration) and adds its hash to the
    /// configuration's `apiKeys`. Returns the key, which is not stored anywhere, and its id.
    pub(crate) fn create_key(
        &self,
        spec: &serde_json::Value,
    ) -> Result<(String, String), HttpError> {
        use serde_json::{Value, json};
        let bad = |m: String| HttpError::bad_request(m);
        let agent =
            string(spec, "agent").ok_or_else(|| bad("agent (an IRI) is missing".to_owned()))?;
        if !is_iri(agent) {
            return Err(bad(format!("agent must be an IRI, not {agent:?}")));
        }
        let groups = strings(spec, "groups").map_err(|e| bad(format!("groups {e}")))?;
        let mut entry = json!({ "agent": agent });
        if let Some(label) = spec.get("label").filter(|l| !l.is_null()) {
            let label = label
                .as_str()
                .ok_or_else(|| bad("label must be a string".to_owned()))?;
            entry["label"] = json!(label);
        }
        if !groups.is_empty() {
            entry["groups"] = json!(groups);
        }
        if let Some(datasets) = spec.get("datasets").filter(|d| !d.is_null()) {
            let datasets = strings(&json!({ "d": datasets }), "d")
                .map_err(|e| bad(format!("datasets {e}")))?;
            entry["datasets"] = json!(datasets);
        }
        if let Some(expires) = spec.get("expires").filter(|e| !e.is_null()) {
            let text = expires
                .as_str()
                .ok_or_else(|| bad("expires must be a string".to_owned()))?;
            if parse_time(text).is_none() {
                return Err(bad(format!("expires is not a date or date-time: {text}")));
            }
            entry["expires"] = json!(text);
        }
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret)
            .map_err(|e| HttpError(500, format!("No random numbers for a key: {e}")))?;
        let key = format!("mg_{}", to_hex(&secret));
        let hash = sha256(key.as_bytes());
        entry["sha256"] = json!(to_hex(&hash));
        self.edit_config(|root| {
            let keys = auth_object(root)?
                .entry("apiKeys")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or("apiKeys must be an array")?;
            keys.push(entry);
            Ok(())
        })
        .map_err(|e| HttpError(422, e))?;
        Ok((key, to_hex(&hash[..4])))
    }

    /// Removes the API key whose id (the first 8 hex digits of its hash, as the access pages
    /// show it) is `id` from the configuration's `apiKeys`.
    pub(crate) fn revoke_key(&self, id: &str) -> Result<(), HttpError> {
        let id = id.to_ascii_lowercase();
        if self.builtin.iter().any(|k| to_hex(&k.sha256[..4]) == id) {
            return Err(HttpError(
                409,
                "A built-in token is set on the command line; restart without it to revoke it"
                    .to_owned(),
            ));
        }
        let mut found = 0;
        self.edit_config(|root| {
            let Some(keys) = auth_object(root)?
                .get_mut("apiKeys")
                .and_then(|k| k.as_array_mut())
            else {
                return Ok(());
            };
            // The hashes as written: `${VAR}` placeholders stay unexpanded, and never match.
            let matches = |k: &serde_json::Value| {
                string(k, "sha256").is_some_and(|h| h.to_ascii_lowercase().starts_with(&id))
            };
            found = keys.iter().filter(|k| matches(k)).count();
            if found == 1 {
                keys.retain(|k| !matches(k));
            }
            Ok(())
        })
        .map_err(|e| HttpError(422, e))?;
        match found {
            1 => Ok(()),
            0 => Err(HttpError(
                404,
                format!("No API key {id} in the configuration"),
            )),
            _ => Err(HttpError(
                409,
                format!("{found} keys start with {id}; edit the configuration by hand"),
            )),
        }
    }

    /// Turns anonymous access on or off (the configuration's `anonymous`).
    pub(crate) fn set_anonymous(&self, on: bool) -> Result<(), HttpError> {
        self.edit_config(|root| {
            auth_object(root)?.insert("anonymous".to_owned(), serde_json::Value::Bool(on));
            Ok(())
        })
        .map_err(|e| HttpError(422, e))
    }

    /// The configuration file.
    /// Whether access changes wait for a second admin (`auth.review`, #207).
    pub(crate) fn review(&self) -> bool {
        self.reload_if_changed();
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .review
    }

    pub(crate) fn config_path(&self) -> &Path {
        &self.path
    }

    /// The configuration file and the policy files it names, for the change history (#205).
    pub(crate) fn access_files(&self) -> Vec<PathBuf> {
        let _ = self.reload_now();
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let mut files = vec![self.path.clone()];
        for f in &loaded.config.files {
            if !files.contains(f) {
                files.push(f.clone());
            }
        }
        files
    }

    /// Writes `files` (an earlier version of [`Self::access_files`]) back and loads them. The
    /// configuration keeps its current `apiKeys`, so a restore never brings back a revoked
    /// key. When the result does not load, every file is put back.
    pub(crate) fn restore(&self, files: &[(PathBuf, String)]) -> Result<(), String> {
        let _one_at_a_time = EDITS.lock().unwrap_or_else(|e| e.into_inner());
        let mut undo: Vec<(PathBuf, Option<String>)> = Vec::new();
        let put_back = |undo: &[(PathBuf, Option<String>)]| {
            for (path, text) in undo.iter().rev() {
                match text {
                    Some(t) => {
                        let _ = write_atomic(path, t, &self.path);
                    }
                    None => {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        };
        for (path, text) in files {
            let now = std::fs::read_to_string(path).ok();
            let text = if *path == self.path {
                let mut old: serde_json::Value =
                    serde_json::from_str(text).map_err(|e| e.to_string())?;
                let current: Option<serde_json::Value> =
                    now.as_deref().and_then(|t| serde_json::from_str(t).ok());
                let keys = current.and_then(|mut c| auth_object(&mut c).ok()?.remove("apiKeys"));
                let auth = auth_object(&mut old)?;
                match keys {
                    Some(k) => auth.insert("apiKeys".to_owned(), k),
                    None => auth.remove("apiKeys"),
                };
                serde_json::to_string_pretty(&old).map_err(|e| e.to_string())? + "\n"
            } else {
                text.clone()
            };
            if now.as_deref() == Some(text.as_str()) {
                continue;
            }
            if let Some(dir) = path.parent()
                && !dir.exists()
            {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            if let Err(e) = write_atomic(path, &text, &self.path) {
                put_back(&undo);
                return Err(e);
            }
            undo.push((path.clone(), now));
        }
        // Policy files the access pages made after that version would block making them
        // again ("exists but the configuration does not name it"): they go, and stay in the
        // history.
        let made_here = self
            .path
            .parent()
            .unwrap_or(Path::new("."))
            .join("policies");
        let current = self
            .loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .files
            .clone();
        for path in current {
            if path.starts_with(&made_here) && !files.iter().any(|(p, _)| *p == path) {
                let now = std::fs::read_to_string(&path).ok();
                if std::fs::remove_file(&path).is_ok() {
                    undo.push((path, now));
                }
            }
        }
        if let Err(e) = self.reload_now() {
            put_back(&undo);
            let _ = self.reload_now();
            return Err(e);
        }
        Ok(())
    }

    /// Changes the configuration file's JSON (as written, without expanding `${VAR}`) with
    /// `edit` and loads it; when the result does not load, the file is put back.
    fn edit_config(
        &self,
        edit: impl FnOnce(&mut serde_json::Value) -> Result<(), String>,
    ) -> Result<(), String> {
        let _one_at_a_time = EDITS.lock().unwrap_or_else(|e| e.into_inner());
        let before = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        let mut root: serde_json::Value =
            serde_json::from_str(&before).map_err(|e| e.to_string())?;
        edit(&mut root)?;
        let after = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())? + "\n";
        if after == before {
            return Ok(());
        }
        write_atomic(&self.path, &after, &self.path)?;
        if let Err(e) = self.reload_now() {
            let _ = write_atomic(&self.path, &before, &self.path);
            let _ = self.reload_now();
            return Err(e);
        }
        Ok(())
    }

    /// The client's address: the last `X-Forwarded-For` address when the request comes from a
    /// listed trusted proxy, otherwise the connection's address.
    pub(crate) fn client(
        &self,
        header: &dyn Fn(&str) -> Option<String>,
        remote: Option<IpAddr>,
    ) -> Option<IpAddr> {
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        client_address(&loaded.config, header, remote)
    }

    /// The caller of a request, from its headers and the address it came from.
    pub(crate) fn identify(
        &self,
        header: &dyn Fn(&str) -> Option<String>,
        remote: Option<IpAddr>,
    ) -> Result<Caller, HttpError> {
        self.reload_if_changed();
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let config = &loaded.config;

        // Only with users: otherwise a Basic header (one a proxy forwards, say) is not for
        // this server, and is ignored as before.
        if !config.users.is_empty()
            && let Some(credentials) = header("Authorization").and_then(|h| basic(&h))
        {
            // A Basic header that cannot be read is a 401, never a fall-through to anonymous.
            let (name, password) =
                credentials.ok_or_else(|| unauthorized("The Basic credentials cannot be read"))?;
            return self.sign_in(config, header, remote, &name, &password);
        }
        let key = header("Authorization")
            .and_then(|h| {
                let (scheme, value) = h.trim().split_once(' ')?;
                scheme
                    .eq_ignore_ascii_case("bearer")
                    .then(|| value.trim().to_owned())
            })
            .or_else(|| header("X-API-Key").map(|k| k.trim().to_owned()));
        #[cfg(feature = "enterprise")]
        if let (Some(token), Some(oidc)) = (&key, &config.oidc)
            && tripletrail_enterprise::oidc::looks_like_jwt(token)
        {
            let remote = client_address(config, header, remote);
            if let Some(ip) = remote
                && self.too_many_failures(ip)
            {
                return Err(HttpError(
                    429,
                    "Too many requests with a refused token; try again later".to_owned(),
                ));
            }
            return match oidc.verify(token, SystemTime::now()) {
                Ok(id) => Ok(Caller {
                    agent: Some(id.agent),
                    groups: id.groups,
                    method: Method::Oidc,
                    datasets: None,
                }),
                Err(e) => {
                    if let Some(ip) = remote {
                        self.record_failure(ip);
                    }
                    Err(unauthorized(&e))
                }
            };
        }
        if let Some(key) = key {
            let remote = client_address(config, header, remote);
            if let Some(ip) = remote
                && self.too_many_failures(ip)
            {
                return Err(HttpError(
                    429,
                    "Too many requests with an unknown API key; try again later".to_owned(),
                ));
            }
            let hash = sha256(key.as_bytes());
            // Every key is compared in full, so the time taken does not tell which one is close.
            let mut found = None;
            for k in self.builtin.iter().chain(&config.keys) {
                if constant_time_eq(&k.sha256, &hash) && found.is_none() {
                    found = Some(k);
                }
            }
            return match found {
                Some(k) if k.expires.is_none_or(|e| e > SystemTime::now()) => {
                    self.last_used
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(k.sha256, SystemTime::now());
                    Ok(Caller {
                        agent: Some(k.agent.clone()),
                        groups: k.groups.clone(),
                        method: Method::ApiKey,
                        datasets: k.datasets.clone(),
                    })
                }
                Some(_) => Err(unauthorized("The API key has expired")),
                None => {
                    if let Some(ip) = remote {
                        self.record_failure(ip);
                    }
                    Err(unauthorized("Unknown API key"))
                }
            };
        }

        // A browser signed in with single sign-on (#198). A cookie whose session has ended is
        // a 401, so the web UI signs in again rather than show less as someone anonymous.
        #[cfg(feature = "enterprise")]
        if let Some(sign_in) = &config.sign_in
            && let Some(id) = session_cookie(header)
        {
            return match sign_in.session(&id, SystemTime::now()) {
                Some(s) => Ok(Caller {
                    agent: Some(s.identity.agent),
                    groups: s.identity.groups,
                    method: Method::Session,
                    datasets: None,
                }),
                None => Err(unauthorized(
                    "Your sign-in has ended or is not known here: sign in again",
                )),
            };
        }
        if let Some(h) = &config.headers
            && let Some(user) = header(&h.agent).map(|u| u.trim().to_owned())
            && !user.is_empty()
            && remote.is_some_and(|ip| h.trusted_proxies.iter().any(|c| c.contains(ip)))
        {
            let agent = if is_iri(&user) {
                user
            } else {
                format!("{}{}", h.agent_prefix, percent_encode(&user))
            };
            let groups = h
                .groups
                .as_deref()
                .and_then(header)
                .map(|g| {
                    g.split(',')
                        .map(str::trim)
                        .filter(|g| !g.is_empty())
                        // The server's own groups only from a proxy that was listed by name.
                        .filter(|g| h.proxies_listed || !g.starts_with("tripletrail:"))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            return Ok(Caller {
                agent: Some(agent),
                groups,
                method: Method::TrustedHeaders,
                datasets: None,
            });
        }

        if config.anonymous {
            return Ok(Caller {
                agent: None,
                groups: Vec::new(),
                method: Method::Anonymous,
                datasets: None,
            });
        }
        Err(unauthorized(if config.users.is_empty() {
            "This needs credentials: Authorization: Bearer <key> or X-API-Key"
        } else {
            "This needs credentials: a user name and password, or Authorization: Bearer <key> or X-API-Key"
        }))
    }

    /// Who a single sign-on token says its bearer is, for the access explain page (#198):
    /// the identity, with the claim each group came from, or why the token is refused.
    #[cfg(feature = "enterprise")]
    pub(crate) fn explain_token(
        &self,
        token: &str,
    ) -> Result<tripletrail_enterprise::oidc::Identity, HttpError> {
        self.reload_if_changed();
        let verifier = self
            .loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .oidc
            .clone()
            .ok_or_else(|| HttpError(400, "No auth.oidc issuer is configured".to_owned()))?;
        verifier
            .verify(token.trim(), SystemTime::now())
            .map_err(|e| HttpError(422, e))
    }

    /// Browser sign-in with single sign-on (#198), when an issuer has a `browser` client.
    #[cfg(feature = "enterprise")]
    pub(crate) fn browser_sign_in(&self) -> Option<Arc<tripletrail_enterprise::sso::SignIn>> {
        self.reload_if_changed();
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .sign_in
            .clone()
    }

    /// Whether users sign in with passwords: then a 401 also asks for HTTP Basic.
    pub(crate) fn has_users(&self) -> bool {
        !self
            .loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .users
            .is_empty()
    }

    /// The caller of a request with HTTP Basic credentials.
    fn sign_in(
        &self,
        config: &Config,
        header: &dyn Fn(&str) -> Option<String>,
        remote: Option<IpAddr>,
        name: &str,
        password: &str,
    ) -> Result<Caller, HttpError> {
        let remote = client_address(config, header, remote);
        if let Some(ip) = remote
            && self.too_many_failures(ip)
        {
            return Err(HttpError(
                429,
                "Too many requests with a wrong password; try again later".to_owned(),
            ));
        }
        let user = config.users.iter().find(|u| u.name == name);
        let stored: &crate::password::PasswordHash = match user {
            Some(u) => &u.password,
            None => crate::password::decoy(),
        };
        let mut memo = Vec::with_capacity(64 + name.len() + password.len());
        memo.extend_from_slice(&self.password_key);
        for part in [name, password, &format!("{stored:?}")] {
            memo.extend_from_slice(&(part.len() as u64).to_le_bytes());
            memo.extend_from_slice(part.as_bytes());
        }
        let memo = sha256(&memo);
        let remembered = self
            .passwords
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&memo)
            .is_some_and(|at| at.elapsed() < PASSWORD_REMEMBERED);
        // An unknown user costs as much as a known one, so the time taken does not tell which
        // names exist.
        let right = remembered || stored.verify(password.as_bytes());
        match user {
            Some(u) if right => {
                if !remembered {
                    let mut passwords = self.passwords.lock().unwrap_or_else(|e| e.into_inner());
                    if passwords.len() > 10_000 {
                        passwords.retain(|_, at| at.elapsed() < PASSWORD_REMEMBERED);
                    }
                    passwords.insert(memo, Instant::now());
                }
                if u.expires.is_some_and(|e| e <= SystemTime::now()) {
                    return Err(unauthorized("The user's access has expired"));
                }
                Ok(Caller {
                    agent: Some(u.agent.clone()),
                    groups: u.groups.clone(),
                    method: Method::Password,
                    datasets: u.datasets.clone(),
                })
            }
            _ => {
                if let Some(ip) = remote {
                    self.record_failure(ip);
                }
                Err(unauthorized("Wrong user name or password"))
            }
        }
    }

    /// Whether any policy has row rules (#202, #818).
    pub(crate) fn has_row_rules(&self) -> bool {
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let config = &loaded.config;
        config
            .datasets
            .values()
            .filter_map(|d| d.policies.as_ref())
            .chain(std::iter::once(&config.default_policies))
            .flatten()
            .any(|p| !p.rows.is_empty())
    }

    /// Whether any dataset policies are configured.
    pub(crate) fn policies_on(&self) -> bool {
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .policies_on
    }

    /// Whether the caller may propose changes to the dataset's shapes (#145) without being one
    /// of its admins: it is named in `datasets.<name>.shapeEditors`, as an agent or a group.
    pub(crate) fn edits_shapes(&self, caller: &Caller, names: &[&str]) -> bool {
        if !caller.may_use(names) {
            return false;
        }
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let config = &loaded.config;
        names
            .iter()
            .find_map(|n| config.datasets.get(*n))
            .is_some_and(|d| {
                d.shape_editors.iter().any(|a| {
                    caller.agent.as_deref() == Some(a.as_str()) || caller.groups.contains(a)
                })
            })
    }

    /// What `caller` may do with a dataset, by the names it was asked for: the name in the
    /// URL (perhaps an alias, whose policies then apply, so a swap does not change who can
    /// read) and the dataset's own.
    pub(crate) fn access(&self, caller: &Caller, names: &[&str]) -> Access {
        if !caller.may_use(names) {
            return Access::default();
        }
        if caller.is_admin() {
            return Access {
                read: true,
                write: true,
                admin: true,
                partial: false,
                partial_write: false,
            };
        }
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let config = &loaded.config;
        let dataset = names.iter().find_map(|n| config.datasets.get(*n));
        let admin = dataset.is_some_and(|d| {
            d.admins
                .iter()
                .any(|a| caller.agent.as_deref() == Some(a.as_str()) || caller.groups.contains(a))
        });
        if admin {
            return Access {
                read: true,
                write: true,
                admin: true,
                partial: false,
                partial_write: false,
            };
        }
        let writer = caller.in_group(WRITER);
        if !config.policies_on {
            return Access {
                read: true,
                write: writer,
                admin: false,
                partial: false,
                partial_write: false,
            };
        }
        let policies = names
            .iter()
            .find_map(|n| config.datasets.get(*n)?.policies.as_ref())
            .unwrap_or(&config.default_policies);
        let mut access = Access {
            read: writer,
            write: writer,
            admin: false,
            partial: false,
            partial_write: false,
        };
        for p in policies {
            if p.applies_to(caller.agent.as_deref(), &caller.groups) {
                access.read |= p.read;
                access.write |= p.write;
                access.partial |= !p.read_graphs.is_empty();
                access.partial_write |= !p.write_graphs.is_empty();
            }
        }
        access.partial &= !access.read;
        access.partial_write &= !access.write;
        access
    }

    /// The graphs of the dataset that `caller` may read when its access is
    /// [`Access::partial`]: those of every `sg:readGraph` of the policies that apply to it.
    pub(crate) fn readable_graphs(&self, caller: &Caller, names: &[&str]) -> ReadableGraphs {
        self.granted_graphs(caller, names, |p| &p.read_graphs)
    }

    /// The row rules (#202) of the policies that apply to `caller` on the dataset, with the
    /// graph their shapes are in; `None` when there are none (and always for admins, who see
    /// everything).
    pub(crate) fn row_rules(
        &self,
        caller: &Caller,
        names: &[&str],
    ) -> Option<(Arc<tripletrail::RowShapes>, Vec<tripletrail::RowRule>)> {
        if self.access(caller, names).admin {
            return None;
        }
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let config = &loaded.config;
        if !config.policies_on {
            return None;
        }
        let policies = names
            .iter()
            .find_map(|n| config.datasets.get(*n)?.policies.as_ref())
            .unwrap_or(&config.default_policies);
        let mut shapes = None;
        let mut rules: Vec<tripletrail::RowRule> = Vec::new();
        for p in policies {
            if p.rows.is_empty() || !p.applies_to(caller.agent.as_deref(), &caller.groups) {
                continue;
            }
            shapes = shapes.or_else(|| p.shapes.clone());
            for r in &p.rows {
                if !rules.contains(r) {
                    rules.push(r.clone());
                }
            }
        }
        Some((shapes?, rules))
    }

    /// The IRIs of the policies of the dataset that apply to `caller` (#202, the access
    /// explain page).
    pub(crate) fn applying_policies(&self, caller: &Caller, names: &[&str]) -> Vec<String> {
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let config = &loaded.config;
        if !config.policies_on {
            return Vec::new();
        }
        let policies = names
            .iter()
            .find_map(|n| config.datasets.get(*n)?.policies.as_ref())
            .unwrap_or(&config.default_policies);
        policies
            .iter()
            .filter(|p| p.applies_to(caller.agent.as_deref(), &caller.groups))
            .map(|p| p.id.clone())
            .collect()
    }

    /// The graphs of the dataset that `caller` may write when its access is
    /// [`Access::partial_write`]: those of every `sg:writeGraph` of the policies that apply.
    pub(crate) fn writable_graphs(&self, caller: &Caller, names: &[&str]) -> ReadableGraphs {
        self.granted_graphs(caller, names, |p| &p.write_graphs)
    }

    fn granted_graphs(
        &self,
        caller: &Caller,
        names: &[&str],
        grants: impl Fn(&Policy) -> &Vec<String>,
    ) -> ReadableGraphs {
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let config = &loaded.config;
        let policies = names
            .iter()
            .find_map(|n| config.datasets.get(*n)?.policies.as_ref())
            .unwrap_or(&config.default_policies);
        let mut readable = ReadableGraphs::default();
        for p in policies {
            if !p.applies_to(caller.agent.as_deref(), &caller.groups) {
                continue;
            }
            for graph in grants(p) {
                if graph == policy::DEFAULT_GRAPH {
                    readable.default_graph = true;
                } else if let Ok(n) = NamedNode::new(graph)
                    && !readable.named.contains(&n)
                {
                    readable.named.push(n);
                }
            }
        }
        readable
    }

    fn too_many_failures(&self, ip: IpAddr) -> bool {
        let failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        failures
            .get(&ip)
            .is_some_and(|(n, since)| *n >= MAX_FAILURES && since.elapsed() < FAILURE_WINDOW)
    }

    fn record_failure(&self, ip: IpAddr) {
        let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        if failures.len() > 10_000 {
            failures.retain(|_, (_, since)| since.elapsed() < FAILURE_WINDOW);
        }
        let entry = failures.entry(ip).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= FAILURE_WINDOW {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }
}

/// The address failed keys are counted against: behind a listed trusted proxy, the address the
/// proxy appended to `X-Forwarded-For` (the last one), so one bad client does not lock out
/// everyone behind the same proxy; otherwise the connection's.
fn client_address(
    config: &Config,
    header: &dyn Fn(&str) -> Option<String>,
    remote: Option<IpAddr>,
) -> Option<IpAddr> {
    let from_proxy = config.headers.as_ref().is_some_and(|h| {
        h.proxies_listed
            && remote.is_some_and(|ip| h.trusted_proxies.iter().any(|c| c.contains(ip)))
    });
    if from_proxy
        && let Some(forwarded) = header("X-Forwarded-For")
        && let Some(ip) = forwarded
            .rsplit(',')
            .next()
            .and_then(|a| a.trim().parse::<IpAddr>().ok())
    {
        return Some(ip);
    }
    remote
}

/// A 401 (the reply gets `WWW-Authenticate`).
/// The user name and password of an `Authorization: Basic` header: `None` for another
/// scheme, `Some(None)` when they cannot be read.
fn basic(header: &str) -> Option<Option<(String, String)>> {
    let (scheme, value) = header.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    Some(
        crate::password::base64_decode(value.trim())
            .and_then(|d| String::from_utf8(d).ok())
            .and_then(|d| {
                let (name, password) = d.split_once(':')?;
                Some((name.to_owned(), password.to_owned()))
            }),
    )
}

pub(crate) fn unauthorized(message: &str) -> HttpError {
    HttpError(401, message.to_owned())
}

/// The header with which a server admin runs a request as someone else ("view as", #205): an
/// agent IRI, or `anonymous`. [`VIEW_AS_GROUPS`] gives that caller's groups.
pub const VIEW_AS: &str = "X-TripleTrail-View-As";
/// The groups of the [`VIEW_AS`] caller, separated by commas.
pub const VIEW_AS_GROUPS: &str = "X-TripleTrail-View-As-Groups";

/// The caller a request runs as: `caller`, or, when a server admin sends [`VIEW_AS`], the
/// caller named there, who then has that caller's access and no more (#205). Anyone else
/// sending the header gets 403, so a view is never silently someone else's.
pub(crate) fn view_as(
    caller: Caller,
    header: &dyn Fn(&str) -> Option<String>,
) -> Result<Caller, HttpError> {
    let Some(agent) = header(VIEW_AS).map(|a| a.trim().to_owned()) else {
        return Ok(caller);
    };
    if !caller.is_admin() {
        return Err(refuse(
            &caller,
            "Viewing as someone else needs the group tripletrail:admin",
        ));
    }
    let groups: Vec<String> = header(VIEW_AS_GROUPS)
        .map(|g| {
            g.split(',')
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if agent.is_empty() || agent.eq_ignore_ascii_case("anonymous") {
        if !groups.is_empty() {
            return Err(HttpError::bad_request(
                "An anonymous caller has no groups: leave X-TripleTrail-View-As-Groups out",
            ));
        }
        return Ok(Caller {
            agent: None,
            groups,
            method: Method::Anonymous,
            datasets: None,
        });
    }
    if !is_iri(&agent) {
        return Err(HttpError::bad_request(format!(
            "{VIEW_AS} must be an agent IRI or anonymous, not {agent:?}"
        )));
    }
    Ok(Caller {
        agent: Some(agent),
        groups,
        method: Method::ApiKey,
        datasets: None,
    })
}

/// 401 for a caller without an identity, 403 for one whose identity is not enough.
pub(crate) fn refuse(caller: &Caller, message: &str) -> HttpError {
    if caller.method == Method::Anonymous {
        unauthorized(message)
    } else {
        HttpError(403, message.to_owned())
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn modified_all(path: &Path, files: &[PathBuf]) -> Vec<Option<SystemTime>> {
    std::iter::once(path)
        .chain(files.iter().map(PathBuf::as_path))
        .map(modified)
        .collect()
}

/// Edits of the configuration and policy files, one at a time.
static EDITS: Mutex<()> = Mutex::new(());

/// The `auth` object of a configuration as written: its `auth` member when it has one,
/// otherwise the configuration itself (as [`parse_auth`] reads it).
fn auth_object(
    root: &mut serde_json::Value,
) -> Result<&mut serde_json::Map<String, serde_json::Value>, String> {
    let root = root
        .as_object_mut()
        .ok_or("The configuration is not a JSON object")?;
    if root.contains_key("auth") {
        root.get_mut("auth")
            .and_then(|a| a.as_object_mut())
            .ok_or_else(|| "auth must be an object".to_owned())
    } else {
        Ok(root)
    }
}

/// Writes `text` to `path` through a temporary file in the same folder and a rename, so a
/// crash never leaves a truncated configuration or policy file. The file keeps its
/// permissions (a configuration holding key hashes may be 0600); a new file gets those of
/// `like`. A symbolic link (a configuration mounted from elsewhere) stays a link: its target
/// is written.
pub(crate) fn write_atomic(path: &Path, text: &str, like: &Path) -> Result<(), String> {
    let shown = |e: std::io::Error| format!("{}: {e}", path.display());
    let path = if path.is_symlink() {
        match std::fs::canonicalize(path) {
            Ok(target) => target,
            // A link to nothing: write through it.
            Err(_) => return std::fs::write(path, text).map_err(shown),
        }
    } else {
        path.to_owned()
    };
    let permissions = std::fs::metadata(&path)
        .or_else(|_| std::fs::metadata(like))
        .map(|m| m.permissions())
        .ok();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        // Before the content goes in, so it is never readable by more than the old file.
        if let Some(p) = &permissions {
            file.set_permissions(p.clone())?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        shown(e)
    })
}

fn read_config(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));
    parse_config(&text, base).map_err(|e| format!("{}: {e}", path.display()))
}

/// Parses a sparql-guard configuration (its `auth` object is used) or a bare `auth` object,
/// plus the policies: `policies` (as in sparql-guard) and TripleTrail's `datasets`, with policy
/// files relative to `base`.
fn parse_config(text: &str, base: &Path) -> Result<Config, String> {
    let root: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let root = substitute_env(root)?;
    let mut config = parse_auth(&root)?;
    let mut all_files = Vec::new();
    let mut load =
        |key: &str, v: &serde_json::Value| -> Result<(Vec<Policy>, Vec<PathBuf>), String> {
            let files: Vec<PathBuf> = match v {
                serde_json::Value::String(s) => vec![base.join(s)],
                serde_json::Value::Array(_) => strings(&serde_json::json!({ "f": v }), "f")
                    .map_err(|e| format!("{key}: {e}"))?
                    .into_iter()
                    .map(|f| base.join(f))
                    .collect(),
                _ => return Err(format!("{key} must be a file or a list of files")),
            };
            let policies = policy::read(&files)?;
            all_files.extend(files.iter().cloned());
            Ok((policies, files))
        };
    if let Some(v) = root.get("policies") {
        let (policies, files) = load("policies", v)?;
        config.default_policies = policies;
        config.default_files = files;
        config.policies_on = true;
    }
    if let Some(datasets) = root.get("datasets") {
        let datasets = datasets
            .as_object()
            .ok_or("datasets must be an object of dataset names")?;
        for (name, d) in datasets {
            let (policies, files) = match d.get("policies") {
                Some(v) => {
                    let (p, f) = load(&format!("datasets.{name}.policies"), v)?;
                    (Some(p), f)
                }
                None => (None, Vec::new()),
            };
            let admins =
                strings(d, "admins").map_err(|e| format!("datasets.{name}.admins: {e}"))?;
            let shape_editors = strings(d, "shapeEditors")
                .map_err(|e| format!("datasets.{name}.shapeEditors: {e}"))?;
            config.datasets.insert(
                name.clone(),
                DatasetAccess {
                    policies,
                    files,
                    admins,
                    shape_editors,
                },
            );
        }
    }
    config.files = all_files;
    config.policies_on |= config.datasets.values().any(|d| d.policies.is_some());
    Ok(config)
}

/// The `auth` object of a configuration (or the configuration itself, when it is one).
fn parse_auth(root: &serde_json::Value) -> Result<Config, String> {
    let auth = root.get("auth").unwrap_or(root);
    let auth = auth.as_object().ok_or("auth must be an object")?;
    let mut config = Config {
        anonymous: match auth.get("anonymous") {
            None => false,
            Some(v) => v.as_bool().ok_or("auth.anonymous must be true or false")?,
        },
        review: match auth.get("review") {
            None => false,
            Some(v) => v.as_bool().ok_or("auth.review must be true or false")?,
        },
        ..Config::default()
    };
    #[cfg(not(feature = "enterprise"))]
    if config.review {
        return Err("auth.review (reviewed access changes) needs TripleTrail Enterprise".into());
    }
    if let Some(keys) = auth.get("apiKeys") {
        for (i, k) in keys
            .as_array()
            .ok_or("auth.apiKeys must be an array")?
            .iter()
            .enumerate()
        {
            let at = |field: &str| format!("auth.apiKeys[{i}].{field}");
            let hex = string(k, "sha256").ok_or_else(|| format!("{} is missing", at("sha256")))?;
            let sha256 =
                parse_hex(hex).ok_or_else(|| format!("{} must be 64 hex digits", at("sha256")))?;
            let agent = string(k, "agent").ok_or_else(|| format!("{} is missing", at("agent")))?;
            config.keys.push(ApiKey {
                sha256,
                agent: agent.to_owned(),
                groups: strings(k, "groups").map_err(|e| format!("{}: {e}", at("groups")))?,
                datasets: match k.get("datasets") {
                    None => None,
                    Some(_) => Some(
                        strings(k, "datasets").map_err(|e| format!("{}: {e}", at("datasets")))?,
                    ),
                },
                label: string(k, "label").map(str::to_owned),
                expires_text: string(k, "expires").map(str::to_owned),
                expires: match string(k, "expires") {
                    None => None,
                    Some(t) => {
                        Some(parse_time(t).ok_or_else(|| {
                            format!("{} is not a date or date-time", at("expires"))
                        })?)
                    }
                },
            });
        }
    }
    if let Some(users) = auth.get("users") {
        for (i, u) in users
            .as_array()
            .ok_or("auth.users must be an array")?
            .iter()
            .enumerate()
        {
            let at = |field: &str| format!("auth.users[{i}].{field}");
            let name = string(u, "name").ok_or_else(|| format!("{} is missing", at("name")))?;
            if name.is_empty() || name.contains(':') {
                return Err(format!("{} must be a name without ':'", at("name")));
            }
            if config.users.iter().any(|v: &User| v.name == name) {
                return Err(format!("{}: {name} is listed twice", at("name")));
            }
            let password =
                string(u, "password").ok_or_else(|| format!("{} is missing", at("password")))?;
            let password = crate::password::PasswordHash::parse(password)
                .map_err(|e| format!("{}: {e}", at("password")))?;
            config.users.push(User {
                name: name.to_owned(),
                password,
                agent: string(u, "agent").map_or_else(
                    || format!("{DEFAULT_AGENT_PREFIX}{}", percent_encode(name)),
                    str::to_owned,
                ),
                groups: strings(u, "groups").map_err(|e| format!("{}: {e}", at("groups")))?,
                datasets: match u.get("datasets") {
                    None => None,
                    Some(_) => Some(
                        strings(u, "datasets").map_err(|e| format!("{}: {e}", at("datasets")))?,
                    ),
                },
                expires_text: string(u, "expires").map(str::to_owned),
                expires: match string(u, "expires") {
                    None => None,
                    Some(t) => {
                        Some(parse_time(t).ok_or_else(|| {
                            format!("{} is not a date or date-time", at("expires"))
                        })?)
                    }
                },
            });
        }
    }
    if let Some(o) = auth.get("oidc") {
        #[cfg(feature = "enterprise")]
        {
            let issuers = tripletrail_enterprise::oidc::parse(o)?;
            let verifier = Arc::new(tripletrail_enterprise::oidc::Verifier::new(
                issuers,
                Arc::new(fetch_document),
            ));
            config.sign_in = tripletrail_enterprise::sso::SignIn::new(
                Arc::clone(&verifier),
                Arc::new(post_form),
            )
            .map(Arc::new);
            config.oidc = Some(verifier);
        }
        #[cfg(not(feature = "enterprise"))]
        {
            let _ = o;
            return Err(
                "auth.oidc (single sign-on) is a TripleTrail Enterprise feature; this is the open \
                 edition"
                    .to_owned(),
            );
        }
    }
    if let Some(h) = auth.get("trustedHeaders") {
        let agent = string(h, "agent").ok_or("auth.trustedHeaders.agent is missing")?;
        let mut trusted_proxies: Vec<Cidr> = Vec::new();
        for c in strings(h, "trustedProxies")
            .map_err(|e| format!("auth.trustedHeaders.trustedProxies: {e}"))?
        {
            trusted_proxies.push(
                Cidr::parse(&c).ok_or_else(|| format!("{c} is not an address or CIDR range"))?,
            );
        }
        let proxies_listed = !trusted_proxies.is_empty();
        if !proxies_listed {
            trusted_proxies = vec![
                Cidr::parse("127.0.0.0/8").expect("valid"),
                Cidr::parse("::1").expect("valid"),
            ];
        }
        config.headers = Some(TrustedHeaders {
            proxies_listed,
            agent: agent.to_owned(),
            groups: string(h, "groups").map(str::to_owned),
            agent_prefix: string(h, "agentPrefix")
                .unwrap_or(DEFAULT_AGENT_PREFIX)
                .to_owned(),
            trusted_proxies,
        });
    }
    Ok(config)
}

/// Posts a form (a sign-in's code, to the provider's token endpoint) and gives the answer.
#[cfg(feature = "enterprise")]
fn post_form(url: &str, form: &[(&str, &str)]) -> Result<String, String> {
    let answer = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .redirects(0)
        .build()
        .post(url)
        .set("Accept", "application/json")
        .send_form(form);
    match answer {
        Ok(r) => r.into_string().map_err(|e| e.to_string()),
        // The provider says why (`{"error": "invalid_grant", ...}`).
        Err(ureq::Error::Status(code, r)) => Err(format!(
            "{code} {}",
            r.into_string()
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect::<String>()
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// Keeps the single sign-on of `old` (its fetched keys, sign-ins under way and sessions) when
/// a reloaded configuration trusts the same issuers in the same way.
#[cfg(feature = "enterprise")]
fn keep_sign_in(old: &Config, new: &mut Config) {
    if let (Some(o), Some(n)) = (&old.oidc, &new.oidc)
        && o.issuers() == n.issuers()
    {
        new.oidc = old.oidc.clone();
        new.sign_in = old.sign_in.clone();
    }
}

/// The session id in a request's `Cookie` header.
#[cfg(feature = "enterprise")]
pub(crate) fn session_cookie(header: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let cookies = header("Cookie")?;
    cookies.split(';').find_map(|c| {
        let (name, value) = c.trim().split_once('=')?;
        (name == tripletrail_enterprise::sso::COOKIE && !value.is_empty())
            .then(|| value.trim_matches('"').to_owned())
    })
}

/// Fetches an OIDC discovery document or JWKS (#198).
#[cfg(feature = "enterprise")]
fn fetch_document(url: &str) -> Result<String, String> {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .redirects(0)
        .build()
        .get(url)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

fn string<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

fn strings(v: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    match v.get(key) {
        None => Ok(Vec::new()),
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .map(|s| s.as_str().map(str::to_owned).ok_or("must hold strings"))
            .collect::<Result<_, _>>()
            .map_err(str::to_owned),
        Some(_) => Err("must be an array of strings".to_owned()),
    }
}

/// Replaces `${NAME}` in every string by the environment variable; an unset one is an error,
/// so a missing secret cannot become an empty credential.
fn substitute_env(value: serde_json::Value) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    Ok(match value {
        Value::String(s) => Value::String(expand(&s)?),
        Value::Array(a) => Value::Array(
            a.into_iter()
                .map(substitute_env)
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| Ok((k, substitute_env(v)?)))
                .collect::<Result<_, String>>()?,
        ),
        other => other,
    })
}

fn expand(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unclosed ${{ in {s:?}"))?;
        let name = &after[..end];
        out.push_str(
            &std::env::var(name)
                .map_err(|_| format!("the environment variable {name} is not set"))?,
        );
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn is_iri(s: &str) -> bool {
    ["http://", "https://", "urn:", "mailto:"]
        .iter()
        .any(|p| s.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p)))
}

/// Percent-encodes everything but unreserved characters (RFC 3986).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn parse_hex(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `YYYY-MM-DD` (midnight UTC) or `YYYY-MM-DDThh:mm:ss` with `Z` or `±hh:mm`.
fn parse_time(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    if s.as_bytes().get(4) != Some(&b'-') || s.as_bytes().get(7) != Some(&b'-') {
        return None;
    }
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut seconds = days_from_civil(y, m, d) * 86_400;
    if s.len() > 10 {
        if !matches!(s.as_bytes()[10], b'T' | b't' | b' ') {
            return None;
        }
        let (h, min, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
        if h > 23 || min > 59 || sec > 60 {
            return None;
        }
        seconds += h * 3600 + min * 60 + sec;
        let mut zone = &s[19..];
        if let Some(frac) = zone.strip_prefix('.') {
            zone = frac.trim_start_matches(|c: char| c.is_ascii_digit());
        }
        match zone {
            "Z" | "z" | "" => {}
            _ => {
                let sign = match zone.as_bytes()[0] {
                    b'+' => 1,
                    b'-' => -1,
                    _ => return None,
                };
                let oh = zone.get(1..3)?.parse::<i64>().ok()?;
                let om = zone.get(4..6)?.parse::<i64>().ok()?;
                if zone.len() != 6 || zone.as_bytes()[3] != b':' {
                    return None;
                }
                seconds -= sign * (oh * 3600 + om * 60);
            }
        }
    }
    let seconds = u64::try_from(seconds).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An address or a CIDR range.
#[derive(Clone, Debug)]
struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    fn parse(s: &str) -> Option<Self> {
        let (addr, prefix) = match s.trim().split_once('/') {
            Some((a, p)) => (a.parse::<IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
            None => (s.trim().parse::<IpAddr>().ok()?, None),
        };
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        (prefix <= max).then_some(Self {
            network: addr,
            prefix,
        })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 client on a dual-stack socket shows up as ::ffff:a.b.c.d.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        match (self.network, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(n) & mask == u32::from(a) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(n) & mask == u128::from(a) & mask
            }
            _ => false,
        }
    }
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(data).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_hash_with_sha256() {
        assert_eq!(
            to_hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            to_hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            to_hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 55, 56 and 64 bytes: the padding edge cases.
        for (n, hex) in [
            (
                55,
                "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318",
            ),
            (
                56,
                "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a",
            ),
            (
                64,
                "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
            ),
        ] {
            assert_eq!(to_hex(&sha256(&vec![b'a'; n])), hex, "{n}");
        }
    }

    #[test]
    fn times_and_ranges_parse() {
        let at = |s| {
            parse_time(s)
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        assert_eq!(at("1970-01-01"), 0);
        assert_eq!(at("2026-11-09"), 1_794_182_400);
        assert_eq!(at("2026-11-09T01:00:00+01:00"), 1_794_182_400);
        assert_eq!(at("2026-11-09T00:00:00.5Z"), 1_794_182_400);
        assert!(parse_time("2026-13-01").is_none());
        assert!(parse_time("tomorrow").is_none());

        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains("10.1.2.3".parse().unwrap()));
        assert!(c.contains("::ffff:10.1.2.3".parse().unwrap()));
        assert!(!c.contains("11.0.0.1".parse().unwrap()));
        assert!(
            Cidr::parse("127.0.0.1")
                .unwrap()
                .contains("127.0.0.1".parse().unwrap())
        );
        assert!(
            Cidr::parse("0.0.0.0/0")
                .unwrap()
                .contains("8.8.8.8".parse().unwrap())
        );
        assert!(
            Cidr::parse("fd00::/8")
                .unwrap()
                .contains("fd12::1".parse().unwrap())
        );
        assert!(Cidr::parse("10.0.0.0/33").is_none());
    }

    #[test]
    fn configurations_parse_like_sparql_guard() {
        // SAFETY: the test sets a variable no other test reads.
        unsafe {
            std::env::set_var(
                "TRIPLETRAIL_TEST_AUTH_PREFIX",
                "https://example.org/people/",
            )
        };
        let config = parse_config(
            r#"{"upstream": "http://x", "auth": {
                "apiKeys": [{"sha256": "9cca4fb4d7e3842c2fad316d5f496cdd3549e79828d778967e0983d2590c333f",
                             "agent": "https://example.org/people/jan", "groups": ["staff"],
                             "datasets": ["rinf"], "expires": "2099-01-01"}],
                "trustedHeaders": {"agent": "x-forwarded-user", "groups": "x-forwarded-groups",
                                   "agentPrefix": "${TRIPLETRAIL_TEST_AUTH_PREFIX}",
                                   "trustedProxies": ["10.0.0.0/8"]},
                "anonymous": true}}"#,
            Path::new("."),
        )
        .unwrap();
        assert_eq!(config.keys.len(), 1);
        assert_eq!(
            config.keys[0].datasets.as_deref(),
            Some(&["rinf".to_owned()][..])
        );
        let h = config.headers.unwrap();
        assert_eq!(h.agent_prefix, "https://example.org/people/");
        assert!(config.anonymous);
        // A bare auth object works too; errors name the field.
        assert!(parse_config(r#"{"anonymous": false}"#, Path::new(".")).is_ok());
        let e = parse_config(
            r#"{"apiKeys": [{"sha256": "00", "agent": "urn:a"}]}"#,
            Path::new("."),
        )
        .unwrap_err();
        assert!(e.contains("auth.apiKeys[0].sha256"), "{e}");
        let e = parse_config(
            r#"{"apiKeys": [], "trustedHeaders": {"agent": "${TRIPLETRAIL_UNSET_VAR_X}"}}"#,
            Path::new("."),
        )
        .unwrap_err();
        assert!(e.contains("TRIPLETRAIL_UNSET_VAR_X"), "{e}");
        assert_eq!(percent_encode("jan de vries"), "jan%20de%20vries");
    }
}
