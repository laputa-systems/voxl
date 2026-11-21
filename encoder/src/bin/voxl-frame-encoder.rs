#[cfg(unix)]
fn main() {
    use std::io;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};

    fn bind(path: &Path) -> io::Result<UnixListener> {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_socket() => std::fs::remove_file(path)?,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    let Some(arg) = std::env::args_os().nth(1) else {
        eprintln!("usage: voxl-frame-encoder <socket-path>");
        std::process::exit(2);
    };
    let path = PathBuf::from(arg);
    let listener = match bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("voxl-frame-encoder: {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let result = voxl_encoder::serve::run(listener);
    let _ = std::fs::remove_file(&path);
    if let Err(e) = result {
        eprintln!("voxl-frame-encoder: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("voxl-frame-encoder is Unix-only");
    std::process::exit(2);
}
