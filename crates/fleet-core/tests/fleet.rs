use fleet_core::{Config, Fleet, Limits};

fn start(dir: &std::path::Path) -> fleet_core::Result<Fleet> {
    Fleet::start(Config { data_dir: dir.to_path_buf(), limits: Limits::default(), demo: true, check_updates: false })
}

#[tokio::test(flavor = "multi_thread")]
async fn clearing_the_cache_removes_scratch_files_but_keeps_firmware_and_backups() {
    let dir = tempfile::tempdir().unwrap();
    let f = start(dir.path()).unwrap();
    let junk = f.paths.cache.join("job-1");
    std::fs::create_dir_all(&junk).unwrap();
    std::fs::write(junk.join("system.dmg"), vec![0u8; 3_000_000]).unwrap();
    std::fs::write(f.paths.firmware.join("x.ipsw.part"), vec![0u8; 2_000_000]).unwrap();
    let keep = f.paths.backups.join("00008101-001A2B3C4D5E6F78").join("Info.plist");
    assert!(keep.exists(), "demo backup present");
    let real_ipsw = f.paths.firmware.join("iPad_Fall_2022_27.0.1_24A446_Restore.ipsw");
    assert!(real_ipsw.exists());

    let before = f.storage_report().await;
    assert!(before.restore_cache >= 3_000_000 && before.partial_downloads >= 2_000_000, "{before:?}");
    // The demo has jobs that look like running work, but none of them is a restore or download.
    let freed = f.clear_cache().unwrap();
    assert!(freed >= 5_000_000);
    assert!(!junk.exists());
    assert!(!f.paths.firmware.join("x.ipsw.part").exists());
    assert!(keep.exists() && real_ipsw.exists(), "library and backups are untouched");
    let after = f.storage_report().await;
    assert_eq!(after.cache_total(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn cache_left_by_a_crash_is_removed_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    {
        let f = start(dir.path()).unwrap();
        std::fs::create_dir_all(f.paths.cache.join("old")).unwrap();
        std::fs::write(f.paths.cache.join("old").join("system.dmg"), b"x").unwrap();
    }
    let f = start(dir.path()).unwrap();
    assert_eq!(std::fs::read_dir(&f.paths.cache).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_one_instance_per_data_folder() {
    let dir = tempfile::tempdir().unwrap();
    let _first = start(dir.path()).unwrap();
    let second = start(dir.path());
    assert!(second.is_err());
    assert!(second.err().unwrap().message.contains("already running"));
}
