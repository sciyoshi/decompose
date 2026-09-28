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
}

/// Keeping supplied fields avoids confusing `false`, `1`, or `[]` with
/// omission. The resolved schema remains the authority for field types.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(transparent)]
pub(super) struct RawProcess(pub BTreeMap<String, Value>);

impl RawProcess {
    fn merge(&mut self, overlay: Self) -> Result<()> {
        for (key, value) in overlay.0 {
            if key == "environment" {
                let mut env = self.environment()?;
                env.0.extend(serde_yaml_ng::from_value::<EnvVars>(value)?.0);
                self.0.insert(key, serde_yaml_ng::to_value(env)?);
            } else if key == "depends_on" {
                let mut deps = self.dependencies()?;
                deps.extend(serde_yaml_ng::from_value::<
                    BTreeMap<String, ProcessDependency>,
                >(value)?);
                self.0.insert(key, serde_yaml_ng::to_value(deps)?);
            } else {
                self.0.insert(key, value);
            }
        }
        Ok(())
    }

    pub fn environment(&self) -> Result<EnvVars> {
        self.0
            .get("environment")
            .cloned()
            .map(serde_yaml_ng::from_value)
            .transpose()
            .map(|v| v.unwrap_or_default())
            .map_err(Into::into)
    }

    pub fn dependencies(&self) -> Result<BTreeMap<String, ProcessDependency>> {
        self.0
            .get("depends_on")
            .cloned()
            .map(serde_yaml_ng::from_value)
            .transpose()
            .map(|v| v.unwrap_or_default())
            .map_err(Into::into)
    }

    pub fn resolve(self) -> Result<ProcessConfig> {
        Ok(serde_yaml_ng::from_value(serde_yaml_ng::to_value(self.0)?)?)
    }
}

impl RawProject {
    pub fn read(path: &Path) -> Result<Self> {
        let data = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let raw: Self = serde_yaml_ng::from_str(&data)
            .with_context(|| format!("config error: {}", path.display()))?;
        // Check supplied field types now, but defer completeness and graph
        // validation until every overlay has been applied.
        for (name, process) in &raw.processes {
            let mut check = process.clone();
            check
                .0
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
                let process = raw.resolve().with_context(|| format!("process `{name}`"))?;
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
