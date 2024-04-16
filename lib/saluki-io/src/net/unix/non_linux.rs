use std::io;

use socket2::SockRef;

pub(super) fn set_uds_passcred<'sock, S>(_socket: &'sock S) -> io::Result<()>
where
    SockRef<'sock>: From<&'sock S>,
{
    Ok(())
}
