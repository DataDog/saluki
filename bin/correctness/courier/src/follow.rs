use std::{io, path::Path, time::Duration};

use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::sleep,
};

use crate::{Report, UsageError, FOLLOW_STARTED};

pub(crate) fn validate_path(path: &str) -> Result<(), UsageError> {
    // Accept drive-rooted paths when rendering Windows invocations from another host.
    let bytes = path.as_bytes();
    let drive_rooted = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":\\";
    if path.contains('\0') || !(Path::new(path).is_absolute() || drive_rooted) {
        return Err(UsageError {
            reason: format!("--path must be absolute, got {path:?}"),
        });
    }
    Ok(())
}

pub(crate) async fn follow(path: &str, stdout: &mut (impl AsyncWrite + Unpin)) -> Report {
    if let Err(error) = validate_path(path) {
        return Report::Usage {
            reason: error.to_string(),
        };
    }
    let result: io::Result<()> = async {
        stdout.write_all(FOLLOW_STARTED.as_bytes()).await?;
        stdout.flush().await?;
        let mut file = loop {
            match File::open(path).await {
                Ok(file) => break file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => sleep(Duration::from_millis(100)).await,
                Err(error) => return Err(error),
            }
        };
        let mut offset = 0;
        let mut buffer = vec![0; 64 * 1024];
        loop {
            if file.metadata().await?.len() < offset {
                return Err(io::Error::other(format!(
                    "file shrank below the copied offset {offset}"
                )));
            }
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                sleep(Duration::from_millis(100)).await;
            } else {
                stdout.write_all(&buffer[..read]).await?;
                stdout.flush().await?;
                offset += read as u64;
            }
        }
    }
    .await;
    Report::Failed {
        reason: format!("following {path:?}: {}", result.unwrap_err()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{execute, Command, Invocation};

    #[tokio::test]
    async fn follow_announces_waits_for_a_missing_file_and_copies_appends_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output");
        let invocation = Invocation {
            command: Command::Follow {
                path: path.to_str().unwrap().into(),
            },
            timeout: None,
        };
        let (mut reader, writer) = tokio::io::duplex(64);
        let follower = tokio::spawn(async move { execute(&invocation, tokio::io::empty(), writer).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut announcement = vec![0; FOLLOW_STARTED.len()];
            reader.read_exact(&mut announcement).await.unwrap();
            assert_eq!(announcement, FOLLOW_STARTED.as_bytes());
            assert!(!path.exists());
            let mut file = File::create(&path).await.unwrap();
            for bytes in [b"first\0\xff\r\n".as_slice(), b"second\nlast"] {
                file.write_all(bytes).await.unwrap();
                file.flush().await.unwrap();
                let mut copied = vec![0; bytes.len()];
                reader.read_exact(&mut copied).await.unwrap();
                assert_eq!(copied, bytes);
            }
            assert!(!follower.is_finished());
        })
        .await
        .unwrap();
        follower.abort();
        assert!(follower.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn follow_starts_at_zero_and_fails_when_the_file_shrinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output");
        tokio::fs::write(&path, b"original").await.unwrap();
        let invocation = Invocation {
            command: Command::Follow {
                path: path.to_str().unwrap().into(),
            },
            timeout: None,
        };
        let (mut reader, writer) = tokio::io::duplex(64);
        tokio::time::timeout(Duration::from_secs(5), async {
            let truncate = async {
                let mut bytes = vec![0; FOLLOW_STARTED.len() + 8];
                reader.read_exact(&mut bytes).await.unwrap();
                assert_eq!(bytes, [FOLLOW_STARTED.as_bytes(), b"original"].concat());
                File::options()
                    .write(true)
                    .open(&path)
                    .await
                    .unwrap()
                    .set_len(1)
                    .await
                    .unwrap();
            };
            let ((), report) = tokio::join!(truncate, execute(&invocation, tokio::io::empty(), writer));
            assert!(matches!(report, Report::Failed { reason } if reason.contains("shrank below the copied offset 8")));
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn follow_reports_unreadable_files_and_broken_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let invocation = Invocation {
            command: Command::Follow {
                path: dir.path().to_str().unwrap().into(),
            },
            timeout: None,
        };
        let mut stdout = Vec::new();
        let report = execute(&invocation, tokio::io::empty(), &mut stdout).await;
        assert!(matches!(report, Report::Failed { .. }));
        assert_eq!(stdout, FOLLOW_STARTED.as_bytes());
        let (reader, writer) = tokio::io::duplex(64);
        drop(reader);
        assert!(matches!(
            execute(&invocation, tokio::io::empty(), writer).await,
            Report::Failed { .. }
        ));
    }
}
