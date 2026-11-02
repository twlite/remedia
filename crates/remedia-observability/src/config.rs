use std::env;

use thiserror::Error;
use tracing_subscriber::EnvFilter;

/// Console output format selected by `REMEDIA_LOG_FORMAT`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogFormat {
    /// Human-readable structured output.
    #[default]
    Text,
    /// One JSON object per event.
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OtlpProtocol {
    Grpc,
    HttpProtobuf,
    HttpJson,
}

impl OtlpProtocol {
    fn parse(variable: &'static str, value: &str) -> Result<Self, ConfigError> {
        match value.trim() {
            "grpc" => Ok(Self::Grpc),
            "http/protobuf" => Ok(Self::HttpProtobuf),
            "http/json" => Ok(Self::HttpJson),
            _ => Err(ConfigError::InvalidValue {
                variable,
                value: value.to_owned(),
                expected: "grpc, http/protobuf, or http/json",
            }),
        }
    }
}

/// Validated configuration for Remedia's owned subscriber and optional OTLP exporters.
#[derive(Clone, Debug)]
pub struct ObservabilityConfig {
    filter: String,
    log_format: LogFormat,
    otel_enabled: bool,
    pub(crate) traces_protocol: OtlpProtocol,
    pub(crate) metrics_protocol: OtlpProtocol,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            filter: "warn".to_owned(),
            log_format: LogFormat::Text,
            otel_enabled: false,
            traces_protocol: OtlpProtocol::HttpProtobuf,
            metrics_protocol: OtlpProtocol::HttpProtobuf,
        }
    }
}

impl ObservabilityConfig {
    /// Read and validate Remedia's variables and standard OpenTelemetry protocol settings.
    ///
    /// The default console filter is `warn`; OTLP is disabled unless
    /// `REMEDIA_OTEL_ENABLED=true` is set.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_values(|name| env::var(name).ok())
    }

    /// Set component filter directives, for example
    /// `remedia_core=debug,remedia_discord=trace`.
    pub fn with_filter(mut self, directives: impl Into<String>) -> Result<Self, ConfigError> {
        let directives = directives.into();
        validate_filter(&directives)?;
        self.filter = directives;
        Ok(self)
    }

    /// Select text or JSON console output.
    pub fn with_log_format(mut self, format: LogFormat) -> Self {
        self.log_format = format;
        self
    }

    /// Enable or disable OTLP traces and metrics export.
    pub fn with_otel_enabled(mut self, enabled: bool) -> Self {
        self.otel_enabled = enabled;
        self
    }

    /// Return the parsed tracing filter directives.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Return the selected console format.
    pub const fn log_format(&self) -> LogFormat {
        self.log_format
    }

    /// Return whether this configuration creates OTLP exporters.
    pub const fn otel_enabled(&self) -> bool {
        self.otel_enabled
    }

    fn from_values(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let filter = if let Some(directives) = get("REMEDIA_LOG") {
            validate_filter(&directives)?;
            directives
        } else {
            match get("REMEDIA_DEBUG").as_deref() {
                None | Some("0") => "warn".to_owned(),
                Some("1") => "remedia_core=debug,remedia_discord=debug,remedia_observability=debug"
                    .to_owned(),
                Some(value) => {
                    return Err(ConfigError::InvalidValue {
                        variable: "REMEDIA_DEBUG",
                        value: value.to_owned(),
                        expected: "0 or 1",
                    });
                }
            }
        };

        let log_format = match get("REMEDIA_LOG_FORMAT").as_deref() {
            None | Some("text") => LogFormat::Text,
            Some("json") => LogFormat::Json,
            Some(value) => {
                return Err(ConfigError::InvalidValue {
                    variable: "REMEDIA_LOG_FORMAT",
                    value: value.to_owned(),
                    expected: "text or json",
                });
            }
        };

        let otel_enabled = match get("REMEDIA_OTEL_ENABLED").as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(value) => {
                return Err(ConfigError::InvalidValue {
                    variable: "REMEDIA_OTEL_ENABLED",
                    value: value.to_owned(),
                    expected: "true or false",
                });
            }
        };

        if otel_enabled {
            validate_sampling_configuration(&mut get)?;
        }

        let common_protocol = get("OTEL_EXPORTER_OTLP_PROTOCOL");
        let mut protocol = |signal_variable: &'static str| {
            get(signal_variable)
                .map(|value| (signal_variable, value))
                .or_else(|| {
                    common_protocol
                        .clone()
                        .map(|value| ("OTEL_EXPORTER_OTLP_PROTOCOL", value))
                })
        };
        let traces_protocol = protocol("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
            .as_ref()
            .map(|(variable, value)| OtlpProtocol::parse(variable, value))
            .transpose()?
            .unwrap_or(OtlpProtocol::HttpProtobuf);
        let metrics_protocol = protocol("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL")
            .as_ref()
            .map(|(variable, value)| OtlpProtocol::parse(variable, value))
            .transpose()?
            .unwrap_or(OtlpProtocol::HttpProtobuf);

        Ok(Self {
            filter,
            log_format,
            otel_enabled,
            traces_protocol,
            metrics_protocol,
        })
    }
}

fn validate_sampling_configuration(
    get: &mut impl FnMut(&str) -> Option<String>,
) -> Result<(), ConfigError> {
    let sampler = get("OTEL_TRACES_SAMPLER").unwrap_or_else(|| "parentbased_always_on".to_owned());
    let ratio_sampler = match sampler.as_str() {
        "always_on" | "always_off" | "parentbased_always_on" | "parentbased_always_off" => false,
        "traceidratio" | "parentbased_traceidratio" => true,
        _ => {
            return Err(ConfigError::InvalidValue {
                variable: "OTEL_TRACES_SAMPLER",
                value: sampler,
                expected: "always_on, always_off, traceidratio, parentbased_always_on, parentbased_always_off, or parentbased_traceidratio",
            });
        }
    };

    if ratio_sampler && let Some(argument) = get("OTEL_TRACES_SAMPLER_ARG") {
        let valid_ratio = argument
            .parse::<f64>()
            .is_ok_and(|ratio| ratio.is_finite() && (0.0..=1.0).contains(&ratio));
        if !valid_ratio {
            return Err(ConfigError::InvalidValue {
                variable: "OTEL_TRACES_SAMPLER_ARG",
                value: argument,
                expected: "a sampling ratio between 0.0 and 1.0",
            });
        }
    }

    Ok(())
}

fn validate_filter(directives: &str) -> Result<(), ConfigError> {
    EnvFilter::try_new(directives)
        .map(|_| ())
        .map_err(|error| ConfigError::InvalidFilter {
            value: directives.to_owned(),
            reason: error.to_string(),
        })
}

/// A configuration value failed validation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    /// A supported environment variable had a malformed or unsupported value.
    #[error("invalid value for {variable}: {value:?}; expected {expected}")]
    InvalidValue {
        variable: &'static str,
        value: String,
        expected: &'static str,
    },
    /// `REMEDIA_LOG` or an explicit filter was not a valid tracing filter.
    #[error("invalid tracing filter {value:?}: {reason}")]
    InvalidFilter { value: String, reason: String },
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, LogFormat, ObservabilityConfig, OtlpProtocol};

    fn parse(values: &[(&str, &str)]) -> Result<ObservabilityConfig, ConfigError> {
        let values: std::collections::HashMap<_, _> = values.iter().copied().collect();
        ObservabilityConfig::from_values(|name| values.get(name).map(|value| (*value).to_owned()))
    }

    #[test]
    fn defaults_and_debug_filter_are_documented() {
        let defaults = parse(&[]).unwrap();
        assert_eq!(defaults.filter(), "warn");
        assert_eq!(defaults.log_format(), LogFormat::Text);
        assert!(!defaults.otel_enabled());

        let debug = parse(&[("REMEDIA_DEBUG", "1")]).unwrap();
        assert!(debug.filter().contains("remedia_core=debug"));
        assert!(debug.filter().contains("remedia_discord=debug"));
    }

    #[test]
    fn explicit_filter_takes_precedence_over_debug() {
        let config = parse(&[
            ("REMEDIA_DEBUG", "not-a-boolean"),
            ("REMEDIA_LOG", "remedia_core=trace,remedia_discord=info"),
        ])
        .unwrap();
        assert_eq!(config.filter(), "remedia_core=trace,remedia_discord=info");
    }

    #[test]
    fn rejects_invalid_filter_format_and_boolean_values() {
        assert!(matches!(
            parse(&[("REMEDIA_LOG", "remedia_core=not-a-level")]),
            Err(ConfigError::InvalidFilter { .. })
        ));
        assert!(matches!(
            parse(&[("REMEDIA_LOG_FORMAT", "pretty")]),
            Err(ConfigError::InvalidValue {
                variable: "REMEDIA_LOG_FORMAT",
                ..
            })
        ));
        assert!(matches!(
            parse(&[("REMEDIA_OTEL_ENABLED", "yes")]),
            Err(ConfigError::InvalidValue {
                variable: "REMEDIA_OTEL_ENABLED",
                ..
            })
        ));
    }

    #[test]
    fn honors_signal_specific_otlp_protocols_before_common_protocol() {
        let config = parse(&[
            ("REMEDIA_OTEL_ENABLED", "true"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
            ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "grpc"),
        ])
        .unwrap();
        assert_eq!(config.traces_protocol, OtlpProtocol::Grpc);
        assert_eq!(config.metrics_protocol, OtlpProtocol::HttpJson);

        assert!(matches!(
            parse(&[
                ("REMEDIA_OTEL_ENABLED", "true"),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "tcp")
            ]),
            Err(ConfigError::InvalidValue {
                variable: "OTEL_EXPORTER_OTLP_PROTOCOL",
                ..
            })
        ));
    }

    #[test]
    fn validates_standard_sampler_configuration_when_export_is_enabled() {
        assert!(
            parse(&[
                ("REMEDIA_OTEL_ENABLED", "true"),
                ("OTEL_TRACES_SAMPLER", "parentbased_traceidratio"),
                ("OTEL_TRACES_SAMPLER_ARG", "0.25"),
            ])
            .is_ok()
        );
        assert!(matches!(
            parse(&[
                ("REMEDIA_OTEL_ENABLED", "true"),
                ("OTEL_TRACES_SAMPLER", "traceidratio"),
                ("OTEL_TRACES_SAMPLER_ARG", "2.5"),
            ]),
            Err(ConfigError::InvalidValue {
                variable: "OTEL_TRACES_SAMPLER_ARG",
                ..
            })
        ));
        assert!(matches!(
            parse(&[
                ("REMEDIA_OTEL_ENABLED", "true"),
                ("OTEL_TRACES_SAMPLER", "jaeger_remote"),
            ]),
            Err(ConfigError::InvalidValue {
                variable: "OTEL_TRACES_SAMPLER",
                ..
            })
        ));
    }
}
