//! Loading, validating and writing the configuration file for plugin commands.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use remedia_server_config::edit::{self, ConfigText, Snapshot};
use remedia_server_config::{ConfigError, Env, Settings};

use super::Failure;

/// Everything a command needs to read configuration and resolve `env('NAME')` references.
pub struct Session {
    pub config_path: PathBuf,
    /// Variables from `--env-file` and `--optional-env-file`; the process environment wins.
    dotenv: HashMap<String, String>,
}

/// A configuration file that was read and validated.
pub struct Loaded {
    pub snapshot: Snapshot,
    pub text: ConfigText,
    pub settings: Settings,
    /// Variables referenced by the configuration which are not available to this command.
    pub unresolved: Vec<String>,
}

impl Loaded {
    /// Say which `env('NAME')` references could not be resolved here, so a reader knows those
    /// settings were checked for shape only.
    pub fn note_unresolved(&self) {
        if !self.unresolved.is_empty() {
            eprintln!(
                "note: not set in this environment, so only validated structurally: {}",
                self.unresolved.join(", ")
            );
        }
    }
}

impl Session {
    pub fn new(config: Option<PathBuf>, dotenv: HashMap<String, String>) -> Result<Self, Failure> {
        let config_path = match config {
            Some(path) => path,
            None => std::env::current_dir()
                .map_err(|error| {
                    Failure::new(format!("cannot determine the working directory: {error}"))
                })?
                .join("remedia.config.json"),
        };

        Ok(Self {
            config_path,
            dotenv,
        })
    }

    /// Process environment first, then dotenv files: the same precedence `serve` uses.
    pub fn lookup(&self, name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .or_else(|| self.dotenv.get(name).cloned())
    }

    /// Read and validate the configuration. Never creates one.
    pub fn load(&self) -> Result<Loaded, Failure> {
        if !self.config_path.exists() {
            return Err(Failure::new(format!(
                "configuration file {} does not exist. Create one with: remedia-server --generate-config-file {}",
                self.config_path.display(),
                self.config_path.display()
            )));
        }

        let snapshot =
            edit::read(&self.config_path).map_err(|error| Failure::new(error.to_string()))?;
        let text = ConfigText::new(snapshot.contents()).map_err(|error| {
            Failure::new(format!(
                "{} is not a valid configuration: {error}",
                self.config_path.display()
            ))
        })?;
        let (settings, unresolved) = self.validate(text.as_str()).map_err(|error| {
            Failure::new(format!(
                "{} is not a valid configuration: {}",
                self.config_path.display(),
                describe(&error)
            ))
        })?;

        Ok(Loaded {
            snapshot,
            text,
            settings,
            unresolved,
        })
    }

    /// Validate a complete document with the same code the server uses at startup. Missing
    /// environment variables are tolerated, because unrelated secrets are not this command's
    /// business; every structural rule still applies.
    pub fn validate(&self, document: &str) -> Result<(Settings, Vec<String>), ConfigError> {
        let base = self
            .config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut env = Env::lenient(|name| self.lookup(name));
        let settings = remedia_server_config::parse(document, base, &mut env)?;

        Ok((settings, env.unresolved()))
    }

    /// Validate `document` and write it. Nothing is written when validation fails.
    pub fn commit(&self, loaded: &Loaded, document: &ConfigText) -> Result<(), Failure> {
        self.validate(document.as_str()).map_err(|error| {
            Failure::new(format!(
                "the resulting configuration would be invalid ({}); nothing was written",
                describe(&error)
            ))
        })?;
        edit::write_atomic(&loaded.snapshot, document.as_str())
            .map_err(|error| Failure::new(error.to_string()))
    }
}

/// A configuration error for display. Messages never contain resolved secrets.
pub fn describe(error: &ConfigError) -> String {
    match error {
        ConfigError::MissingEnvironment { field, variable } => {
            format!("{field} references environment variable {variable}, which is not set")
        }
        other => other.to_string(),
    }
}
