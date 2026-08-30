use std::env;

use serde::Deserialize;

use super::AnyError;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PostgresConfig {
    url_env: String,
    schema: String,
    max_connections: usize,
}

impl PostgresConfig {
    pub(super) fn validate(&self) -> Result<(), AnyError> {
        if self.url_env.trim().is_empty() {
            return Err("PostgreSQL URL environment name must not be empty".into());
        }
        if !valid_schema(&self.schema) {
            return Err("PostgreSQL schema must be a canonical application identifier".into());
        }
        if self.max_connections == 0 {
            return Err("PostgreSQL maximum connections must be positive".into());
        }
        Ok(())
    }

    pub(crate) fn url(&self) -> Result<String, AnyError> {
        self.read_url(|name| env::var(name))
    }

    pub(crate) fn schema(&self) -> &str {
        &self.schema
    }

    pub(crate) const fn max_connections(&self) -> usize {
        self.max_connections
    }

    fn read_url(
        &self,
        lookup: impl FnOnce(&str) -> Result<String, env::VarError>,
    ) -> Result<String, AnyError> {
        lookup(&self.url_env)
            .map_err(|_| "configured PostgreSQL URL environment variable is unavailable".into())
    }
}

fn valid_schema(schema: &str) -> bool {
    let bytes = schema.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .skip(1)
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
        && !schema.starts_with("pg_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_only_the_named_environment_without_disclosing_credentials() {
        let config = PostgresConfig {
            url_env: "PAYMENT_DATABASE_URL".to_owned(),
            schema: "payment".to_owned(),
            max_connections: 8,
        };
        let url = "postgres://user:secret@database.invalid/payment";
        assert_eq!(
            config
                .read_url(|name| {
                    assert_eq!(name, "PAYMENT_DATABASE_URL");
                    Ok(url.to_owned())
                })
                .expect("configured environment"),
            url
        );
        let error = config
            .read_url(|_| Err(env::VarError::NotPresent))
            .expect_err("missing environment");
        assert!(!error.to_string().contains("secret"));
        assert!(!error.to_string().contains("postgres://"));
    }
}
