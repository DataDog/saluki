//! Reads and writes pod storage through the verified Docker connection to the kind node.

use airlock::{docker::Docker, exec::ExecRequest};
use saluki_error::{generic_error, GenericError};

use crate::handle::{FileKind, FileStatus};

#[derive(Clone)]
pub(crate) struct Node {
    pub(crate) docker: Docker,
    pub(crate) container: String,
}

impl Node {
    pub(crate) async fn shell(
        &self, script: &str, args: &[&str], stdin: Option<Vec<u8>>,
    ) -> Result<airlock::exec::ExecOutput, GenericError> {
        let mut argv = vec!["sh".into(), "-eu".into(), "-c".into(), script.into(), "sh".into()];
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
        let mut request = ExecRequest::new(argv);
        if let Some(stdin) = stdin {
            request = request.with_stdin(stdin);
        }
        request.execute(&self.docker, &self.container).await
    }

    pub(crate) async fn checked_shell(
        &self, script: &str, args: &[&str], stdin: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, GenericError> {
        let output = self.shell(script, args, stdin).await?;
        if output.exit_code != 0 {
            return Err(generic_error!(
                "Node '{}' file operation exited {}: {}",
                self.container,
                output.exit_code,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(output.stdout)
    }

    pub(crate) async fn write(&self, path: &str, mode: u32, bytes: Vec<u8>) -> Result<(), GenericError> {
        self.checked_shell(
            "mkdir -p -- \"$(dirname -- \"$1\")\"; tmp=$(mktemp \"$1.XXXXXX\"); \
             trap 'rm -f -- \"$tmp\"' EXIT; cat > \"$tmp\"; chmod \"$2\" \"$tmp\"; mv -f -- \"$tmp\" \"$1\"",
            &[path, &format!("{mode:o}")],
            Some(bytes),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn read(&self, path: &str) -> Result<Option<Vec<u8>>, GenericError> {
        let output = self
            .shell(
                "if [ ! -e \"$1\" ] && [ ! -L \"$1\" ]; then exit 44; fi; [ -f \"$1\" ]; cat -- \"$1\"",
                &[path],
                None,
            )
            .await?;
        match output.exit_code {
            0 => Ok(Some(output.stdout)),
            44 => Ok(None),
            code => Err(generic_error!(
                "Cannot read node file '{path}': exit {code}: {}",
                String::from_utf8_lossy(&output.stderr)
            )),
        }
    }

    pub(crate) async fn stat(&self, path: &str) -> Result<Option<FileStatus>, GenericError> {
        let output = self
            .shell(
                "if [ ! -e \"$1\" ] && [ ! -L \"$1\" ]; then exit 44; fi; \
             stat -c '%f %s' -- \"$1\"; if [ -L \"$1\" ]; then readlink -- \"$1\"; fi",
                &[path],
                None,
            )
            .await?;
        if output.exit_code == 44 {
            return Ok(None);
        }
        if output.exit_code != 0 {
            return Err(generic_error!(
                "Cannot stat node path '{path}': {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let text = String::from_utf8(output.stdout)?;
        let (header, target) = text
            .split_once('\n')
            .ok_or_else(|| generic_error!("Invalid node stat for '{path}'"))?;
        let (mode, size) = header
            .split_once(' ')
            .ok_or_else(|| generic_error!("Invalid node stat for '{path}'"))?;
        let mode = u32::from_str_radix(mode, 16)?;
        let kind = match mode & 0o170000 {
            0o100000 => FileKind::File,
            0o040000 => FileKind::Directory,
            0o120000 => FileKind::Symlink {
                target: target.strip_suffix('\n').unwrap_or(target).into(),
            },
            0o140000 => FileKind::Socket,
            0o010000 => FileKind::NamedPipe,
            _ => FileKind::Other,
        };
        Ok(Some(FileStatus {
            kind,
            mode: mode & 0o7777,
            size: size.parse()?,
        }))
    }

    pub(crate) async fn remove_directory(&self, path: &str) -> Result<(), GenericError> {
        self.checked_shell(
            "rm -rf -- \"$1\"; rmdir -- \"$(dirname -- \"$1\")\" 2>/dev/null || :",
            &[path],
            None,
        )
        .await?;
        Ok(())
    }
}
