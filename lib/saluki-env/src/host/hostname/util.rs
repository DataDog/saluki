#[cfg(feature = "hostname-os")]
pub fn get_os_hostname() -> Option<String> {
    match hostname::get() {
        Ok(hostname) => Some(hostname.to_string_lossy().to_string()),
        Err(e) => {
            tracing::debug!(error = %e, "Failed to query hostname.");
            None
        }
    }
}

#[cfg(all(feature = "hostname-os", target_os = "linux"))]
pub async fn is_os_hostname_trustworthy() -> bool {
    is_running_in_host_uts_namespace().await
}

#[cfg(all(feature = "hostname-os", not(target_os = "linux")))]
pub async fn is_os_hostname_trustworthy() -> bool {
    true
}

#[cfg(all(any(feature = "hostname-kubernetes", feature = "hostname-os"), target_os = "linux"))]
pub async fn is_running_in_host_uts_namespace() -> bool {
    use std::os::unix::fs::MetadataExt as _;

    // This is a fixed inode for the host UTS namespace on Linux, and has been the same thing for over 13 years, so
    // we're more or less clear to hardcode it.
    //
    // - https://github.com/torvalds/linux/blob/5859a2b1991101d6b978f3feb5325dad39421f29/include/linux/proc_ns.h#L41-L49
    const HOST_UTS_INODE: u64 = 0xEFFFFFFE;
    const PROC_SELF_UTS_NS_PATH: &str = "/proc/self/ns/uts";

    // We figure out if we're running in the host UTS namespace.
    //
    // A UTS namespace allows overriding the value that is reported when querying the OS for the hostname, which is how
    // the hostname gets overridden in a Kubernetes pod or container. Thus, if we're querying for the pod's hostname
    // specifically, we generally want to make sure we're not in the "host" UTS namespace, which would imply that we get
    // back the hostname of the underlying host itself.
    //
    // We depend on this to know if we can reliably get the name of the pod we're running in, for the purpose of
    // querying the Kubernetes API to figure out what node our pod is running on.
    let self_uts_ns_inode = match tokio::fs::metadata(PROC_SELF_UTS_NS_PATH).await {
        Ok(metadata) => metadata.ino(),
        Err(_) => return false,
    };

    self_uts_ns_inode == HOST_UTS_INODE
}
