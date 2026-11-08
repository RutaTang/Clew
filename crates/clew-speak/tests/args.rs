//! The program as it is started: its arguments as the system gives them.

/// A model folder whose name is not Unicode — on Linux, a name is any bytes
/// — is read as named: the program says the model is not there, as for any
/// folder without it, rather than ending on the name.
#[cfg(unix)]
#[test]
fn a_folder_named_in_any_bytes_is_read() {
    use std::os::unix::ffi::OsStrExt;
    let dir = std::ffi::OsStr::from_bytes(b"/nonexistent/caf\xe9");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_clew-speak"))
        .arg("embed")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("could not be loaded"), "{said}");
}
