use std::env;
use std::path::Path;
use std::process;

mod detc;
mod detcd;
mod detctl;
mod hosts;
mod manager;
mod record;
mod varlink;

/// Dispatch to the tool that matches the name used to call the binary.
///
/// The three tools live in the same binary, that is expected to be installed
/// once and symlinked, so that a system only carries one copy of it.
fn main() {
    // `env::current_exe()` is more clear, but if the binary is a
    // softlink it will still return the real binary name
    let path = match env::args().next() {
        Some(path) => path,
        _ => "detc".to_string(),
    };

    let res = match Path::new(&path).file_name().and_then(|name| name.to_str()) {
        Some("detctl") => detctl::detctl(),
        Some("detcd") => detcd::detcd(),
        _ => detc::detc(),
    };

    if let Err(err) = res {
        eprintln!("{err}");
        process::exit(1);
    }
}
