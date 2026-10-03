//! Explicit qualification-only inputs; these never enable ordinary execution.
use anyhow::{Result, ensure};
use std::{ffi::OsString, path::PathBuf};

pub const CREDENTIAL_DATABASE_FILE_ENV: &str = "ORBIT_QUALIFICATION_CREDENTIAL_DATABASE_URL_FILE";
pub const PROVIDER_OPT_IN_ENV: &str = "ORBIT_QUALIFICATION_PROVIDER_OPT_IN";

pub fn credential_database_file() -> Result<Option<PathBuf>> {
    parse_inputs(
        std::env::var_os(CREDENTIAL_DATABASE_FILE_ENV),
        std::env::var_os(PROVIDER_OPT_IN_ENV),
        std::env::var_os("ORBIT_B34_LIVE_CREDENTIAL_DATABASE_URL_FILE"),
        std::env::var_os("ORBIT_B34_LIVE_PROVIDER_OPT_IN"),
    )
}

fn parse_inputs(
    database: Option<OsString>,
    authorization: Option<OsString>,
    legacy_database: Option<OsString>,
    legacy_authorization: Option<OsString>,
) -> Result<Option<PathBuf>> {
    // Historical names are rejected, never aliases that can enable live calls.
    ensure!(
        !((legacy_database.is_some() || legacy_authorization.is_some())
            && (database.is_some() || authorization.is_some())),
        "QUALIFICATION_ENV_CONFLICT: historical and current names cannot coexist"
    );
    ensure!(
        legacy_database.is_none() && legacy_authorization.is_none(),
        "QUALIFICATION_ENV_RETIRED: use ORBIT_QUALIFICATION_* names"
    );
    if database.is_none() && authorization.is_none() {
        return Ok(None);
    }
    ensure!(
        authorization.as_deref() == Some(std::ffi::OsStr::new("I_AUTHORIZE_LIVE_PROVIDER_CALLS")),
        "live qualification requires explicit provider opt-in"
    );
    let path = PathBuf::from(
        database
            .ok_or_else(|| anyhow::anyhow!("qualification credential database file required"))?,
    );
    ensure!(
        path.is_absolute(),
        "qualification credential database file must be absolute"
    );
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renamed_inputs_preserve_explicit_authorization_and_reject_legacy_conflicts() {
        let path = Some(OsString::from("/private/database-url"));
        let authorization = Some(OsString::from("I_AUTHORIZE_LIVE_PROVIDER_CALLS"));
        assert_eq!(
            parse_inputs(path.clone(), authorization.clone(), None, None).unwrap(),
            Some(PathBuf::from("/private/database-url"))
        );
        assert_eq!(parse_inputs(None, None, None, None).unwrap(), None);
        assert!(parse_inputs(path.clone(), None, None, None).is_err());
        assert!(parse_inputs(None, authorization.clone(), None, None).is_err());
        assert!(parse_inputs(Some("relative".into()), authorization.clone(), None, None).is_err());
        assert!(parse_inputs(path.clone(), Some("wrong".into()), None, None).is_err());
        assert!(
            parse_inputs(path.clone(), authorization.clone(), path.clone(), None)
                .unwrap_err()
                .to_string()
                .contains("CONFLICT")
        );
        assert!(
            parse_inputs(None, None, path, authorization)
                .unwrap_err()
                .to_string()
                .contains("RETIRED")
        );
    }
}
