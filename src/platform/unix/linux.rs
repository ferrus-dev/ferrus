pub(super) fn set_serve_process_name() {
    unsafe {
        let name = b"ferrus-mcp\0";
        let _ = libc::prctl(libc::PR_SET_NAME, name.as_ptr() as libc::c_ulong, 0, 0, 0);
    }
}

pub(super) fn install_serve_parent_death_signal() {
    unsafe {
        let _ = libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGTERM as libc::c_ulong,
            0,
            0,
            0,
        );
    }
}

pub(super) fn install_headless_child_lifecycle_hook() -> std::io::Result<()> {
    unsafe {
        if libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGTERM as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(())
}
