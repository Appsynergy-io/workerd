use super::ResidentMemory;
use super::parse_vm_rss;

#[test]
fn vm_rss_is_read_in_bytes() {
    let status = "Name:\tworkerd\nVmPeak:\t  999 kB\nVmRSS:\t   1234 kB\nThreads:\t4\n";
    assert_eq!(parse_vm_rss(status), Some(1234 * 1024));
}

#[test]
fn a_status_without_vm_rss_has_no_figure() {
    assert_eq!(parse_vm_rss("Name:\tworkerd\n"), None);
    assert_eq!(parse_vm_rss("VmRSS:\tlots\n"), None);
}

#[cfg(target_os = "linux")]
#[test]
fn this_process_has_a_resident_figure_by_path_and_by_held_descriptor() {
    let by_path = ResidentMemory::new(None).bytes().unwrap();
    assert!(by_path > 0);
    let held = std::fs::File::open("/proc/self/status").unwrap();
    assert!(ResidentMemory::new(Some(held)).bytes().unwrap() > 0);
}
