//! Collects validation errors with the path of the field each one is about.

use crate::manifest::error::{ManifestError, ValidationError};

/// Walks a manifest, tracking the current path, and records every broken rule.
#[derive(Debug, Default)]
pub(crate) struct Validator {
    path: Vec<String>,
    errors: Vec<ValidationError>,
}

impl Validator {
    /// Run `f` with `segment` appended to the current path.
    pub(crate) fn at(&mut self, segment: impl Into<String>, f: impl FnOnce(&mut Self)) {
        self.path.push(segment.into());
        f(self);
        self.path.pop();
    }

    /// Record an error at the current path.
    pub(crate) fn error(&mut self, message: impl Into<String>) {
        let path = if self.path.is_empty() {
            ".".to_string()
        } else {
            self.path.join(".")
        };
        self.errors.push(ValidationError {
            path,
            message: message.into(),
        });
    }

    /// Record an error at `segment` below the current path.
    pub(crate) fn error_at(&mut self, segment: impl Into<String>, message: impl Into<String>) {
        let message = message.into();
        self.at(segment, |v| v.error(message));
    }

    /// Check that `name` is a valid identifier; record an error if not.
    pub(crate) fn name(&mut self, segment: impl Into<String>, name: &str) {
        if !is_valid_name(name) {
            self.error_at(
                segment,
                format!(
                    "'{name}' is not a valid name (lowercase letters, digits, '-' and '_'; \
                     starts with a letter or digit; at most {MAX_NAME_LEN} characters)"
                ),
            );
        }
    }

    /// Check that each name in `names` is valid and appears once.
    pub(crate) fn unique_names<'a>(
        &mut self,
        segment: &str,
        names: impl IntoIterator<Item = &'a str>,
    ) {
        let mut seen = std::collections::BTreeSet::new();
        for (i, name) in names.into_iter().enumerate() {
            let at = format!("{segment}[{i}]");
            self.name(at.clone(), name);
            if !seen.insert(name) {
                self.error_at(at, format!("'{name}' is listed more than once"));
            }
        }
    }

    /// Finish: `Ok` if nothing was recorded.
    pub(crate) fn finish(self) -> Result<(), ManifestError> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(ManifestError::Invalid(self.errors))
        }
    }
}

/// Longest allowed app, bucket, volume, child or registered name.
pub const MAX_NAME_LEN: usize = 64;

/// Whether `name` is a valid identifier for an app, registered name, bucket,
/// volume or child: 1 to [`MAX_NAME_LEN`] characters of lowercase ASCII
/// letters, digits, `-` and `_`, starting with a letter or digit.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= MAX_NAME_LEN
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod validate_tests;
