//! Credential references and secret-bearing sources.

use std::{fmt, sync::Arc};

use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

use super::config::DEFAULT_API_KEY_ENV;

/// Maximum environment-variable name accepted as a credential reference.
const MAX_CREDENTIAL_REF_BYTES: usize = 128;

/// Validated environment-variable name, never a literal credential.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CredentialRef(String);

impl CredentialRef {
    /// Validate one portable environment-variable identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialRefError> {
        let value = value.into();
        let mut bytes = value.bytes();
        let valid_first = bytes
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic());
        if !valid_first
            || value.len() > MAX_CREDENTIAL_REF_BYTES
            || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        {
            return Err(CredentialRefError);
        }
        Ok(Self(value))
    }

    pub(super) fn default_deepseek() -> Self {
        Self(DEFAULT_API_KEY_ENV.to_owned())
    }

    /// Borrow the non-secret environment-variable name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Invalid credential-reference syntax.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("credential reference must be a portable environment-variable name")]
pub struct CredentialRefError;

/// Owned secret text whose ordinary formatting is always redacted.
#[derive(Clone)]
pub struct SecretValue(SecretString);

impl SecretValue {
    /// Take ownership of secret text.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(SecretString::from(value.into()))
    }

    pub(super) fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

/// Result of looking up one named credential.
#[derive(Clone, Debug)]
pub enum CredentialLookup {
    /// No value exists in this source.
    Missing,
    /// A value exists and must still pass API-key validation.
    Present(SecretValue),
    /// The platform value could not be represented as Unicode.
    InvalidEncoding,
}

/// Replaceable source resolved once at the beginning of every request.
pub trait CredentialSource: Send + Sync {
    /// Resolve exactly one named secret without logging it.
    fn resolve(&self, reference: &CredentialRef) -> CredentialLookup;
}

/// Process-environment credential source used by the real CLI path.
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvironmentCredentials;

impl CredentialSource for EnvironmentCredentials {
    fn resolve(&self, reference: &CredentialRef) -> CredentialLookup {
        match std::env::var_os(reference.as_str()) {
            None => CredentialLookup::Missing,
            Some(value) if value.is_empty() => CredentialLookup::Missing,
            Some(value) => match value.into_string() {
                Ok(value) => CredentialLookup::Present(SecretValue::new(value)),
                Err(_) => CredentialLookup::InvalidEncoding,
            },
        }
    }
}

/// Explicit in-memory credential source for embedding and deterministic tests.
#[derive(Clone, Debug)]
pub struct StaticCredentials {
    reference: CredentialRef,
    secret: Arc<SecretValue>,
}

impl StaticCredentials {
    /// Associate one secret with one validated reference.
    #[must_use]
    pub fn new(reference: CredentialRef, secret: SecretValue) -> Self {
        Self {
            reference,
            secret: Arc::new(secret),
        }
    }
}

impl CredentialSource for StaticCredentials {
    fn resolve(&self, reference: &CredentialRef) -> CredentialLookup {
        if reference != &self.reference {
            return CredentialLookup::Missing;
        }
        CredentialLookup::Present((*self.secret).clone())
    }
}

/// One normalized API key, kept private to the DeepSeek boundary.
pub(super) struct ApiKey(SecretString);

impl ApiKey {
    pub(super) fn normalize(value: SecretValue) -> Result<Self, ApiKeyError> {
        let value = value.expose().trim();
        if value.is_empty() {
            return Err(ApiKeyError::Empty);
        }
        if !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
            return Err(ApiKeyError::IllegalCharacters);
        }
        Ok(Self(SecretString::from(value.to_owned())))
    }

    pub(super) fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey([REDACTED])")
    }
}

/// A supplied key cannot be used as an Authorization value.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum ApiKeyError {
    #[error("API key is empty after trimming")]
    Empty,
    #[error("API key contains characters that are unsafe in an HTTP header")]
    IllegalCharacters,
}
