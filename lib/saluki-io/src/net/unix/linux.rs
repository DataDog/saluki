use std::io;

use socket2::SockRef;

pub(super) fn set_uds_passcred<'sock, S>(socket: &'sock S) -> io::Result<()>
where
    SockRef<'sock>: From<&'sock S>,
{
    let sock_ref = socket2::SockRef::from(socket);
    sock_ref.set_passcred(true)
}
