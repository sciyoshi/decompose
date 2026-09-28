//! Recursive composition and source-aware project resolution.
use super::raw::RawProject;
use super::*;

pub const MAX_INCLUDE_DEPTH: usize = MAX_DEPENDENCY_DEPTH;

#[derive(Debug, Serialize)]
pub struct ProcessProvenance {
    pub command_source: PathBuf,
    /// Contributing definitions, in merge order.
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Default, Serialize)]
pub struct Provenance {
    pub processes: BTreeMap<String, ProcessProvenance>,
}

#[derive(Debug)]
pub struct LoadedProject {
    pub config: ProjectConfig,
    pub dotenv: BTreeMap<String, String>,
    pub provenance: Provenance,
}

/// Resolve every entry point identically: root dotenv, composition,
/// interpolation, completeness/graph validation, then path validation.
pub fn load_project(
    paths: &[PathBuf],
    env_files: &[PathBuf],
    disable_dotenv: bool,
) -> Result<LoadedProject> {
    let first = paths
        .first()
        .context("at least one config path is required")?;
    let first = fs::canonicalize(first)
        .with_context(|| format!("failed to read config file {}", first.display()))?;
    let root = first
        .parent()
        .context("config file has no parent directory")?;
    let dotenv = load_dotenv_files(root, env_files, disable_dotenv)?;
    let mut vars = dotenv.clone();
    // Shell values override dotenv for interpolation, as in Compose. These
    // values are not copied into the explicit child environment.
    vars.extend(
        std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))),
    );
    let mut raw = RawProject::default();
    for path in paths {
        raw = raw.merge(compose(path, root, &vars, &mut Vec::new())?)?;
    }
    let provenance = Provenance {
        processes: raw
            .processes
            .iter()
            .map(|(name, process)| {
                (
                    name.clone(),
                    ProcessProvenance {
                        command_source: process.sources.get("command").cloned().unwrap_or_default(),
                        files: process.files.clone(),
                    },
                )
            })
            .collect(),
    };
    let mut config = raw
        .clone()
        .resolve()
        .context("invalid merged configuration")?;
    if !config.disable_env_expansion {
        interpolate(&mut config, &raw, root, &vars);
    }
    for (name, process) in &config.processes {
        if process.command.trim().is_empty() {
            bail!(
                "process `{name}` has an empty command from {}",
                provenance.processes[name].command_source.display()
            );
        }
    }
    validate_config(&config).with_context(|| {
        format!(
            "invalid configuration from {}",
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    for (name, process) in &config.processes {
        let source_dir = raw.processes[name]
            .sources
            .get("env_file")
            .and_then(|p| p.parent());
        validate_env_paths(name, &process.env_file, root, source_dir)?;
    }
    Ok(LoadedProject {
        config,
        dotenv,
        provenance,
    })
}

fn anchored(vars: &BTreeMap<String, String>, root: &Path, file: &Path) -> BTreeMap<String, String> {
    let mut vars = vars.clone();
    vars.insert(
        "DECOMPOSE_PROJECT_DIR".into(),
        root.to_string_lossy().into_owned(),
    );
    vars.insert(
        "DECOMPOSE_FILE_DIR".into(),
        file.parent().unwrap_or(root).to_string_lossy().into_owned(),
    );
    vars
}

fn compose(
    path: &Path,
    root: &Path,
    vars: &BTreeMap<String, String>,
    stack: &mut Vec<PathBuf>,
) -> Result<RawProject> {
    let path = fs::canonicalize(path).with_context(|| {
        format!(
            "failed to read included config {} (include chain: {})",
            path.display(),
            chain(stack)
        )
    })?;
    if stack.contains(&path) {
        bail!("include cycle: {} -> {}", chain(stack), path.display());
    }
    if stack.len() > MAX_INCLUDE_DEPTH {
        bail!(
            "include depth exceeds limit of {MAX_INCLUDE_DEPTH}: {} -> {}",
            chain(stack),
            path.display()
        );
    }
    stack.push(path.clone());
    let mut local = RawProject::read(&path)?;
    // Discovery uses only root dotenv/shell and this file's own global env;
    // imported environment cannot change which subsequent files get loaded.
    let mut discovery = anchored(vars, root, &path);
    for (key, value) in &local.environment.0 {
        let value = interpolate_vars(value, &anchored(&discovery, root, &path));
        discovery.insert(key.clone(), value);
    }
    discovery = anchored(&discovery, root, &path);
    let mut imported = RawProject::default();
    for include in std::mem::take(&mut local.include) {
        let (entry, selection) = include.parts();
        let expanded = interpolate_vars(entry, &discovery);
        if expanded.is_empty() {
            bail!("empty include path in {}: `{entry}`", path.display());
        }
        let include_path = path.parent().unwrap().join(expanded);
        let mut child = compose(&include_path, root, vars, stack)
            .with_context(|| format!("included by {}", path.display()))?;
        if let Some(names) = selection {
            let graph = child
                .processes
                .iter()
                .map(|(name, process)| {
                    Ok((
                        name.clone(),
                        process.dependencies()?.into_keys().collect::<Vec<_>>(),
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let selected = collect_subset(&graph, names, true)?;
            child.processes.retain(|name, _| selected.contains(name));
        }
        for (name, process) in &child.processes {
            if let Some(previous) = imported.processes.get(name)
                && !local.processes.contains_key(name)
            {
                bail!(
                    "included process `{name}` conflicts between {} and {}; define `{name}` in {} to override it",
                    chain(&previous.files),
                    chain(&process.files),
                    path.display()
                );
            }
        }
        // Only processes and environment are imported. Root-file overlays
        // alone control the daemon's global behavior.
        child.exit_mode = None;
        child.disable_env_expansion = None;
        imported = imported.merge(child)?;
    }
    stack.pop();
    imported.merge(local)
}

fn chain(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(" -> ")
}

fn interpolate(
    cfg: &mut ProjectConfig,
    raw: &RawProject,
    root: &Path,
    base: &BTreeMap<String, String>,
) {
    let mut global = base.clone();
    for (key, value) in &mut cfg.environment.0 {
        let source = &raw.environment_sources[key];
        *value = interpolate_vars(value, &anchored(&global, root, source));
        global.insert(key.clone(), value.clone());
    }
    for (name, process) in &mut cfg.processes {
        let origin = &raw.processes[name];
        let mut vars = global.clone();
        vars.extend(process.environment.0.clone());
        // Each field retains its defining file even when another file adds
        // a partial process override. Blocks and lists replace as a unit.
        macro_rules! field {
            ($field:ident) => {
                if let Some(source) = origin.sources.get(stringify!($field)) {
                    process.$field.interpolate(&anchored(&vars, root, source));
                }
            };
        }
        field!(command);
        field!(description);
        field!(working_dir);
        field!(env_file);
        field!(ready_log_line);
        field!(shutdown);
        field!(readiness_probe);
        field!(liveness_probe);
        // Hooks expand only after service env_file values are available.
        // Retain each phase's source without expanding commands twice.
        for (phase, hooks) in [
            ("pre_start", &mut process.pre_start),
            ("post_start", &mut process.post_start),
        ] {
            if let Some(source) = origin.sources.get(phase)
                && let Some(hooks) = hooks
            {
                for hook in hooks {
                    hook.interpolation_anchors = anchored(&BTreeMap::new(), root, source);
                }
            }
        }
        for (key, value) in &mut process.environment.0 {
            *value = interpolate_vars(
                value,
                &anchored(&vars, root, &origin.environment_sources[key]),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Fixture(TempDir);
    impl Fixture {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.path().canonicalize().unwrap().join(name)
        }
        fn write(&self, name: &str, content: &str) -> PathBuf {
            let path = self.path(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            path
        }
        fn load(&self) -> Result<LoadedProject> {
            load_project(&[self.path("project/root.yaml")], &[], false)
        }
        fn error(&self) -> String {
            format!("{:#}", self.load().unwrap_err())
        }
    }

    #[test]
    fn nested_includes_merge_globals_and_partial_overrides_with_provenance() {
        let f = Fixture::new();
        let leaf = f.write(
            "fragments/nested/db.yaml",
            "environment: {TIER: leaf, LEAF: yes}\nprocesses: {db: {command: database}}",
        );
        f.write("fragments/services.yaml", "include: [nested/db.yaml]\nenvironment: {TIER: fragment}\nprocesses: {api: {command: serve, environment: {PORT: '3000'}, depends_on: {db: {}}}}");
        let root = f.write("project/root.yaml", "include: [../fragments/services.yaml]\nenvironment: {TIER: root}\nprocesses: {api: {environment: {PORT: '9000'}}}");
        let loaded = f.load().unwrap();
        assert_eq!(loaded.config.environment.0["TIER"], "root");
        assert_eq!(loaded.config.environment.0["LEAF"], "yes");
        assert_eq!(loaded.config.processes["api"].command, "serve");
        assert_eq!(loaded.config.processes["api"].environment.0["PORT"], "9000");
        assert_eq!(loaded.provenance.processes["db"].command_source, leaf);
        assert_eq!(
            loaded.provenance.processes["api"].files,
            vec![f.path("fragments/services.yaml"), root]
        );
    }

    #[test]
    fn sibling_conflicts_require_an_immediate_local_definition() {
        let f = Fixture::new();
        f.write(
            "fragments/a.yaml",
            "processes: {db: {command: first, environment: {A: keep}}}",
        );
        f.write("fragments/b.yaml", "processes: {db: {command: second}}");
        let includes = "include: [../fragments/a.yaml, ../fragments/b.yaml]\n";
        f.write("project/root.yaml", includes);
        let error = f.error();
        assert!(
            error.contains("conflicts") && error.contains("a.yaml") && error.contains("b.yaml")
        );
        f.write(
            "project/root.yaml",
            &format!("{includes}processes: {{db: {{environment: {{B: local}}}}}}"),
        );
        let loaded = f.load().unwrap();
        assert_eq!(loaded.config.processes["db"].command, "second");
        assert_eq!(loaded.config.processes["db"].environment.0["A"], "keep");
        // A definition in an ancestor cannot resolve a conflict in a child.
        f.write("fragments/conflict.yaml", "include: [a.yaml, b.yaml]");
        f.write(
            "project/root.yaml",
            "include: [../fragments/conflict.yaml]\nprocesses: {db: {command: local}}",
        );
        assert!(f.error().contains("conflicts"));
    }

    #[test]
    fn selection_includes_transitive_dependencies_and_all_global_environment() {
        let f = Fixture::new();
        f.write("fragments/services.yaml", "environment: {SHARED: yes}\nprocesses:\n  api: {command: serve, depends_on: {cache: {}}}\n  cache: {command: cache, depends_on: {db: {}}}\n  db: {command: database}\n  unused: {command: unused}");
        f.write(
            "project/root.yaml",
            "include: [{path: ../fragments/services.yaml, processes: [api]}]",
        );
        let cfg = f.load().unwrap().config;
        assert_eq!(
            cfg.processes.keys().cloned().collect::<Vec<_>>(),
            ["api", "cache", "db"]
        );
        assert_eq!(cfg.environment.0["SHARED"], "yes");
        f.write("project/root.yaml", "include: [{path: ../fragments/services.yaml, processes: []}]\nprocesses: {local: {command: local}}");
        let cfg = f.load().unwrap().config;
        assert_eq!(cfg.processes.len(), 1);
        assert_eq!(cfg.environment.0["SHARED"], "yes");
        f.write(
            "project/root.yaml",
            "include: [{path: ../fragments/services.yaml, processes: [missing]}]",
        );
        assert!(f.error().contains("unknown process `missing`"));
    }

    #[test]
    fn final_validation_allows_cross_file_dependencies_but_rejects_missing_ones() {
        let f = Fixture::new();
        f.write(
            "fragments/api.yaml",
            "processes: {api: {command: serve, depends_on: {db: {}}}}",
        );
        f.write("project/root.yaml", "include: [{path: ../fragments/api.yaml, processes: [api]}]\nprocesses: {db: {command: database}}");
        f.load().unwrap();
        f.write("project/root.yaml", "include: [../fragments/api.yaml]");
        assert!(f.error().contains("unknown process `db`"));
        f.write("project/root.yaml", "include: [../fragments/api.yaml]\nprocesses: {db: {command: database, depends_on: {api: {}}}}");
        assert!(f.error().contains("cycle"));
    }

    #[test]
    fn missing_files_and_cycles_report_the_include_chain() {
        let f = Fixture::new();
        f.write("project/root.yaml", "include: [../fragments/a.yaml]");
        assert!(f.error().contains("a.yaml"));
        f.write("fragments/a.yaml", "include: [../project/root.yaml]");
        let error = f.error();
        assert!(
            error.contains("include cycle")
                && error.contains("a.yaml")
                && error.contains("root.yaml")
        );
    }

    #[test]
    fn include_depth_allows_32_edges_and_rejects_33() {
        let f = Fixture::new();
        f.write("project/root.yaml", "include: [0.yaml]");
        for i in 0..MAX_INCLUDE_DEPTH - 1 {
            f.write(
                &format!("project/{i}.yaml"),
                &format!("include: [{}.yaml]", i + 1),
            );
        }
        f.write("project/31.yaml", "processes: {ok: {command: echo}}");
        f.load().unwrap();
        f.write("project/31.yaml", "include: [32.yaml]");
        f.write("project/32.yaml", "processes: {ok: {command: echo}}");
        assert!(f.error().contains("include depth exceeds limit of 32"));
    }

    #[test]
    fn reused_file_is_not_a_cycle_and_selection_precedes_conflict_detection() {
        let f = Fixture::new();
        f.write(
            "fragments/services.yaml",
            "processes: {a: {command: a}, b: {command: b}}",
        );
        f.write("project/root.yaml", "include:\n  - {path: ../fragments/services.yaml, processes: [a]}\n  - {path: ../fragments/services.yaml, processes: [b]}");
        assert_eq!(f.load().unwrap().config.processes.len(), 2);
    }

    #[test]
    fn anchors_follow_individual_fields_and_environment_entries() {
        let f = Fixture::new();
        f.write(
            "fragments/services.yaml",
            r#"
environment:
  ASSET: '${DECOMPOSE_FILE_DIR}/asset'
processes:
  api:
    command: 'serve ${PORT} ${DECOMPOSE_FILE_DIR} ${DECOMPOSE_PROJECT_DIR} $${RUNTIME:-4222}'
    working_dir: work
    env_file: ['${DECOMPOSE_FILE_DIR}/fragment.env']
    environment:
      PORT: '3000'
      FILE: '${DECOMPOSE_FILE_DIR}'
    readiness_probe:
      exec: {command: 'check ${DECOMPOSE_FILE_DIR}'}
"#,
        );
        f.write("fragments/fragment.env", "FROM_FILE=yes");
        f.write(
            "project/root.yaml",
            r#"
include: ['${DECOMPOSE_FILE_DIR}/../fragments/services.yaml']
processes:
  api:
    description: '${DECOMPOSE_FILE_DIR}'
    environment: {PORT: '9000', ROOT: '${DECOMPOSE_FILE_DIR}'}
"#,
        );
        let loaded = f.load().unwrap();
        let api = &loaded.config.processes["api"];
        assert_eq!(
            api.command,
            format!(
                "serve 9000 {} {} ${{RUNTIME:-4222}}",
                f.path("fragments").display(),
                f.path("project").display()
            )
        );
        assert_eq!(api.description.as_deref(), f.path("project").to_str());
        assert_eq!(
            api.environment.0["FILE"],
            f.path("fragments").to_str().unwrap()
        );
        assert_eq!(
            api.environment.0["ROOT"],
            f.path("project").to_str().unwrap()
        );
        assert_eq!(
            loaded.config.environment.0["ASSET"],
            f.path("fragments/asset").to_str().unwrap()
        );
        let instances = build_process_instances(&loaded.config, &f.path("project"), &loaded.dotenv);
        assert_eq!(instances["api"].spec.working_dir, f.path("project/work"));
        assert_eq!(instances["api"].spec.environment["FROM_FILE"], "yes");
    }

    #[test]
    fn dotenv_is_root_only_and_explicit_files_control_discovery() {
        let f = Fixture::new();
        f.write(
            "project/.env",
            "DECOMPOSE_TEST_FRAGMENT=../fragments/a.yaml\nDECOMPOSE_TEST_VALUE=root\n",
        );
        f.write("fragments/.env", "DECOMPOSE_TEST_VALUE=fragment\n");
        f.write(
            "fragments/a.yaml",
            "processes: {a: {command: 'echo ${DECOMPOSE_TEST_VALUE}'}}",
        );
        f.write(
            "fragments/b.yaml",
            "processes: {b: {command: 'echo ${DECOMPOSE_TEST_VALUE}'}}",
        );
        let root = f.write(
            "project/root.yaml",
            "include: ['${DECOMPOSE_TEST_FRAGMENT}']",
        );
        assert_eq!(f.load().unwrap().config.processes["a"].command, "echo root");
        assert!(load_project(std::slice::from_ref(&root), &[], true).is_err());
        f.write(
            "project/custom.env",
            "DECOMPOSE_TEST_FRAGMENT=../fragments/b.yaml\nDECOMPOSE_TEST_VALUE=explicit\n",
        );
        let loaded = load_project(&[root], &[PathBuf::from("custom.env")], true).unwrap();
        assert_eq!(loaded.config.processes["b"].command, "echo explicit");
    }

    #[test]
    fn env_file_uses_project_root_and_only_its_own_source_directory_is_allowed() {
        let f = Fixture::new();
        f.write("project/local.env", "VALUE=project");
        f.write("fragments/local.env", "VALUE=fragment");
        f.write("elsewhere/secret.env", "VALUE=outside");
        f.write(
            "fragments/a.yaml",
            "processes: {a: {command: run, env_file: [local.env]}}",
        );
        f.write("project/root.yaml", "include: [../fragments/a.yaml]");
        let loaded = f.load().unwrap();
        let env = resolve_process_env(
            &loaded.config.processes["a"],
            &loaded.config,
            &f.path("project"),
            &loaded.dotenv,
        );
        assert_eq!(env["VALUE"], "project");
        f.write("fragments/a.yaml", "processes: {a: {command: run, env_file: ['${DECOMPOSE_FILE_DIR}/../elsewhere/secret.env']}}");
        assert!(f.error().contains("outside the project directory"));
        // The root cannot use a fragment's allowance when it replaces env_file.
        f.write(
            "project/root.yaml",
            "include: [../fragments/a.yaml]\nprocesses: {a: {env_file: [../fragments/local.env]}}",
        );
        assert!(f.error().contains("outside the project directory"));
    }

    #[cfg(unix)]
    #[test]
    fn canonical_paths_detect_symlink_cycles_and_env_file_escapes() {
        let f = Fixture::new();
        f.write("project/root.yaml", "include: [alias.yaml]");
        std::os::unix::fs::symlink(f.path("project/root.yaml"), f.path("project/alias.yaml"))
            .unwrap();
        assert!(f.error().contains("include cycle"));
        f.write("outside/secret.env", "SECRET=yes");
        f.write(
            "fragments/a.yaml",
            "processes: {a: {command: run, env_file: ['${DECOMPOSE_FILE_DIR}/escape.env']}}",
        );
        std::os::unix::fs::symlink(f.path("outside/secret.env"), f.path("fragments/escape.env"))
            .unwrap();
        f.write("project/root.yaml", "include: [../fragments/a.yaml]");
        assert!(f.error().contains("outside the project directory"));
    }

    #[test]
    fn root_settings_control_content_expansion_and_root_overlays_win() {
        let f = Fixture::new();
        f.write("fragments/a.yaml", "disable_env_expansion: true\nexit_mode: exit_on_end\nprocesses: {a: {command: 'echo ${VALUE}'}}");
        let root = f.write(
            "project/root.yaml",
            "include: [../fragments/a.yaml]\nenvironment: {VALUE: expanded}",
        );
        let loaded = f.load().unwrap();
        assert_eq!(loaded.config.exit_mode, ExitMode::WaitAll);
        assert_eq!(loaded.config.processes["a"].command, "echo expanded");
        let overlay = f.write("project/override.yaml", "disable_env_expansion: true\nexit_mode: exit_on_failure\nprocesses: {a: {description: overlay}}");
        let loaded = load_project(&[root, overlay], &[], false).unwrap();
        assert_eq!(loaded.config.exit_mode, ExitMode::ExitOnFailure);
        assert_eq!(loaded.config.processes["a"].command, "echo ${VALUE}");
        // Include discovery still expands anchors when content expansion is off.
        f.write(
            "project/root.yaml",
            "disable_env_expansion: true\ninclude: ['${DECOMPOSE_FILE_DIR}/../fragments/a.yaml']",
        );
        f.load().unwrap();
    }

    #[test]
    fn final_command_errors_include_process_and_source() {
        let f = Fixture::new();
        f.write(
            "fragments/a.yaml",
            "processes: {api: {description: partial}}",
        );
        f.write("project/root.yaml", "include: [../fragments/a.yaml]");
        let error = f.error();
        assert!(error.contains("api") && error.contains("a.yaml") && error.contains("command"));
        f.write("project/root.yaml", "include: [../fragments/a.yaml]\nprocesses: {api: {command: '${DECOMPOSE_TEST_UNSET:-}'}}");
        assert!(f.error().contains("empty command"));
    }
}
