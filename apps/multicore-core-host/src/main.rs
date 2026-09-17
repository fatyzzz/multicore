#[cfg(windows)]
mod runtime_host;
#[cfg(windows)]
mod runtime_paths;
#[cfg(windows)]
mod windows_pipe;

#[cfg(windows)]
fn main() {
    if windows_pipe::run().is_err() {
        eprintln!("MultiCore core host: privileged broker stopped safely.");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("MultiCore core host is available only on Windows.");
    std::process::exit(1);
}
