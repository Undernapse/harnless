//! Non-Turing-complete substitution: `${env:NAME}` and `${home}/path`.
//!
//! The reference harness this model descends from interpolated its config
//! with arbitrary JavaScript. That is a configuration system with a scripting
//! engine bolted on: a dumped config no longer tells you what the running
//! system was configured with (the expression's *result* depends on control
//! flow you cannot see), a typo becomes a runtime crash in a boot path, and
//! reading a config file requires executing it. This crate deliberately
//! replaces that with **two** expansions and nothing else:
//!
//! * `${env:NAME}` — the value of environment variable `NAME`, or a typed
//!   error naming the expression when it is unset;
//! * `${home}` — the home directory (or an explicit override), so
//!   `${home}/.config/harnless/bundle.yml` expands without a shell.
//!
//! There is no arithmetic, no conditionals, no function calls, no command
//! substitution, and no way to compute one value from another. Two
//! consequences follow, and they are the point:
//!
//! 1. **Expansion is a pure, total function of (document, environment,
//!    home).** `--dump-config` can therefore print the *expanded* document and
//!    still equal what boot mounts — dump-equals-mount survives substitution
//!    instead of quietly breaking under it.
//! 2. **Every failure is a typed, located error.** An unknown function names
//!    the offending expression (`unknown-substitution`) rather than evaluating
//!    to `undefined` and surfacing three layers away.
//!
//! Expansion walks every string scalar in the document (mapping values,
//! sequence items, and mapping keys), so substitution works inside a plugin's
//! opaque `config` exactly as it does in a row's `plugin` name.

use std::collections::BTreeMap;

use serde_yaml::Value;

use crate::error::{ConfigError, Result, Stage};

/// The environment and home a document expands against.
///
/// `home` is injectable so tests — and a `--home` style flag — control the
/// expansion without touching the real filesystem.
#[derive(Debug, Clone, Default)]
pub struct Subst {
    /// Environment variables, keyed by name.
    pub env: BTreeMap<String, String>,
    /// The directory `${home}` expands to.
    pub home: Option<String>,
}

impl Subst {
    /// An empty substitution context (no variables, no home).
    pub fn new() -> Self {
        Self::default()
    }

    /// A context reading the process environment with `home` as the home
    /// directory.
    pub fn from_env(home: Option<String>) -> Self {
        Self {
            env: std::env::vars().collect(),
            home,
        }
    }

    /// Add one variable.
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Set the home directory.
    pub fn with_home(mut self, home: impl Into<String>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// Expand one string.
    ///
    /// A string with no `${` is returned untouched, so the common case costs
    /// one scan and no allocation.
    pub fn expand_str(&self, input: &str) -> Result<String> {
        if !input.contains("${") {
            return Ok(input.to_string());
        }
        let mut out = String::with_capacity(input.len());
        let rest = input;
        let mut cursor = 0usize;
        let bytes = rest.as_bytes();
        while cursor < bytes.len() {
            let open = match rest[cursor..].find("${") {
                Some(rel) => cursor + rel,
                None => {
                    out.push_str(&rest[cursor..]);
                    return Ok(out);
                }
            };
            out.push_str(&rest[cursor..open]);
            let close = match rest[open..].find('}') {
                Some(rel) => open + rel,
                None => {
                    return Err(ConfigError::new(
                        Stage::Substitute,
                        "bad-substitution",
                        format!("unterminated substitution expression {input:?}"),
                    ))
                }
            };
            let expr = &rest[open + 2..close];
            out.push_str(&self.expand_expr(expr, input)?);
            cursor = close + 1;
        }
        Ok(out)
    }

    /// Expand one `${...}` body (without the braces).
    fn expand_expr(&self, expr: &str, whole: &str) -> Result<String> {
        // `${home}` is a bare name (no `:`); everything else is `fn:arg`.
        let (func, arg) = match expr.split_once(':') {
            Some((func, arg)) => (func, Some(arg)),
            None => (expr, None),
        };
        match (func, arg) {
            ("env", Some(name)) => {
                if name.is_empty() {
                    return Err(unknown(whole, "env requires a variable name"));
                }
                match self.env.get(name) {
                    Some(value) => Ok(value.clone()),
                    None => Err(ConfigError::new(
                        Stage::Substitute,
                        "missing-env",
                        format!(
                            "environment variable {name:?} is not set (in expression {whole:?})"
                        ),
                    )),
                }
            }
            ("home", arg) => {
                // `${home}` alone is the directory; a suffix must be a path.
                let suffix = arg.unwrap_or("");
                if !suffix.is_empty() && !suffix.starts_with('/') {
                    return Err(unknown(
                        whole,
                        "${home} takes no argument; use ${home}/path",
                    ));
                }
                match &self.home {
                    Some(home) => Ok(format!("{home}{suffix}")),
                    None => Err(ConfigError::new(
                        Stage::Substitute,
                        "missing-home",
                        format!("home directory is unknown (in expression {whole:?})"),
                    )),
                }
            }
            (other, _) => Err(unknown(
                whole,
                format!("unknown substitution function {other:?}"),
            )),
        }
    }

    /// Expand every string scalar in `value`.
    pub fn expand_value(&self, value: &Value) -> Result<Value> {
        match value {
            Value::String(s) => Ok(Value::String(self.expand_str(s)?)),
            Value::Sequence(items) => items
                .iter()
                .map(|item| self.expand_value(item))
                .collect::<Result<Vec<_>>>()
                .map(Value::Sequence),
            Value::Mapping(map) => {
                let mut out = serde_yaml::Mapping::new();
                for (key, val) in map {
                    let original = match key {
                        Value::String(s) => Some(s.clone()),
                        other => {
                            let expanded = other.clone();
                            if out.insert(expanded.clone(), self.expand_value(val)?).is_some() {
                                let label = format!("{other:?}");
                                return Err(collision(&label, &label));
                            }
                            continue;
                        }
                    };
                    let key = Value::String(self.expand_str(original.as_ref().unwrap())?);
                    if out.insert(key.clone(), self.expand_value(val)?).is_some() {
                        return Err(collision(original.as_ref().unwrap(), &format!("{key:?}")));
                    }
                }
                Ok(Value::Mapping(out))
            }
            other => Ok(other.clone()),
        }
    }
}

/// An `unknown-substitution` failure naming the expression and the reason.
fn unknown(whole: &str, reason: impl std::fmt::Display) -> ConfigError {
    ConfigError::new(
        Stage::Substitute,
        "unknown-substitution",
        format!("unsupported expression {whole:?}: {reason}"),
    )
}

/// A `substitution-key-collision` failure: expansion made two distinct
/// mapping keys identical, so one entry would silently vanish.
fn collision(original: &str, expanded: &str) -> ConfigError {
    ConfigError::new(
        Stage::Substitute,
        "substitution-key-collision",
        format!("expanding key {original:?} yields {expanded:?}, which the mapping already \
                 carries; a substituted key must not overwrite another entry"),
    )
}

/// Expand every string in `value`, failing with a typed error that names the
/// offending expression.
pub fn expand(value: &Value, subst: &Subst) -> Result<Value> {
    subst.expand_value(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subst() -> Subst {
        Subst::new().with_env("HARNESS_KEY", "sk-1").with_home("/home/u")
    }

    fn yaml(text: &str) -> Value {
        serde_yaml::from_str(text).unwrap()
    }

    #[test]
    fn env_and_home_expand_inside_plugin_config() {
        let value = yaml(
            "plugin: llm-openai\nconfig:\n  api_key: ${env:HARNESS_KEY}\n  cache: ${home}/.cache\n",
        );
        let out = expand(&value, &subst()).unwrap();
        assert_eq!(out["config"]["api_key"].as_str(), Some("sk-1"));
        assert_eq!(out["config"]["cache"].as_str(), Some("/home/u/.cache"));
    }

    #[test]
    fn interpolation_within_a_string_works() {
        assert_eq!(
            subst().expand_str("${env:HARNESS_KEY}@${home}").unwrap(),
            "sk-1@/home/u"
        );
    }

    #[test]
    fn strings_without_expressions_are_untouched() {
        assert_eq!(subst().expand_str("plain $not ${x").is_err(), true);
        assert_eq!(subst().expand_str("plain").unwrap(), "plain");
    }

    #[test]
    fn unknown_function_is_a_typed_error_naming_the_expression() {
        let err = subst().expand_str("${eval:1+1}").unwrap_err();
        assert_eq!(err.code, "unknown-substitution");
        assert_eq!(err.stage, Stage::Substitute);
        assert!(err.message.contains("${eval:1+1}"), "{}", err.message);
    }

    #[test]
    fn unset_env_var_is_a_typed_error() {
        let err = subst().expand_str("${env:NOPE}").unwrap_err();
        assert_eq!(err.code, "missing-env");
        assert!(err.message.contains("NOPE"));
    }

    #[test]
    fn unterminated_expression_is_a_typed_error() {
        let err = subst().expand_str("${env:A").unwrap_err();
        assert_eq!(err.code, "bad-substitution");
    }

    #[test]
    fn expansion_is_deterministic_so_dumps_stay_stable() {
        let value = yaml("a: ${env:HARNESS_KEY}\nb: ${home}/x\n");
        let once = expand(&value, &subst()).unwrap();
        let twice = expand(&value, &subst()).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn a_colliding_substituted_key_is_a_typed_error() {
        // Expansion is the security-relevant pass; losing an entry to a
        // silent key collision is the one composition failure mode that
        // must be typed, not swallowed by Mapping::insert.
        let s = Subst::new().with_env("A", "fixed");
        let value = yaml("${env:A}: one\nfixed: two\n");
        let err = expand(&value, &s).unwrap_err();
        assert_eq!(err.code, "substitution-key-collision");
        assert_eq!(err.stage, Stage::Substitute);
    }
}
