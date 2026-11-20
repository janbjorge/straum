//! The tests of `memory.rs`: resident memory read from a status text and
//! from a file the test writes, the check against the limit, and the
//! watch that ends when memory passes it.

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use super::*;

/// The lines of a `/proc/self/status` around `VmRSS`, as Linux 6 writes
/// them.
const STATUS_TEXT: &str = "\
Name:\tstraum
Umask:\t0022
State:\tS (sleeping)
Tgid:\t1
Pid:\t1
VmPeak:\t  123456 kB
VmSize:\t  120000 kB
VmHWM:\t   40000 kB
VmRSS:\t   38912 kB
RssAnon:\t   30000 kB
RssFile:\t    8912 kB
Threads:\t4
";

const MEBIBYTE: u64 = 1 << 20;

/// A file of this test's own in the temporary directory.
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("straum-memory-{}-{name}", std::process::id()))
}

/// Writes a status text holding `kib` kB resident at `path`, whole: a
/// watch reading it meanwhile sees the old text or the new one.
fn write_status(path: &PathBuf, kib: u64) {
    let written = path.with_extension("new");
    fs::write(&written, format!("Name:\tstraum\nVmRSS:\t {kib} kB\n")).unwrap();
    fs::rename(&written, path).unwrap();
}

#[test]
fn reads_resident_memory_from_a_status_text() {
    assert_eq!(resident_bytes(STATUS_TEXT).unwrap(), 38_912 * BYTES_PER_KIB);
    assert_eq!(resident_bytes("VmRSS:\t0 kB\n").unwrap(), 0);
}

#[test]
fn refuses_a_status_text_without_a_size() {
    let missing = resident_bytes("Name:\tstraum\nRssAnon:\t30000 kB\n").unwrap_err();
    assert!(matches!(missing, ReadError::NoResident), "{missing:?}");
    assert_eq!(missing.to_string(), "it has no VmRSS line");
    for line in [
        "VmRSS:",
        "VmRSS:\t lots kB",
        "VmRSS:\t 38912 MB",
        "VmRSS:\t 38912",
        "VmRSS:\t -1 kB",
        "VmRSS:\t 18446744073709551615 kB",
    ] {
        let error = resident_bytes(&format!("Name:\tstraum\n{line}\n")).unwrap_err();
        let ReadError::Malformed(said) = &error else {
            panic!("{line}: {error:?}");
        };
        assert_eq!(said, line);
        assert_eq!(error.to_string(), format!("{line:?} is not a size in kB"));
    }
}

#[test]
fn reads_resident_memory_from_a_file() {
    let path = scratch("read");
    write_status(&path, 2_048);
    let text = path.to_str().unwrap();
    assert_eq!(read_resident(text).unwrap(), 2 * MEBIBYTE);
    fs::write(&path, "Name:\tstraum\n").unwrap();
    let error = read_resident(text).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("--max-memory needs {text} to read Straum's resident memory: it has no VmRSS line")
    );
    fs::remove_file(&path).unwrap();
    let error = read_resident(text).unwrap_err();
    let MemoryError::Unreadable {
        error: ReadError::File(_),
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    let start = format!("--max-memory needs {text} to read Straum's resident memory: ");
    assert!(error.to_string().starts_with(&start), "{error}");
}

#[test]
fn stops_only_past_the_limit() {
    assert!(check(3 * MEBIBYTE, 3 * MEBIBYTE).is_ok());
    assert!(check(0, 1).is_ok());
    let stop = check(3 * MEBIBYTE + 1, 3 * MEBIBYTE).unwrap_err();
    assert!(matches!(stop, MemoryError::PastLimit { .. }), "{stop:?}");
    let stop = check(4 * MEBIBYTE, 3 * MEBIBYTE).unwrap_err();
    assert_eq!(
        stop.to_string(),
        "Straum holds 4.0 MiB of resident memory, past the --max-memory limit of 3.0 MiB"
    );
}

/// The watch reads again and again while memory stays under the limit,
/// and ends once it passes it or can't be read.
#[test]
fn the_watch_ends_once_memory_passes_the_limit() {
    let path = scratch("watch");
    write_status(&path, 1_024);
    let text = path.to_str().unwrap().to_owned();
    let watching = thread::spawn(move || watch(&text, 2 * MEBIBYTE));
    thread::sleep(Duration::from_millis(300));
    assert!(!watching.is_finished(), "the watch ended under the limit");
    write_status(&path, 3_072);
    let stop = watching.join().unwrap();
    assert_eq!(
        stop.to_string(),
        "Straum holds 3.0 MiB of resident memory, past the --max-memory limit of 2.0 MiB"
    );
    fs::remove_file(&path).unwrap();
    let gone = watch(path.to_str().unwrap(), 2 * MEBIBYTE);
    assert!(matches!(gone, MemoryError::Unreadable { .. }), "{gone:?}");
}
