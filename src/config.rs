use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{
    DependencyCondition, ExecCheck, ExitMode, HealthProbe, ProcessInstanceSpec, ProcessRuntime,
    ProcessStatus, RestartPolicy,
};

// ---------------------------------------------------------------------------
// Environment variable container
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct EnvVars(pub BTreeMap<String, String>);

impl EnvVars {
    pub fn merged(&self, other: &EnvVars) -> BTreeMap<String, String> {
        let mut out = self.0.clone();
        out.extend(other.0.clone());
        out
    }
}

impl<'de> Deserialize<'de> for EnvVars {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawEnv {
            Map(BTreeMap<String, String>),
            List(Vec<String>),
        }

        let raw = RawEnv::deserialize(deserializer)?;
        let mut env = BTreeMap::new();

        match raw {
            RawEnv::Map(m) => env.extend(m),
            RawEnv::List(entries) => {
                for entry in entries {
                    let (k, v) = entry.split_once('=').ok_or_else(|| {
                        serde::de::Error::custom("invalid env entry, expected KEY=VALUE")
                    })?;
                    env.insert(k.to_string(), v.to_string());
                }
            }
        }

        Ok(Self(env))
    }
}

// ---------------------------------------------------------------------------
// Config structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProjectConfig {
    #[serde(default)]
    pub environment: EnvVars,
    pub processes: BTreeMap<String, ProcessConfig>,
    #[serde(default)]
    pub disable_env_expansion: bool,
    #[serde(default)]
    pub exit_mode: ExitMode,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProcessConfig {
    pub command: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub environment: EnvVars,
    #[serde(default)]
    pub env_file: Vec<String>,
    #[serde(default)]
    pub depends_on: BTreeMap<String, ProcessDependency>,
    #[serde(default = "default_replicas")]
    pub replicas: u16,
    #[serde(default)]
    pub ready_log_line: Option<String>,
    #[serde(default)]
    pub restart_policy: Option<RestartPolicy>,
    #[serde(default)]
    pub backoff_seconds: Option<u64>,
    #[serde(default)]
    pub max_restarts: Option<u32>,
    #[serde(default)]
    pub shutdown: Option<ShutdownConfig>,
    #[serde(default)]
    pub readiness_probe: Option<HealthProbe>,
    #[serde(default)]
    pub liveness_probe: Option<HealthProbe>,
    #[serde(default)]
    pub disabled: bool,
}

fn default_replicas() -> u16 {
    1
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ShutdownConfig {
    #[serde(default = "default_signal")]
    pub signal: i32,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub command: Option<String>,
}

fn default_signal() -> i32 {
    15
}

fn default_timeout() -> u64 {
    10
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProcessDependency {
    #[serde(default)]
    pub condition: DependencyCondition,
}

// ---------------------------------------------------------------------------
// Loading and validation
// ---------------------------------------------------------------------------

pub fn load_config(path: &Path) -> Result<ProjectConfig> {
    let data = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let cfg: ProjectConfig = serde_yaml_ng::from_str(&data).context("invalid yaml")?;
    validate_config(&cfg)?;
    Ok(cfg)
}

pub fn load_and_merge_configs(paths: &[PathBuf]) -> Result<ProjectConfig> {
    assert!(!paths.is_empty(), "at least one config path is required");
    let mut cfg = load_config(&paths[0])?;
    for path in &paths[1..] {
        let overlay = load_config(path)?;
        cfg = merge_configs(cfg, overlay);
    }
    validate_config(&cfg)?;
    Ok(cfg)
}

/// Upper bound on `replicas` per process. Much higher than any sane local-dev
/// workload; designed to catch typos (e.g. `replicas: 1000`) before they fork
/// a thousand children.
pub const MAX_REPLICAS: u16 = 100;

/// Upper bound on `depends_on` DAG depth. Catches pathological configs where
/// the supervisor would walk an absurdly deep chain on every tick.
pub const MAX_DEPENDENCY_DEPTH: usize = 32;

pub fn validate_config(cfg: &ProjectConfig) -> Result<()> {
    if cfg.processes.is_empty() {
        bail!("config has no processes");
    }

    for (name, proc_cfg) in &cfg.processes {
        if proc_cfg.command.trim().is_empty() {
            bail!("process `{name}` has an empty command");
        }
        if proc_cfg.replicas == 0 {
            bail!("process `{name}` has replicas=0");
        }
        if proc_cfg.replicas > MAX_REPLICAS {
            bail!(
                "process `{name}` has replicas={}, which exceeds the limit of {MAX_REPLICAS}",
                proc_cfg.replicas
            );
        }
        if let Some(ref probe) = proc_cfg.readiness_probe {
            validate_probe(name, "readiness_probe", probe)?;
        }
        if let Some(ref probe) = proc_cfg.liveness_probe {
            validate_probe(name, "liveness_probe", probe)?;
        }
        for (dep, dep_cfg) in &proc_cfg.depends_on {
            if !cfg.processes.contains_key(dep) {
                bail!("process `{name}` depends on unknown process `{dep}`");
            }
            if dep_cfg.condition == DependencyCondition::ProcessLogReady {
                if let Some(dep_proc) = cfg.processes.get(dep) {
                    if dep_proc.ready_log_line.is_none() {
                        bail!(
                            "process `{name}` depends on `{dep}` with condition process_log_ready, \
                             but `{dep}` has no ready_log_line defined"
                        );
                    }
                }
            }
        }
    }

    detect_dependency_cycles(cfg)?;
    check_dependency_depth(cfg)?;

    Ok(())
}

/// Shared sanity checks for `readiness_probe` and `liveness_probe`.
///
/// Zero-valued periods/timeouts are nonsensical (the probe would fire every
/// tick and never succeed). `timeout_seconds > period_seconds` means a probe
/// could still be running when the next scheduling tick wants to start
/// another — reject outright. `timeout == period` is allowed but warned
/// because it leaves no slack for clock drift.
fn validate_probe(process: &str, kind: &str, probe: &HealthProbe) -> Result<()> {
    if probe.period_seconds == 0 {
        bail!("process `{process}` {kind}.period_seconds must be > 0");
    }
    if probe.timeout_seconds == 0 {
        bail!("process `{process}` {kind}.timeout_seconds must be > 0");
    }
    if probe.timeout_seconds > probe.period_seconds {
        bail!(
            "process `{process}` {kind}.timeout_seconds ({}) must be <= period_seconds ({})",
            probe.timeout_seconds,
            probe.period_seconds
        );
    }
    if probe.timeout_seconds == probe.period_seconds {
        eprintln!(
            "warning: process `{process}` {kind}.timeout_seconds == period_seconds ({}) \
             leaves no slack between probe attempts",
            probe.timeout_seconds
        );
    }
    if probe.success_threshold == 0 {
        bail!("process `{process}` {kind}.success_threshold must be > 0");
    }
    if probe.failure_threshold == 0 {
        bail!("process `{process}` {kind}.failure_threshold must be > 0");
    }
    Ok(())
}

/// Walk the dependency DAG and bail if any path exceeds [`MAX_DEPENDENCY_DEPTH`].
/// Assumes cycle detection has already run — so recursion terminates.
fn check_dependency_depth(cfg: &ProjectConfig) -> Result<()> {
    fn walk(node: &str, cfg: &ProjectConfig, depth: usize, stack: &mut Vec<String>) -> Result<()> {
        if depth > MAX_DEPENDENCY_DEPTH {
            stack.push(node.to_string());
            bail!(
                "dependency depth exceeds limit of {MAX_DEPENDENCY_DEPTH}: {}",
                stack.join(" -> ")
            );
        }
        stack.push(node.to_string());
        if let Some(proc) = cfg.processes.get(node) {
            for dep in proc.depends_on.keys() {
                walk(dep, cfg, depth + 1, stack)?;
            }
        }
        stack.pop();
        Ok(())
    }

    for start in cfg.processes.keys() {
        let mut stack: Vec<String> = Vec::new();
        walk(start, cfg, 0, &mut stack)?;
    }
    Ok(())
}

/// Walk the `depends_on` graph with a DFS three-coloring. Returns an error
/// describing the cycle if one is found.
///
/// Assumes all dependency names refer to existing processes (checked earlier
/// in `validate_config`). A self-dependency `a -> a` is reported as the
/// one-node cycle `a -> a`.
fn detect_dependency_cycles(cfg: &ProjectConfig) -> Result<()> {
    use std::collections::HashMap;

    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    fn dfs(
        node: &str,
        cfg: &ProjectConfig,
        color: &mut HashMap<String, Color>,
        path: &mut Vec<String>,
    ) -> Result<()> {
        color.insert(node.to_string(), Color::Gray);
        path.push(node.to_string());

        if let Some(proc) = cfg.processes.get(node) {
            for dep in proc.depends_on.keys() {
                match color.get(dep).copied().unwrap_or(Color::White) {
                    Color::Gray => {
                        let cycle_start = path.iter().position(|n| n == dep).unwrap_or(0);
                        let mut cycle: Vec<String> = path[cycle_start..].to_vec();
                        cycle.push(dep.clone());
                        bail!("dependency cycle detected: {}", cycle.join(" -> "));
                    }
                    Color::White => {
                        dfs(dep, cfg, color, path)?;
                    }
                    Color::Black => {}
                }
            }
        }

        color.insert(node.to_string(), Color::Black);
        path.pop();
        Ok(())
    }

    let mut color: HashMap<String, Color> = cfg
        .processes
        .keys()
        .map(|k| (k.clone(), Color::White))
        .collect();

    for start in cfg.processes.keys() {
        if color.get(start).copied() == Some(Color::White) {
            let mut path: Vec<String> = Vec::new();
            dfs(start, cfg, &mut color, &mut path)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Config merging
// ---------------------------------------------------------------------------

pub fn merge_configs(base: ProjectConfig, overlay: ProjectConfig) -> ProjectConfig {
    let mut env = base.environment.0;
    env.extend(overlay.environment.0);

    let mut processes = base.processes;
    for (name, overlay_proc) in overlay.processes {
        if let Some(base_proc) = processes.get_mut(&name) {
            base_proc.command = overlay_proc.command;
            if overlay_proc.description.is_some() {
                base_proc.description = overlay_proc.description;
            }
            if overlay_proc.working_dir.is_some() {
                base_proc.working_dir = overlay_proc.working_dir;
            }
            base_proc.environment.0.extend(overlay_proc.environment.0);
            base_proc.depends_on.extend(overlay_proc.depends_on);
            if !overlay_proc.env_file.is_empty() {
                base_proc.env_file = overlay_proc.env_file;
            }
            if overlay_proc.replicas != 1 {
                base_proc.replicas = overlay_proc.replicas;
            }
            if overlay_proc.ready_log_line.is_some() {
                base_proc.ready_log_line = overlay_proc.ready_log_line;
            }
            if overlay_proc.restart_policy.is_some() {
                base_proc.restart_policy = overlay_proc.restart_policy;
            }
            if overlay_proc.backoff_seconds.is_some() {
                base_proc.backoff_seconds = overlay_proc.backoff_seconds;
            }
            if overlay_proc.max_restarts.is_some() {
                base_proc.max_restarts = overlay_proc.max_restarts;
            }
            if overlay_proc.shutdown.is_some() {
                base_proc.shutdown = overlay_proc.shutdown;
            }
            if overlay_proc.readiness_probe.is_some() {
                base_proc.readiness_probe = overlay_proc.readiness_probe;
            }
            if overlay_proc.liveness_probe.is_some() {
                base_proc.liveness_probe = overlay_proc.liveness_probe;
            }
            if overlay_proc.disabled {
                base_proc.disabled = true;
            }
        } else {
            processes.insert(name, overlay_proc);
        }
    }

    ProjectConfig {
        environment: EnvVars(env),
        processes,
        disable_env_expansion: overlay.disable_env_expansion || base.disable_env_expansion,
        exit_mode: if overlay.exit_mode != ExitMode::WaitAll {
            overlay.exit_mode
        } else {
            base.exit_mode
        },
    }
}

// ---------------------------------------------------------------------------
// Process subset filtering (Phase A3)
// ---------------------------------------------------------------------------

/// Compute the set of process names that should be selected for launch,
/// expanding to include transitive dependencies when `include_deps` is true.
/// Does NOT mutate the config — the caller decides how to handle non-selected
/// services.
pub fn collect_process_subset(
    cfg: &ProjectConfig,
    names: &[String],
    include_deps: bool,
) -> Result<HashSet<String>> {
    for name in names {
        if !cfg.processes.contains_key(name) {
            bail!("unknown process `{name}`");
        }
    }

    let keep: HashSet<String> = if include_deps {
        let mut visited = HashSet::new();
        let mut queue: VecDeque<String> = names.iter().cloned().collect();
        while let Some(current) = queue.pop_front() {
            if !visited.insert(current.clone()) {
                continue;
            }
            if let Some(proc_cfg) = cfg.processes.get(&current) {
                for dep_name in proc_cfg.depends_on.keys() {
                    queue.push_back(dep_name.clone());
                }
            }
        }
        visited
    } else {
        names.iter().cloned().collect()
    };

    Ok(keep)
}

pub fn filter_process_subset(
    cfg: &mut ProjectConfig,
    names: &[String],
    include_deps: bool,
) -> Result<()> {
    let keep = collect_process_subset(cfg, names, include_deps)?;

    cfg.processes.retain(|name, _| keep.contains(name));

    // If --no-deps, strip depends_on references to excluded processes
    if !include_deps {
        for proc_cfg in cfg.processes.values_mut() {
            proc_cfg.depends_on.retain(|dep, _| keep.contains(dep));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Config path resolution
// ---------------------------------------------------------------------------

pub fn resolve_config_paths(user_supplied: &[PathBuf], cwd: &Path) -> Result<Vec<PathBuf>> {
    if user_supplied.is_empty() {
        let discovered = discover_config(cwd)?;
        let resolved = if discovered.exists() {
            discovered
                .canonicalize()
                .with_context(|| format!("failed to canonicalize {}", discovered.display()))?
        } else {
            discovered
        };
        return Ok(vec![resolved]);
    }

    let mut resolved = Vec::with_capacity(user_supplied.len());
    for path in user_supplied {
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let canonical = if abs.exists() {
            abs.canonicalize()
                .with_context(|| format!("failed to canonicalize {}", abs.display()))?
        } else {
            abs
        };
        resolved.push(canonical);
    }
    Ok(resolved)
}

pub fn discover_config(cwd: &Path) -> Result<PathBuf> {
    const CANDIDATES: [&str; 4] = [
        "decompose.yml",
        "decompose.yaml",
        "compose.yml",
        "compose.yaml",
    ];
    for name in CANDIDATES {
        let candidate = cwd.join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("no config file found (tried decompose.yml, decompose.yaml, compose.yml, compose.yaml)")
}

// ---------------------------------------------------------------------------
// .env file loading
// ---------------------------------------------------------------------------

pub fn parse_dotenv(path: &Path) -> Result<BTreeMap<String, String>> {
    let data = fs::read_to_string(path)
        .with_context(|| format!("failed to read env file {}", path.display()))?;
    parse_dotenv_with_source(&data, Some(&path.display().to_string()))
}

pub fn parse_dotenv_str(data: &str) -> Result<BTreeMap<String, String>> {
    parse_dotenv_with_source(data, None)
}

/// Parse a `.env`-style file. Malformed lines (no `=` separator, empty key)
/// are skipped but emit a warning on stderr so users can notice typos.
///
/// `source` is an optional label (typically the file path) used in warnings.
pub fn parse_dotenv_with_source(
    data: &str,
    source: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();

    for (idx, line) in data.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);

        let Some((key, value)) = trimmed.split_once('=') else {
            warn_malformed_dotenv(source, line_no, line, "missing '=' separator");
            continue;
        };

        let key = key.trim();
        if key.is_empty() {
            warn_malformed_dotenv(source, line_no, line, "empty key");
            continue;
        }

        let value = strip_quotes(value.trim());
        env.insert(key.to_string(), value);
    }

    Ok(env)
}

fn warn_malformed_dotenv(source: Option<&str>, line_no: usize, line: &str, reason: &str) {
    let src = source.unwrap_or("<env>");
    eprintln!(
        "warning: {src}:{line_no}: skipping malformed env line ({reason}): {:?}",
        line.trim_end()
    );
}

fn strip_quotes(s: &str) -> String {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        return s[1..s.len() - 1].to_string();
    }
    s.to_string()
}

pub fn load_dotenv_files(
    cwd: &Path,
    explicit: &[PathBuf],
    disable_auto: bool,
) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();

    if !disable_auto {
        let dotenv_path = cwd.join(".env");
        if dotenv_path.exists() {
            let parsed = parse_dotenv(&dotenv_path)?;
            env.extend(parsed);
        }
    }

    for path in explicit {
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let parsed = parse_dotenv(&abs)?;
        env.extend(parsed);
    }

    Ok(env)
}

// ---------------------------------------------------------------------------
// Variable interpolation
// ---------------------------------------------------------------------------

/// Matches, in priority order:
///   1. `$$` — literal `$` escape (consumes both dollars)
///   2. `${...}` — braced expansion (greedy up to the FIRST `}`)
///   3. `$IDENT` — unbraced expansion (alpha/underscore then
///      alphanumeric/underscore)
///   4. `$` — lone `$` with no valid follower; emitted as-is
///
/// The braced form intentionally uses `[^}]*` so a nested `${B:-c}` inside a
/// default is NOT parsed recursively — the scanner grabs the first close brace
/// and emits the default literally. This behavior is locked in by
/// `interpolate_nested_default_is_not_recursive`; do not "fix" it here.
static INTERPOLATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\$|\$\{([^}]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)|\$").unwrap());

pub fn interpolate_vars(input: &str, vars: &BTreeMap<String, String>) -> String {
    INTERPOLATE_RE
        .replace_all(input, |caps: &Captures<'_>| {
            let whole = &caps[0];
            if whole == "$$" {
                return "$".to_string();
            }
            if whole == "$" {
                // Lone `$` with no valid follower — emit verbatim.
                return "$".to_string();
            }
            if let Some(inner) = caps.get(1) {
                // `${...}` form — optionally with `:-default`.
                let (name, default) = match inner.as_str().split_once(":-") {
                    Some((n, d)) => (n, Some(d)),
                    None => (inner.as_str(), None),
                };
                return match lookup_var(name, vars) {
                    Some(v) => v,
                    None => default.unwrap_or("").to_string(),
                };
            }
            if let Some(name) = caps.get(2) {
                // `$IDENT` form — undefined becomes empty.
                return lookup_var(name.as_str(), vars).unwrap_or_default();
            }
            // Unreachable given the regex above, but fall back to the raw match.
            whole.to_string()
        })
        .into_owned()
}

fn lookup_var(name: &str, vars: &BTreeMap<String, String>) -> Option<String> {
    if let Some(v) = vars.get(name) {
        return Some(v.clone());
    }
    std::env::var(name).ok()
}

/// Walks the tree of interpolated config fields, substituting `${VAR}` and
/// `$VAR` references against `vars`. Each impl is responsible only for its
/// own string-valued fields and for recursing into children; containers like
/// `Option<T>` and `Vec<T>` are handled by blanket impls. Cross-cutting
/// concerns (building the global/per-process var sets, global-env sequential
/// evaluation, the `disable_env_expansion` short-circuit) live in
/// `apply_interpolation` rather than in the trait.
trait Interpolate {
    fn interpolate(&mut self, vars: &BTreeMap<String, String>);
}

impl Interpolate for String {
    fn interpolate(&mut self, vars: &BTreeMap<String, String>) {
        *self = interpolate_vars(self, vars);
    }
}

impl Interpolate for PathBuf {
    fn interpolate(&mut self, vars: &BTreeMap<String, String>) {
        *self = PathBuf::from(interpolate_vars(&self.to_string_lossy(), vars));
    }
}

impl<T: Interpolate> Interpolate for Option<T> {
    fn interpolate(&mut self, vars: &BTreeMap<String, String>) {
        if let Some(inner) = self {
            inner.interpolate(vars);
        }
    }
}

impl Interpolate for ExecCheck {
    fn interpolate(&mut self, vars: &BTreeMap<String, String>) {
        self.command.interpolate(vars);
    }
}

impl Interpolate for HealthProbe {
    fn interpolate(&mut self, vars: &BTreeMap<String, String>) {
        self.exec.interpolate(vars);
        // `http_get` has no interpolated fields today; if it gains one
        // (e.g. `path`), add it here.
    }
}

impl Interpolate for ShutdownConfig {
    fn interpolate(&mut self, vars: &BTreeMap<String, String>) {
        self.command.interpolate(vars);
    }
}

impl Interpolate for ProcessConfig {
    fn interpolate(&mut self, vars: &BTreeMap<String, String>) {
        self.command.interpolate(vars);
        self.description.interpolate(vars);
        self.working_dir.interpolate(vars);
        self.ready_log_line.interpolate(vars);
        self.shutdown.interpolate(vars);
        self.readiness_probe.interpolate(vars);
        self.liveness_probe.interpolate(vars);
        // `environment` is interpolated by `apply_interpolation` after its
        // caller has assembled the process-scoped var set; doing it here
        // would double-apply.
    }
}

pub fn apply_interpolation(cfg: &mut ProjectConfig) {
    if cfg.disable_env_expansion {
        return;
    }

    // Global env is interpolated sequentially so each entry can reference
    // keys that were declared earlier in the map.
    let mut global_vars = BTreeMap::new();
    let keys: Vec<String> = cfg.environment.0.keys().cloned().collect();
    for key in &keys {
        if let Some(raw) = cfg.environment.0.get(key) {
            let interpolated = interpolate_vars(raw, &global_vars);
            global_vars.insert(key.clone(), interpolated.clone());
            cfg.environment.0.insert(key.clone(), interpolated);
        }
    }

    for proc_cfg in cfg.processes.values_mut() {
        let mut vars = global_vars.clone();
        vars.extend(proc_cfg.environment.0.clone());

        proc_cfg.interpolate(&vars);

        // Per-process env entries resolve against a frozen snapshot that
        // already includes their own (uninterpolated) values, matching the
        // previous hand-written behavior.
        let env_keys: Vec<String> = proc_cfg.environment.0.keys().cloned().collect();
        for key in &env_keys {
            if let Some(raw) = proc_cfg.environment.0.get(key) {
                let interpolated = interpolate_vars(raw, &vars);
                proc_cfg.environment.0.insert(key.clone(), interpolated);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-service config hash
// ---------------------------------------------------------------------------

/// Serializable view of `ProcessConfig` with the fields that Docker Compose
/// treats as changeable-without-recreate (`depends_on`, `replicas`, `disabled`)
/// stripped out. Used exclusively to compute [`compute_config_hash`]. All
/// remaining fields are serialized through their `Serialize` impls; the
/// containers (`EnvVars` wraps a `BTreeMap`, `env_file` is a `Vec`) emit
/// deterministic key orderings, so `serde_json::to_vec` over this struct is
/// stable across runs.
#[derive(Serialize)]
struct ProcessConfigHashView<'a> {
    command: &'a str,
    description: &'a Option<String>,
    working_dir: &'a Option<PathBuf>,
    environment: &'a EnvVars,
    env_file: &'a Vec<String>,
    ready_log_line: &'a Option<String>,
    restart_policy: &'a Option<RestartPolicy>,
    backoff_seconds: &'a Option<u64>,
    max_restarts: &'a Option<u32>,
    shutdown: &'a Option<ShutdownConfig>,
    readiness_probe: &'a Option<HealthProbe>,
    liveness_probe: &'a Option<HealthProbe>,
}

/// Compute a stable SHA-256 hash of the service's `ProcessConfig`, **excluding**
/// `depends_on`, `replicas`, and `disabled`. These three fields can be changed
/// on a running compose stack without tearing down the underlying service —
/// mirroring Docker Compose's recreate semantics. Everything else (command,
/// environment, env files, working dir, probes, shutdown, restart policy, etc.)
/// contributes to the hash, so any change there means the service must be
/// recreated.
///
/// Used by the reload path to diff services: same hash == same definition,
/// different hash == tear down and respawn.
pub fn compute_config_hash(cfg: &ProcessConfig) -> String {
    let view = ProcessConfigHashView {
        command: &cfg.command,
        description: &cfg.description,
        working_dir: &cfg.working_dir,
        environment: &cfg.environment,
        env_file: &cfg.env_file,
        ready_log_line: &cfg.ready_log_line,
        restart_policy: &cfg.restart_policy,
        backoff_seconds: &cfg.backoff_seconds,
        max_restarts: &cfg.max_restarts,
        shutdown: &cfg.shutdown,
        readiness_probe: &cfg.readiness_probe,
        liveness_probe: &cfg.liveness_probe,
    };
    // serde_json serialization of structs is field-declaration-order stable,
    // and the containers we reference (BTreeMap, Vec, Option) are themselves
    // deterministic. Unwrap is safe: the view contains only json-serializable
    // leaf types (strings, numbers, maps keyed by String).
    let bytes = serde_json::to_vec(&view).expect("ProcessConfigHashView is serializable");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Process instance building
// ---------------------------------------------------------------------------

pub fn build_process_instances(
    cfg: &ProjectConfig,
    cwd: &Path,
    dotenv: &BTreeMap<String, String>,
) -> BTreeMap<String, ProcessRuntime> {
    let mut out = BTreeMap::new();

    for (base_name, proc_cfg) in &cfg.processes {
        // Hash once per service; replicas share the same config hash since
        // they're all spawned from the same ProcessConfig entry.
        let config_hash = compute_config_hash(proc_cfg);
        for idx in 0..proc_cfg.replicas {
            let replica = idx + 1;
            let instance_name = if proc_cfg.replicas > 1 {
                format!("{base_name}[{replica}]")
            } else {
                base_name.clone()
            };

            let mut env = dotenv.clone();
            env.extend(cfg.environment.0.clone());

            for env_file_path in &proc_cfg.env_file {
                let abs = if Path::new(env_file_path).is_absolute() {
                    PathBuf::from(env_file_path)
                } else {
                    cwd.join(env_file_path)
                };
                if let Ok(parsed) = parse_dotenv(&abs) {
                    env.extend(parsed);
                }
            }

            env.extend(proc_cfg.environment.0.clone());

            let working_dir = match &proc_cfg.working_dir {
                Some(d) if d.is_absolute() => d.clone(),
                Some(d) => cwd.join(d),
                None => cwd.to_path_buf(),
            };

            let depends_on = proc_cfg
                .depends_on
                .iter()
                .map(|(k, dep)| (k.clone(), dep.condition))
                .collect::<BTreeMap<_, _>>();

            let disabled = proc_cfg.disabled;

            let spec = ProcessInstanceSpec {
                name: instance_name.clone(),
                base_name: base_name.clone(),
                replica,
                command: proc_cfg.command.clone(),
                description: proc_cfg.description.clone(),
                working_dir,
                environment: env,
                depends_on,
                ready_log_line: proc_cfg.ready_log_line.clone(),
                restart_policy: proc_cfg.restart_policy.unwrap_or(RestartPolicy::No),
                backoff_seconds: proc_cfg.backoff_seconds.unwrap_or(1),
                max_restarts: proc_cfg.max_restarts,
                shutdown_signal: proc_cfg.shutdown.as_ref().map(|s| s.signal),
                shutdown_timeout_seconds: proc_cfg
                    .shutdown
                    .as_ref()
                    .map(|s| s.timeout_seconds)
                    .unwrap_or(10),
                shutdown_command: proc_cfg.shutdown.as_ref().and_then(|s| s.command.clone()),
                readiness_probe: proc_cfg.readiness_probe.clone(),
                liveness_probe: proc_cfg.liveness_probe.clone(),
                disabled,
                config_hash: config_hash.clone(),
            };

            let name_handle = crate::model::make_name_handle(instance_name.clone());
            out.insert(
                instance_name,
                ProcessRuntime {
                    spec,
                    status: if disabled {
                        ProcessStatus::Disabled
                    } else {
                        ProcessStatus::Pending
                    },
                    started_once: false,
                    log_ready: false,
                    restart_count: 0,
                    ready: false,
                    alive: true,
                    name_handle,
                },
            );
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn env_vars_deserialize_from_map() {
        let yaml = r#"
processes:
  a:
    command: "echo hi"
environment:
  A: "1"
  B: "2"
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        assert_eq!(cfg.environment.0.get("A"), Some(&"1".to_string()));
        assert_eq!(cfg.environment.0.get("B"), Some(&"2".to_string()));
    }

    #[test]
    fn env_vars_deserialize_from_list() {
        let yaml = r#"
processes:
  a:
    command: "echo hi"
environment:
  - A=1
  - B=2
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        assert_eq!(cfg.environment.0.get("A"), Some(&"1".to_string()));
        assert_eq!(cfg.environment.0.get("B"), Some(&"2".to_string()));
    }

    #[test]
    fn validate_rejects_unknown_dependency() {
        let yaml = r#"
processes:
  a:
    command: "echo hi"
    depends_on:
      missing:
        condition: process_started
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        let err = validate_config(&cfg).expect_err("must reject missing dep");
        assert!(err.to_string().contains("depends on unknown process"));
    }

    #[test]
    fn validate_rejects_log_ready_without_ready_log_line() {
        let yaml = r#"
processes:
  a:
    command: "echo hi"
    depends_on:
      b:
        condition: process_log_ready
  b:
    command: "echo"
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        let err = validate_config(&cfg).expect_err("must reject missing ready_log_line");
        assert!(err.to_string().contains("ready_log_line"));
    }

    #[test]
    fn validate_rejects_self_dependency() {
        let yaml = r#"
processes:
  a:
    command: "echo hi"
    depends_on:
      a:
        condition: process_started
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        let err = validate_config(&cfg).expect_err("must reject self dependency");
        assert!(
            err.to_string().contains("dependency cycle detected"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("a -> a"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_rejects_two_node_cycle() {
        let yaml = r#"
processes:
  a:
    command: "echo a"
    depends_on:
      b:
        condition: process_started
  b:
    command: "echo b"
    depends_on:
      a:
        condition: process_started
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        let err = validate_config(&cfg).expect_err("must reject cycle");
        assert!(
            err.to_string().contains("dependency cycle detected"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_rejects_transitive_cycle() {
        let yaml = r#"
processes:
  a:
    command: "echo a"
    depends_on:
      b:
        condition: process_started
  b:
    command: "echo b"
    depends_on:
      c:
        condition: process_started
  c:
    command: "echo c"
    depends_on:
      a:
        condition: process_started
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        let err = validate_config(&cfg).expect_err("must reject transitive cycle");
        assert!(
            err.to_string().contains("dependency cycle detected"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_accepts_dag() {
        // a -> b -> c, and a -> c directly. Not a cycle.
        let yaml = r#"
processes:
  a:
    command: "echo a"
    depends_on:
      b:
        condition: process_started
      c:
        condition: process_started
  b:
    command: "echo b"
    depends_on:
      c:
        condition: process_started
  c:
    command: "echo c"
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        validate_config(&cfg).expect("dag should validate");
    }

    #[test]
    fn validate_rejects_replicas_over_limit() {
        let yaml = format!(
            r#"
processes:
  a:
    command: "echo hi"
    replicas: {n}
"#,
            n = MAX_REPLICAS + 1
        );
        let cfg: ProjectConfig = serde_yaml_ng::from_str(&yaml).expect("parse config");
        let err = validate_config(&cfg).expect_err("must reject over-limit replicas");
        assert!(
            err.to_string().contains("exceeds the limit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_accepts_replicas_at_limit() {
        let yaml = format!(
            r#"
processes:
  a:
    command: "echo hi"
    replicas: {n}
"#,
            n = MAX_REPLICAS
        );
        let cfg: ProjectConfig = serde_yaml_ng::from_str(&yaml).expect("parse config");
        validate_config(&cfg).expect("exactly at the limit is allowed");
    }

    #[test]
    fn validate_rejects_zero_probe_period() {
        let yaml = r#"
processes:
  a:
    command: "echo"
    readiness_probe:
      exec:
        command: "true"
      period_seconds: 0
      timeout_seconds: 1
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        let err = validate_config(&cfg).expect_err("zero period rejected");
        assert!(
            err.to_string().contains("period_seconds must be > 0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_rejects_probe_timeout_greater_than_period() {
        let yaml = r#"
processes:
  a:
    command: "echo"
    liveness_probe:
      exec:
        command: "true"
      period_seconds: 5
      timeout_seconds: 10
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        let err = validate_config(&cfg).expect_err("timeout > period rejected");
        assert!(
            err.to_string().contains("must be <= period_seconds"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_accepts_probe_with_sane_period_and_timeout() {
        let yaml = r#"
processes:
  a:
    command: "echo"
    readiness_probe:
      exec:
        command: "true"
      period_seconds: 10
      timeout_seconds: 2
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        validate_config(&cfg).expect("sane probe accepted");
    }

    #[test]
    fn validate_rejects_zero_probe_thresholds() {
        let yaml = r#"
processes:
  a:
    command: "echo"
    readiness_probe:
      exec:
        command: "true"
      period_seconds: 5
      timeout_seconds: 1
      success_threshold: 0
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        let err = validate_config(&cfg).expect_err("zero success threshold rejected");
        assert!(
            err.to_string().contains("success_threshold must be > 0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_rejects_overly_deep_dependency_chain() {
        // Build a linear chain p0 -> p1 -> ... -> p(MAX+2) so depth exceeds
        // the limit. Every process depends on the next, no cycles.
        let total = MAX_DEPENDENCY_DEPTH + 3;
        let mut yaml = String::from("processes:\n");
        for i in 0..total {
            yaml.push_str(&format!("  p{i}:\n    command: \"echo\"\n"));
            if i + 1 < total {
                yaml.push_str("    depends_on:\n");
                yaml.push_str(&format!(
                    "      p{next}:\n        condition: process_started\n",
                    next = i + 1
                ));
            }
        }
        let cfg: ProjectConfig = serde_yaml_ng::from_str(&yaml).expect("parse config");
        let err = validate_config(&cfg).expect_err("deep chain rejected");
        assert!(
            err.to_string().contains("dependency depth exceeds limit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_accepts_log_ready_with_ready_log_line() {
        let yaml = r#"
processes:
  a:
    command: "echo hi"
    depends_on:
      b:
        condition: process_log_ready
  b:
    command: "echo ready"
    ready_log_line: "ready"
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        validate_config(&cfg).expect("should be valid");
    }

    #[test]
    fn build_instances_applies_replicas_and_injected_env() {
        let yaml = r#"
environment:
  GLOBAL: g
processes:
  api:
    command: "echo hi"
    replicas: 2
    environment:
      LOCAL: l
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        validate_config(&cfg).expect("valid config");
        let cwd = Path::new("/tmp");
        let dotenv = BTreeMap::new();
        let out = build_process_instances(&cfg, cwd, &dotenv);

        assert_eq!(out.len(), 2);
        let first = out.get("api[1]").expect("first replica");
        assert!(out.contains_key("api[2]"), "second replica");
        assert_eq!(first.spec.environment.get("GLOBAL"), Some(&"g".to_string()));
        assert_eq!(first.spec.environment.get("LOCAL"), Some(&"l".to_string()));
    }

    #[test]
    fn build_instances_includes_restart_fields() {
        let yaml = r#"
processes:
  api:
    command: "echo hi"
    restart_policy: on_failure
    backoff_seconds: 5
    max_restarts: 3
    shutdown:
      signal: 9
      timeout_seconds: 30
      command: "cleanup.sh"
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).expect("parse config");
        let cwd = Path::new("/tmp");
        let dotenv = BTreeMap::new();
        let out = build_process_instances(&cfg, cwd, &dotenv);

        let api = out.get("api").expect("api process");
        assert_eq!(api.spec.restart_policy, RestartPolicy::OnFailure);
        assert_eq!(api.spec.backoff_seconds, 5);
        assert_eq!(api.spec.max_restarts, Some(3));
        assert_eq!(api.spec.shutdown_signal, Some(9));
        assert_eq!(api.spec.shutdown_timeout_seconds, 30);
        assert_eq!(api.spec.shutdown_command, Some("cleanup.sh".to_string()));
    }

    #[test]
    fn discover_config_uses_documented_priority() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        fs::write(
            root.join("decompose.yml"),
            "processes: {a: {command: 'echo'}}",
        )
        .expect("write");
        fs::write(
            root.join("decompose.yaml"),
            "processes: {a: {command: 'echo'}}",
        )
        .expect("write");
        fs::write(
            root.join("compose.yml"),
            "processes: {a: {command: 'echo'}}",
        )
        .expect("write");
        fs::write(
            root.join("compose.yaml"),
            "processes: {a: {command: 'echo'}}",
        )
        .expect("write");

        let chosen = discover_config(root).expect("discover");
        assert_eq!(chosen, root.join("decompose.yml"));
    }

    #[test]
    fn merge_overlays_process_fields() {
        let base_yaml = r#"
processes:
  api:
    command: "echo base"
    description: "base desc"
    replicas: 1
    environment:
      A: "1"
"#;
        let overlay_yaml = r#"
processes:
  api:
    command: "echo overlay"
    replicas: 3
    environment:
      B: "2"
"#;
        let base: ProjectConfig = serde_yaml_ng::from_str(base_yaml).unwrap();
        let overlay: ProjectConfig = serde_yaml_ng::from_str(overlay_yaml).unwrap();
        let merged = merge_configs(base, overlay);

        let api = merged.processes.get("api").unwrap();
        assert_eq!(api.command, "echo overlay");
        assert_eq!(api.description.as_deref(), Some("base desc"));
        assert_eq!(api.replicas, 3);
        assert_eq!(api.environment.0.get("A"), Some(&"1".to_string()));
        assert_eq!(api.environment.0.get("B"), Some(&"2".to_string()));
    }

    #[test]
    fn merge_adds_new_processes() {
        let base_yaml = r#"
processes:
  api:
    command: "echo api"
"#;
        let overlay_yaml = r#"
processes:
  worker:
    command: "echo worker"
"#;
        let base: ProjectConfig = serde_yaml_ng::from_str(base_yaml).unwrap();
        let overlay: ProjectConfig = serde_yaml_ng::from_str(overlay_yaml).unwrap();
        let merged = merge_configs(base, overlay);

        assert!(merged.processes.contains_key("api"));
        assert!(merged.processes.contains_key("worker"));
    }

    #[test]
    fn merge_global_env() {
        let base_yaml = r#"
environment:
  A: "1"
  B: "base"
processes:
  x:
    command: "echo"
"#;
        let overlay_yaml = r#"
environment:
  B: "overlay"
  C: "3"
processes:
  x:
    command: "echo"
"#;
        let base: ProjectConfig = serde_yaml_ng::from_str(base_yaml).unwrap();
        let overlay: ProjectConfig = serde_yaml_ng::from_str(overlay_yaml).unwrap();
        let merged = merge_configs(base, overlay);

        assert_eq!(merged.environment.0.get("A"), Some(&"1".to_string()));
        assert_eq!(merged.environment.0.get("B"), Some(&"overlay".to_string()));
        assert_eq!(merged.environment.0.get("C"), Some(&"3".to_string()));
    }

    #[test]
    fn load_and_merge_works() {
        let dir = tempdir().unwrap();
        let base_path = dir.path().join("base.yaml");
        let overlay_path = dir.path().join("overlay.yaml");

        fs::write(
            &base_path,
            r#"
processes:
  api:
    command: "echo base"
    environment:
      PORT: "3000"
"#,
        )
        .unwrap();

        fs::write(
            &overlay_path,
            r#"
processes:
  api:
    command: "echo overlay"
    environment:
      PORT: "8080"
"#,
        )
        .unwrap();

        let cfg = load_and_merge_configs(&[base_path, overlay_path]).unwrap();
        let api = cfg.processes.get("api").unwrap();
        assert_eq!(api.command, "echo overlay");
        assert_eq!(api.environment.0.get("PORT"), Some(&"8080".to_string()));
    }

    #[test]
    fn merge_three_layer_overlay_last_wins() {
        // Three layers: base -> staging -> local. Each layer overrides part
        // of the previous one. Locks in that env maps layer additively
        // while simple scalars (command, working_dir) follow last-write-wins
        // and probes/shutdown get replaced as a whole (overlay struct wins).
        let base_yaml = r#"
environment:
  TIER: "base"
  ONLY_BASE: "b"
processes:
  api:
    command: "echo base"
    working_dir: "/srv/base"
    environment:
      PORT: "3000"
      FROM_BASE: "yes"
    readiness_probe:
      exec:
        command: "check-base"
      period_seconds: 5
    shutdown:
      signal: 15
      timeout_seconds: 5
"#;
        let staging_yaml = r#"
environment:
  TIER: "staging"
  ONLY_STAGING: "s"
processes:
  api:
    command: "echo staging"
    environment:
      PORT: "4000"
      FROM_STAGING: "yes"
    shutdown:
      signal: 2
      timeout_seconds: 15
"#;
        let local_yaml = r#"
environment:
  TIER: "local"
processes:
  api:
    command: "echo local"
    environment:
      PORT: "9000"
    readiness_probe:
      exec:
        command: "check-local"
      period_seconds: 2
"#;

        let base: ProjectConfig = serde_yaml_ng::from_str(base_yaml).unwrap();
        let staging: ProjectConfig = serde_yaml_ng::from_str(staging_yaml).unwrap();
        let local: ProjectConfig = serde_yaml_ng::from_str(local_yaml).unwrap();

        let merged = merge_configs(merge_configs(base, staging), local);

        // Global env: last-wins on conflicts, additive on distinct keys.
        assert_eq!(merged.environment.0.get("TIER"), Some(&"local".to_string()));
        assert_eq!(
            merged.environment.0.get("ONLY_BASE"),
            Some(&"b".to_string())
        );
        assert_eq!(
            merged.environment.0.get("ONLY_STAGING"),
            Some(&"s".to_string())
        );

        let api = merged.processes.get("api").unwrap();
        // Scalars: last file wins.
        assert_eq!(api.command, "echo local");
        // working_dir only set in base, middle & last layer don't touch it.
        assert_eq!(api.working_dir.as_deref(), Some(Path::new("/srv/base")));

        // Process env: all three layers contribute.
        assert_eq!(api.environment.0.get("PORT"), Some(&"9000".to_string()));
        assert_eq!(api.environment.0.get("FROM_BASE"), Some(&"yes".to_string()));
        assert_eq!(
            api.environment.0.get("FROM_STAGING"),
            Some(&"yes".to_string())
        );

        // Probe replaced by local (base's probe should be gone).
        let probe = api.readiness_probe.as_ref().unwrap();
        assert_eq!(probe.exec.as_ref().unwrap().command, "check-local");
        assert_eq!(probe.period_seconds, 2);

        // Shutdown only set in base+staging; staging wins since local left it
        // untouched — demonstrates "intermediate layers stick" behavior.
        let shutdown = api.shutdown.as_ref().unwrap();
        assert_eq!(shutdown.signal, 2);
        assert_eq!(shutdown.timeout_seconds, 15);
    }

    #[test]
    fn merge_overlay_without_matching_process_is_noop() {
        // Overlay referencing a new process adds it; overlay with *no*
        // process section doesn't wipe the base processes. This used to
        // catch a regression where an empty-processes overlay reset the
        // map.
        let base_yaml = r#"
processes:
  api:
    command: "echo api"
  worker:
    command: "echo worker"
"#;
        let overlay_yaml = r#"
environment:
  EXTRA: "1"
processes: {}
"#;
        let base: ProjectConfig = serde_yaml_ng::from_str(base_yaml).unwrap();
        let overlay: ProjectConfig = serde_yaml_ng::from_str(overlay_yaml).unwrap();
        let merged = merge_configs(base, overlay);

        assert_eq!(merged.processes.len(), 2);
        assert!(merged.processes.contains_key("api"));
        assert!(merged.processes.contains_key("worker"));
        assert_eq!(merged.environment.0.get("EXTRA"), Some(&"1".to_string()));
    }

    #[test]
    fn merge_preserves_base_fields_when_overlay_omits_them() {
        // When overlay only tweaks command, the rest of the base process
        // (description, env_file, depends_on, probe) should survive
        // untouched. Prevents accidental "overlay resets to defaults".
        let base_yaml = r#"
processes:
  db:
    command: "echo db"
  api:
    command: "echo base"
    description: "base description"
    env_file: ["base.env"]
    depends_on:
      db:
        condition: process_started
    liveness_probe:
      exec:
        command: "ping"
      period_seconds: 3
"#;
        let overlay_yaml = r#"
processes:
  api:
    command: "echo overlay"
"#;
        let base: ProjectConfig = serde_yaml_ng::from_str(base_yaml).unwrap();
        let overlay: ProjectConfig = serde_yaml_ng::from_str(overlay_yaml).unwrap();
        let merged = merge_configs(base, overlay);

        let api = merged.processes.get("api").unwrap();
        assert_eq!(api.command, "echo overlay");
        assert_eq!(api.description.as_deref(), Some("base description"));
        assert_eq!(api.env_file, vec!["base.env"]);
        assert!(api.depends_on.contains_key("db"));
        let probe = api.liveness_probe.as_ref().unwrap();
        assert_eq!(probe.exec.as_ref().unwrap().command, "ping");
    }

    #[test]
    fn filter_process_subset_with_deps() {
        let yaml = r#"
processes:
  db:
    command: "echo db"
  api:
    command: "echo api"
    depends_on:
      db:
        condition: process_started
  worker:
    command: "echo worker"
"#;
        let mut cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).unwrap();
        filter_process_subset(&mut cfg, &["api".to_string()], true).unwrap();
        assert!(cfg.processes.contains_key("api"));
        assert!(cfg.processes.contains_key("db"));
        assert!(!cfg.processes.contains_key("worker"));
    }

    #[test]
    fn filter_process_subset_no_deps() {
        let yaml = r#"
processes:
  db:
    command: "echo db"
  api:
    command: "echo api"
    depends_on:
      db:
        condition: process_started
  worker:
    command: "echo worker"
"#;
        let mut cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).unwrap();
        filter_process_subset(&mut cfg, &["api".to_string()], false).unwrap();
        assert!(cfg.processes.contains_key("api"));
        assert!(!cfg.processes.contains_key("db"));
        // depends_on should be stripped for excluded processes
        assert!(cfg.processes.get("api").unwrap().depends_on.is_empty());
    }

    #[test]
    fn filter_process_subset_rejects_unknown() {
        let yaml = r#"
processes:
  api:
    command: "echo"
"#;
        let mut cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let err = filter_process_subset(&mut cfg, &["nope".to_string()], true).unwrap_err();
        assert!(err.to_string().contains("unknown process"));
    }

    #[test]
    fn parse_dotenv_keeps_valid_lines_when_malformed_present() {
        // Malformed lines (no '=') should be skipped but not cause the parse
        // to fail; valid lines on either side of them must still be returned.
        // A warning is emitted to stderr — we can't capture that here without
        // forking tests, but we assert the good lines survive.
        let data = "GOOD1=yes\nbare_no_equals\n=empty_key_ignored\nGOOD2=also_yes\n";
        let env = parse_dotenv_str(data).expect("parse should not fail on malformed lines");
        assert_eq!(env.get("GOOD1"), Some(&"yes".to_string()));
        assert_eq!(env.get("GOOD2"), Some(&"also_yes".to_string()));
        // Empty-key line is dropped with a warning.
        assert!(!env.contains_key(""));
        // Stray bare line didn't get turned into anything accidentally.
        assert!(!env.contains_key("bare_no_equals"));
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn parse_dotenv_basic() {
        let data = r#"
# comment
KEY1=value1
KEY2="quoted value"
KEY3='single quoted'
export KEY4=exported

"#;
        let env = parse_dotenv_str(data).unwrap();
        assert_eq!(env.get("KEY1"), Some(&"value1".to_string()));
        assert_eq!(env.get("KEY2"), Some(&"quoted value".to_string()));
        assert_eq!(env.get("KEY3"), Some(&"single quoted".to_string()));
        assert_eq!(env.get("KEY4"), Some(&"exported".to_string()));
    }

    #[test]
    fn load_dotenv_from_cwd() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".env"), "AUTO=loaded\n").unwrap();

        let env = load_dotenv_files(dir.path(), &[], false).unwrap();
        assert_eq!(env.get("AUTO"), Some(&"loaded".to_string()));
    }

    #[test]
    fn load_dotenv_disabled() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".env"), "AUTO=loaded\n").unwrap();

        let env = load_dotenv_files(dir.path(), &[], true).unwrap();
        assert!(env.is_empty());
    }

    #[test]
    fn load_dotenv_explicit_overrides() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".env"), "KEY=auto\n").unwrap();
        fs::write(dir.path().join("custom.env"), "KEY=custom\n").unwrap();

        let explicit = vec![dir.path().join("custom.env")];
        let env = load_dotenv_files(dir.path(), &explicit, false).unwrap();
        assert_eq!(env.get("KEY"), Some(&"custom".to_string()));
    }

    #[test]
    fn dotenv_precedence_in_build_instances() {
        let yaml = r#"
environment:
  GLOBAL: from_config
processes:
  api:
    command: "echo"
    environment:
      LOCAL: from_process
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let cwd = Path::new("/tmp");
        let mut dotenv = BTreeMap::new();
        dotenv.insert("GLOBAL".to_string(), "from_dotenv".to_string());
        dotenv.insert("DOTONLY".to_string(), "dotenv_val".to_string());

        let out = build_process_instances(&cfg, cwd, &dotenv);
        let api = out.get("api").unwrap();
        assert_eq!(
            api.spec.environment.get("GLOBAL"),
            Some(&"from_config".to_string())
        );
        assert_eq!(
            api.spec.environment.get("DOTONLY"),
            Some(&"dotenv_val".to_string())
        );
        assert_eq!(
            api.spec.environment.get("LOCAL"),
            Some(&"from_process".to_string())
        );
    }

    #[test]
    fn interpolate_basic_braced() {
        let mut vars = BTreeMap::new();
        vars.insert("NAME".to_string(), "world".to_string());
        assert_eq!(interpolate_vars("hello ${NAME}", &vars), "hello world");
    }

    #[test]
    fn interpolate_basic_unbraced() {
        let mut vars = BTreeMap::new();
        vars.insert("NAME".to_string(), "world".to_string());
        assert_eq!(interpolate_vars("hello $NAME!", &vars), "hello world!");
    }

    #[test]
    fn interpolate_default_value() {
        let vars = BTreeMap::new();
        assert_eq!(interpolate_vars("${MISSING:-fallback}", &vars), "fallback");
    }

    #[test]
    fn interpolate_default_not_used_when_set() {
        let mut vars = BTreeMap::new();
        vars.insert("VAR".to_string(), "actual".to_string());
        assert_eq!(interpolate_vars("${VAR:-fallback}", &vars), "actual");
    }

    #[test]
    fn interpolate_dollar_escape() {
        let vars = BTreeMap::new();
        assert_eq!(interpolate_vars("price is $$5", &vars), "price is $5");
    }

    #[test]
    fn interpolate_undefined_becomes_empty() {
        let vars = BTreeMap::new();
        assert_eq!(
            interpolate_vars("hello ${UNDEF} world", &vars),
            "hello  world"
        );
    }

    #[test]
    fn interpolate_lone_dollar() {
        let vars = BTreeMap::new();
        assert_eq!(interpolate_vars("just $ here", &vars), "just $ here");
    }

    #[test]
    fn interpolate_multiple_vars() {
        let mut vars = BTreeMap::new();
        vars.insert("A".to_string(), "1".to_string());
        vars.insert("B".to_string(), "2".to_string());
        assert_eq!(interpolate_vars("$A and ${B}", &vars), "1 and 2");
    }

    #[test]
    fn interpolate_double_dollar_then_var() {
        // "$$VAR" should produce a literal "$" followed by the unexpanded
        // text "VAR" — the "$$" escape consumes both dollar signs, so the
        // remaining "VAR" is just plain text (no `$` prefix to kick off
        // another substitution). Set VAR anyway to prove that.
        let mut vars = BTreeMap::new();
        vars.insert("VAR".to_string(), "world".to_string());
        assert_eq!(interpolate_vars("$$VAR", &vars), "$VAR");
        assert_eq!(interpolate_vars("a$$b", &vars), "a$b");
        // Chaining: two escapes in a row still collapse independently.
        assert_eq!(interpolate_vars("$$$$", &vars), "$$");
    }

    #[test]
    fn interpolate_nested_default_is_not_recursive() {
        // Nested `${A:-${B:-c}}` is NOT supported: the scanner grabs the
        // first closing brace and treats everything up to it as the inner
        // expression. The default portion is emitted verbatim — no second
        // pass of interpolation over the fallback text. This test locks in
        // that behavior so a future change is made consciously.
        let mut vars = BTreeMap::new();
        vars.insert("B".to_string(), "bee".to_string());
        // A unset, B set: fallback is the raw literal "${B" (without the
        // inner close) and the outer scanner then prints the trailing "}"
        // as a plain character.
        assert_eq!(interpolate_vars("${A:-${B:-c}}", &vars), "${B:-c}");
        // A unset, simpler nested ${B}: fallback stays literal.
        assert_eq!(interpolate_vars("${A:-${B}}", &vars), "${B}");
        // A set: default branch never runs, so the nesting quirk is
        // invisible and it behaves normally.
        vars.insert("A".to_string(), "ay".to_string());
        assert_eq!(interpolate_vars("${A:-${B}}", &vars), "ay}");
    }

    #[test]
    fn interpolate_adjacent_substitutions() {
        // No separator between two variables — each resolves independently.
        let mut vars = BTreeMap::new();
        vars.insert("A".to_string(), "foo".to_string());
        vars.insert("B".to_string(), "bar".to_string());
        assert_eq!(interpolate_vars("${A}${B}", &vars), "foobar");
        assert_eq!(interpolate_vars("$A$B", &vars), "foobar");
    }

    #[test]
    fn interpolate_empty_default() {
        // `${VAR:-}` with VAR unset should produce the empty string (not
        // something like a literal `${VAR:-}`).
        let vars = BTreeMap::new();
        assert_eq!(interpolate_vars("x${UNSET:-}y", &vars), "xy");
    }

    #[test]
    fn interpolate_unterminated_brace() {
        // `${FOO` with no closing brace is not a valid expansion; the `$` is
        // emitted literally and the rest of the text is left untouched so the
        // user can see what they wrote.
        let mut vars = BTreeMap::new();
        vars.insert("FOO".to_string(), "ignored".to_string());
        assert_eq!(interpolate_vars("hi ${FOO", &vars), "hi ${FOO");
    }

    #[test]
    fn interpolate_var_at_end_of_string() {
        // Expansion that goes right up to the end of the input.
        let mut vars = BTreeMap::new();
        vars.insert("NAME".to_string(), "world".to_string());
        assert_eq!(interpolate_vars("hello ${NAME}", &vars), "hello world");
        assert_eq!(interpolate_vars("hello $NAME", &vars), "hello world");
    }

    #[test]
    fn apply_interpolation_on_config() {
        let yaml = r#"
environment:
  VERSION: "1.0"
processes:
  api:
    command: "run --version ${VERSION}"
    description: "API v${VERSION}"
"#;
        let mut cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).unwrap();
        apply_interpolation(&mut cfg);

        let api = cfg.processes.get("api").unwrap();
        assert_eq!(api.command, "run --version 1.0");
        assert_eq!(api.description.as_deref(), Some("API v1.0"));
    }

    #[test]
    fn apply_interpolation_on_probe_commands() {
        let yaml = r#"
environment:
  PORT: "4222"
processes:
  svc:
    command: "echo hi"
    readiness_probe:
      exec:
        command: "check --port ${PORT}"
      period_seconds: 5
    liveness_probe:
      exec:
        command: "alive --port $PORT"
      period_seconds: 10
"#;
        let mut cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).unwrap();
        apply_interpolation(&mut cfg);

        let svc = cfg.processes.get("svc").unwrap();
        assert_eq!(
            svc.readiness_probe
                .as_ref()
                .unwrap()
                .exec
                .as_ref()
                .unwrap()
                .command,
            "check --port 4222"
        );
        assert_eq!(
            svc.liveness_probe
                .as_ref()
                .unwrap()
                .exec
                .as_ref()
                .unwrap()
                .command,
            "alive --port 4222"
        );
    }

    #[test]
    fn apply_interpolation_disabled() {
        let yaml = r#"
disable_env_expansion: true
environment:
  VERSION: "1.0"
processes:
  api:
    command: "run --version ${VERSION}"
"#;
        let mut cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).unwrap();
        apply_interpolation(&mut cfg);

        let api = cfg.processes.get("api").unwrap();
        assert_eq!(api.command, "run --version ${VERSION}");
    }

    #[test]
    fn resolve_config_paths_empty_uses_discovery() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("decompose.yaml"),
            "processes: {a: {command: 'echo'}}",
        )
        .unwrap();

        let paths = resolve_config_paths(&[], dir.path()).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("decompose.yaml"));
    }

    #[test]
    fn resolve_config_paths_explicit() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("custom.yaml");
        fs::write(&p, "processes: {a: {command: 'echo'}}").unwrap();

        let paths = resolve_config_paths(std::slice::from_ref(&p), dir.path()).unwrap();
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn exit_mode_deserialization() {
        let yaml = r#"
exit_mode: exit_on_failure
processes:
  a:
    command: "echo"
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.exit_mode, ExitMode::ExitOnFailure);
    }

    #[test]
    fn config_hash_is_stable_and_sensitive() {
        // Baseline config with a broad mix of fields set.
        let yaml_a = r#"
processes:
  api:
    command: "run server"
    description: "the api"
    working_dir: "/srv"
    environment:
      PORT: "8080"
      LOG_LEVEL: "info"
    env_file:
      - "extra.env"
    ready_log_line: "listening"
    restart_policy: on_failure
    backoff_seconds: 5
    max_restarts: 3
    shutdown:
      signal: 15
      timeout_seconds: 10
      command: "cleanup.sh"
    readiness_probe:
      exec:
        command: "curl -f localhost"
      period_seconds: 5
      timeout_seconds: 1
    depends_on:
      db:
        condition: process_started
    replicas: 2
  db:
    command: "db"
"#;
        let cfg_a: ProjectConfig = serde_yaml_ng::from_str(yaml_a).unwrap();
        let api_a = cfg_a.processes.get("api").unwrap();
        let hash_a = compute_config_hash(api_a);

        // Identical config parsed independently must produce the same hash.
        let cfg_a2: ProjectConfig = serde_yaml_ng::from_str(yaml_a).unwrap();
        let hash_a2 = compute_config_hash(cfg_a2.processes.get("api").unwrap());
        assert_eq!(hash_a, hash_a2, "same config must hash the same");

        // Changing `command` must change the hash.
        let mut cfg_cmd = cfg_a.clone();
        cfg_cmd.processes.get_mut("api").unwrap().command = "run server --port 9000".to_string();
        let hash_cmd = compute_config_hash(cfg_cmd.processes.get("api").unwrap());
        assert_ne!(hash_a, hash_cmd, "command change must change hash");

        // Changing only `depends_on`, `replicas`, or `disabled` must NOT
        // change the hash — these are the Docker-Compose "mutable without
        // recreate" fields.
        let mut cfg_depends = cfg_a.clone();
        cfg_depends
            .processes
            .get_mut("api")
            .unwrap()
            .depends_on
            .clear();
        assert_eq!(
            hash_a,
            compute_config_hash(cfg_depends.processes.get("api").unwrap()),
            "depends_on change must NOT affect hash"
        );

        let mut cfg_replicas = cfg_a.clone();
        cfg_replicas.processes.get_mut("api").unwrap().replicas = 7;
        assert_eq!(
            hash_a,
            compute_config_hash(cfg_replicas.processes.get("api").unwrap()),
            "replicas change must NOT affect hash"
        );

        let mut cfg_disabled = cfg_a.clone();
        cfg_disabled.processes.get_mut("api").unwrap().disabled = true;
        assert_eq!(
            hash_a,
            compute_config_hash(cfg_disabled.processes.get("api").unwrap()),
            "disabled change must NOT affect hash"
        );

        // Changing environment (a non-excluded field) must change the hash.
        let mut cfg_env = cfg_a.clone();
        cfg_env
            .processes
            .get_mut("api")
            .unwrap()
            .environment
            .0
            .insert("NEW_VAR".to_string(), "x".to_string());
        assert_ne!(
            hash_a,
            compute_config_hash(cfg_env.processes.get("api").unwrap()),
            "environment change must change hash"
        );
    }

    #[test]
    fn build_instances_propagates_config_hash_to_replicas() {
        let yaml = r#"
processes:
  web:
    command: "serve"
    replicas: 3
"#;
        let cfg: ProjectConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let expected = compute_config_hash(cfg.processes.get("web").unwrap());
        let out = build_process_instances(&cfg, Path::new("/tmp"), &BTreeMap::new());
        assert_eq!(out.len(), 3);
        for runtime in out.values() {
            assert_eq!(runtime.spec.config_hash, expected);
        }
    }
}
