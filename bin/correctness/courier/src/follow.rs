use std::{io, path::Path, time::Duration};

use snafu::ResultExt as _;
use tokio::{
    fs::File,
    io::{AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    time::{sleep_until, timeout_at, Instant},
};

use crate::{Error, OutputSnafu, TargetSnafu, TimedOutSnafu};

/// How often to look again for a file that does not exist yet, or for more of one that has no new data.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Writes the contents of the file at `path` to `out` as they are written, until `limit` passes.
///
/// Waits for the file to exist, then writes it from the start. Reaching `limit` after the file was found is the
/// normal end. A file that is truncated, renamed or replaced is not followed again.
///
/// # Errors
///
/// Fails with [`Error::TimedOut`] when the file does not appear before `limit` passes, [`Error::Target`] when it
/// cannot be opened or read, and [`Error::Output`] when writing to `out` fails.
pub async fn follow<W>(path: &Path, limit: Duration, out: &mut W) -> Result<(), Error>
where
    W: AsyncWrite + Unpin,
{
    let deadline = Instant::now() + limit;
    let context = |action| TargetSnafu {
        action,
        target: path.display().to_string(),
    };

    let mut file = loop {
        match File::open(path).await {
            Ok(file) => break file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if Instant::now() >= deadline {
                    return TimedOutSnafu {
                        operation: "follow",
                        target: path.display().to_string(),
                        limit,
                    }
                    .fail();
                }
                sleep_until((Instant::now() + POLL_INTERVAL).min(deadline)).await;
            }
            Err(e) => return Err(e).context(context("open")),
        }
    };

    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = match timeout_at(deadline, file.read(&mut buf)).await {
            Ok(read) => read.with_context(|_| context("read"))?,
            Err(_) => return Ok(()),
        };
        if n > 0 {
            let written = timeout_at(deadline, async {
                out.write_all(&buf[..n]).await?;
                out.flush().await
            })
            .await;
            match written {
                Ok(written) => written.context(OutputSnafu)?,
                Err(_) => return Ok(()),
            }
        } else if Instant::now() >= deadline {
            return Ok(());
        } else {
            sleep_until((Instant::now() + POLL_INTERVAL).min(deadline)).await;
        }
    }
}
