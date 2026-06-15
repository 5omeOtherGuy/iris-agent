use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Result, bail};

pub(crate) fn transcript_path() -> Result<PathBuf> {
    transcript_path_from_env(|key| std::env::var_os(key))
}

fn iris_home_from_env(mut env: impl FnMut(&str) -> Option<OsString>) -> Result<PathBuf> {
    if let Some(path) = non_empty_os(env("IRIS_HOME")) {
        return Ok(PathBuf::from(path));
    }
    let Some(home) = non_empty_os(env("HOME")) else {
        bail!("HOME is not set; set IRIS_HOME to choose an Iris data directory");
    };
    Ok(PathBuf::from(home).join(".iris"))
}

fn transcript_path_from_env(mut env: impl FnMut(&str) -> Option<OsString>) -> Result<PathBuf> {
    if let Some(path) = non_empty_os(env("IRIS_TRANSCRIPT_PATH")) {
        return Ok(PathBuf::from(path));
    }
    let home = iris_home_from_env(env)?;
    Ok(home.join("transcripts").join("session.log"))
}

fn non_empty_os(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl FnMut(&str) -> Option<OsString> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn iris_home_prefers_override() -> Result<()> {
        assert_eq!(
            iris_home_from_env(env(&[("IRIS_HOME", "/tmp/iris")]))?,
            PathBuf::from("/tmp/iris")
        );
        Ok(())
    }

    #[test]
    fn iris_home_falls_back_to_home() -> Result<()> {
        assert_eq!(
            iris_home_from_env(env(&[("HOME", "/home/alice")]))?,
            PathBuf::from("/home/alice/.iris")
        );
        Ok(())
    }

    #[test]
    fn transcript_path_prefers_override() -> Result<()> {
        assert_eq!(
            transcript_path_from_env(env(&[("IRIS_TRANSCRIPT_PATH", "/tmp/session.log")]))?,
            PathBuf::from("/tmp/session.log")
        );
        Ok(())
    }
}
