use std::ffi::OsString;
use std::path::PathBuf;

use crate::{Error, Result};

pub const USAGE: &str = "usage:\n  forge-sync github <OWNER/REPOSITORY> <ABSOLUTE_OUTPUT_DIRECTORY>\n  forge-sync gitlab <HOST> <GROUP/PROJECT> <ABSOLUTE_OUTPUT_DIRECTORY>\n  forge-sync status <ABSOLUTE_OUTPUT_DIRECTORY>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Github {
        repository: String,
        output: PathBuf,
    },
    Gitlab {
        host: String,
        project: String,
        output: PathBuf,
    },
    Status {
        output: PathBuf,
    },
}

pub fn parse<I>(args: I) -> Result<Command>
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<_> = args.into_iter().collect();
    let strings: Vec<String> = args
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| usage("arguments must be valid UTF-8"))
        })
        .collect::<Result<_>>()?;
    match strings.as_slice() {
        [command, repository, output] if command == "github" => {
            validate_project_path(repository, 2)?;
            Ok(Command::Github {
                repository: repository.clone(),
                output: absolute(output)?,
            })
        }
        [command, host, project, output] if command == "gitlab" => {
            validate_host(host)?;
            validate_project_path(project, usize::MAX)?;
            Ok(Command::Gitlab {
                host: host.clone(),
                project: project.clone(),
                output: absolute(output)?,
            })
        }
        [command, output] if command == "status" => Ok(Command::Status {
            output: absolute(output)?,
        }),
        _ => Err(usage("invalid arguments")),
    }
}

fn usage(message: &str) -> Error {
    Error::Usage(format!("{message}\n{USAGE}"))
}

fn absolute(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || value.contains('\0')
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(usage("output directory must be an absolute path"));
    }
    Ok(path)
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_project_path(value: &str, exact_segments: usize) -> Result<()> {
    if value.len() > 1024 || value.contains("://") {
        return Err(usage("malformed repository or project identifier"));
    }
    let segments: Vec<_> = value.split('/').collect();
    if (exact_segments != usize::MAX && segments.len() != exact_segments)
        || segments.len() < 2
        || !segments.iter().all(|segment| valid_segment(segment))
    {
        return Err(usage("malformed repository or project identifier"));
    }
    Ok(())
}

fn validate_host(value: &str) -> Result<()> {
    if value.len() > 253
        || value.contains("://")
        || value.contains('/')
        || value.contains(':')
        || !value.contains('.')
        || value.split('.').any(|part| {
            part.is_empty()
                || part.len() > 63
                || part.starts_with('-')
                || part.ends_with('-')
                || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err(usage("malformed GitLab host"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_s(args: &[&str]) -> Result<Command> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn accepts_exact_commands() {
        assert!(matches!(
            parse_s(&["github", "Blockstream/lwk", "/tmp/x"]),
            Ok(Command::Github { .. })
        ));
        assert!(matches!(
            parse_s(&["gitlab", "gl.example.com", "group/sub/project", "/tmp/x"]),
            Ok(Command::Gitlab { .. })
        ));
        assert!(matches!(
            parse_s(&["status", "/tmp/x"]),
            Ok(Command::Status { .. })
        ));
    }

    #[test]
    fn rejects_bad_shapes_and_values() {
        for args in [
            vec![],
            vec!["github"],
            vec!["github", "a/b", "/tmp/x", "extra"],
            vec!["github", "https://x/y", "/tmp/x"],
            vec!["github", "a/b", "relative"],
            vec!["github", "a/b/c", "/tmp/x"],
            vec!["github", "a/b", "/tmp/../x"],
            vec!["gitlab", "host:22", "a/b", "/tmp/x"],
            vec!["gitlab", "host", "a/b", "/tmp/x"],
            vec!["unknown", "/tmp/x"],
        ] {
            assert!(parse_s(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn parsing_depends_only_on_arguments() {
        std::env::set_var("GITHUB_TOKEN", "must-not-be-used");
        std::env::set_var("FORGE_SYNC_PROVIDER", "gitlab");
        let parsed = parse_s(&["github", "owner/repo", "/tmp/cache"]).unwrap();
        assert!(matches!(parsed, Command::Github { repository, .. } if repository == "owner/repo"));
        std::env::remove_var("GITHUB_TOKEN");
        std::env::remove_var("FORGE_SYNC_PROVIDER");
    }
}
