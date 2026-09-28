//! Presence-preserving configuration, before defaults and project validation.
use super::*;
use serde_yaml_ng::Value;

#[derive(Debug, Default, Deserialize, Clone)]
pub(super) struct RawProject {
    #[serde(default)]
    pub environment: EnvVars,
    #[serde(default)]
    pub processes: BTreeMap<String, RawProcess>,
    pub disable_env_expansion: Option<bool>,
    pub exit_mode: Option<ExitMode>,
    #[serde(default)]
    pub include: Vec<Include>,
    #[serde(skip)]
    pub environment_sources: BTreeMap<String, PathBuf>,
}

/// Keeping supplied fields avoids confusing `false`, `1`, or `[]` with
/// omission. The resolved schema remains the authority for field types.
#[derive(Debug, Default, Deserialize, Clone)]
pub(super) struct RawProcess {
    #[serde(flatten)]
    pub fields: BTreeMap<String, Value>,
    #[serde(skip)]
    pub sources: BTreeMap<String, PathBuf>,
    #[serde(skip)]
    pub environment_sources: BTreeMap<String, PathBuf>,
    #[serde(skip)]
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub(super) enum Include {
    Path(String),
    Detail(IncludeDetail),
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(super) struct IncludeDetail {
    pub path: String,
    pub processes: Option<Vec<String>>,
}

impl Include {
    pub fn parts(&self) -> (&str, Option<&[String]>) {
        match self {
            Self::Path(path) => (path, None),
            Self::Detail(detail) => (&detail.path, detail.processes.as_deref()),
        }
    }
}

impl RawProcess {
    fn merge(&mut self, overlay: Self) -> Result<()> {
        self.sources.extend(overlay.sources);
        self.environment_sources.extend(overlay.environment_sources);
        for file in overlay.files {
            if !self.files.contains(&file) {
                self.files.push(file);
            }
        }
        for (key, value) in overlay.fields {
            if key == "environment" {
                let mut env = self.environment()?;
                env.0.extend(serde_yaml_ng::from_value::<EnvVars>(value)?.0);
                self.fields.insert(key, serde_yaml_ng::to_value(env)?);
            } else if key == "depends_on" {
                let mut deps = self.dependencies()?;
                deps.extend(serde_yaml_ng::from_value::<
                    BTreeMap<String, ProcessDependency>,
                >(value)?);
                self.fields.insert(key, serde_yaml_ng::to_value(deps)?);
            } else {
                self.fields.insert(key, value);
            }
        }
        Ok(())
    }

    pub fn environment(&self) -> Result<EnvVars> {
        self.fields
            .get("environment")
            .cloned()
            .map(serde_yaml_ng::from_value)
            .transpose()
            .map(|v| v.unwrap_or_default())
            .map_err(Into::into)
    }

    pub fn dependencies(&self) -> Result<BTreeMap<String, ProcessDependency>> {
        self.fields
            .get("depends_on")
            .cloned()
            .map(serde_yaml_ng::from_value)
            .transpose()
            .map(|v| v.unwrap_or_default())
            .map_err(Into::into)
    }

    pub fn resolve(self) -> Result<ProcessConfig> {
        Ok(serde_yaml_ng::from_value(serde_yaml_ng::to_value(
            self.fields,
        )?)?)
    }
}

impl RawProject {
    pub fn read(path: &Path) -> Result<Self> {
        let data = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut raw: Self = serde_yaml_ng::from_str(&data)
            .with_context(|| format!("config error: {}", path.display()))?;
        // Check supplied field types now, but defer completeness and graph
        // validation until every overlay has been applied.
        raw.environment_sources = raw
            .environment
            .0
            .keys()
            .map(|key| (key.clone(), path.to_path_buf()))
            .collect();
        for (name, process) in &mut raw.processes {
            process.sources = process
                .fields
                .keys()
                .map(|key| (key.clone(), path.to_path_buf()))
                .collect();
            process.environment_sources = process
                .environment()?
                .0
                .keys()
                .map(|key| (key.clone(), path.to_path_buf()))
                .collect();
            process.files.push(path.to_path_buf());
            let mut check = process.clone();
            check
                .fields
                .entry("command".into())
                .or_insert(Value::String(String::new()));
            check
                .resolve()
                .with_context(|| format!("config error: {}: process `{name}`", path.display()))?;
        }
        Ok(raw)
    }

    pub fn merge(mut self, overlay: Self) -> Result<Self> {
        self.environment.0.extend(overlay.environment.0);
        self.environment_sources.extend(overlay.environment_sources);
        for (name, process) in overlay.processes {
            self.processes.entry(name).or_default().merge(process)?;
        }
        if overlay.disable_env_expansion.is_some() {
            self.disable_env_expansion = overlay.disable_env_expansion;
        }
        if overlay.exit_mode.is_some() {
            self.exit_mode = overlay.exit_mode;
        }
        Ok(self)
    }

    pub fn resolve(self) -> Result<ProjectConfig> {
        let processes = self
            .processes
            .into_iter()
            .map(|(name, raw)| {
                let files = raw
                    .files
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let process = raw
                    .resolve()
                    .with_context(|| format!("process `{name}` from {files}"))?;
                Ok((name, process))
            })
            .collect::<Result<_>>()?;
        Ok(ProjectConfig {
            environment: self.environment,
            processes,
            disable_env_expansion: self.disable_env_expansion.unwrap_or_default(),
            exit_mode: self.exit_mode.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_overrides_preserve_omission_and_apply_explicit_defaults() {
        let base: RawProject = serde_yaml_ng::from_str(
            r#"
exit_mode: exit_on_failure
disable_env_expansion: true
processes:
  api:
    command: serve
    disabled: true
    is_dotenv_disabled: true
    replicas: 3
    env_file: [base.env]
    environment: [A=base, B=keep]
    depends_on: {db: {}}
  db: {command: database}
"#,
        )
        .unwrap();
        let overlay: RawProject = serde_yaml_ng::from_str(
            r#"
exit_mode: wait_all
disable_env_expansion: false
processes:
  api:
    disabled: false
    is_dotenv_disabled: false
    replicas: 1
    env_file: []
    environment: {A: local}
    depends_on: {worker: {}}
  worker: {command: work}
"#,
        )
        .unwrap();
        let cfg = base.merge(overlay).unwrap().resolve().unwrap();
        validate_config(&cfg).unwrap();
        let api = &cfg.processes["api"];
        assert_eq!(api.command, "serve");
        assert!(!api.disabled && !api.is_dotenv_disabled && !cfg.disable_env_expansion);
        assert_eq!(cfg.exit_mode, ExitMode::WaitAll);
        assert_eq!(api.replicas, 1);
        assert!(api.env_file.is_empty());
        assert_eq!(api.environment.0["A"], "local");
        assert_eq!(api.environment.0["B"], "keep");
        assert_eq!(api.depends_on.len(), 2);
    }

    #[test]
    fn command_is_required_after_all_files_are_merged() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.yaml");
        let overlay = dir.path().join("overlay.yaml");
        fs::write(&base, "processes: {api: {depends_on: {db: {}}}}").unwrap();
        fs::write(
            &overlay,
            "processes: {api: {command: serve}, db: {command: database}}",
        )
        .unwrap();
        load_and_merge_configs(&[base.clone(), overlay]).unwrap();
        let error = load_and_merge_configs(&[base]).unwrap_err();
        assert!(format!("{error:#}").contains("command"));
    }
}
