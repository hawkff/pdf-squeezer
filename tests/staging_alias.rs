//! Staging next to a directory whose name is not UTF-8. One test per file keeps
//! the TMPDIR change contained.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;

#[test]
fn staging_alias_avoids_non_utf8_temp_directory() {
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join(OsStr::from_bytes(b"M\xe4rz"));
    if fs::create_dir(&dir).is_err() {
        eprintln!("skipping: this filesystem rejects names that are not UTF-8");
        return;
    }
    // An alias in a temporary directory that is not UTF-8 would be no better.
    // SAFETY: this test binary runs a single test.
    unsafe { std::env::set_var("TMPDIR", &dir) };
    let staging = pdf_squeezer::files::Staging::create(&dir.join("out.pdf")).unwrap();
    assert!(std::str::from_utf8(staging.path.as_os_str().as_bytes()).is_ok(), "staging path {:?} is not valid UTF-8", staging.path);
    fs::write(staging.path.join("stage.pdf"), b"%PDF-1.4").unwrap();
    let alias_parent = staging.path.parent().unwrap().to_path_buf();
    staging.remove();
    let left: Vec<_> =
        fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().as_bytes().starts_with(b".pdf-squeezer-")).collect();
    assert!(left.is_empty(), "staging directories remain");
    assert!(fs::symlink_metadata(&alias_parent).is_err(), "alias remains");
}
