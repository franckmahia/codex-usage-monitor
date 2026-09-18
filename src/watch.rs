use std::path::Path;
use std::time::Duration;

use crate::import as imp;
use crate::pricing::PricingEngine;

/// Surveille sessions/ et archived_sessions/ et relance un import incremental
/// apres une periode de quietude. Totaux mis a jour affiches a chaque lot.
pub fn run_watch(
    home: &Path,
    db_path: &Path,
    engine: &PricingEngine,
    quiet_ms: u64,
) -> Result<(), String> {
    use notify::{RecommendedWatcher, RecursiveMode, Watcher};
    use std::sync::mpsc::RecvTimeoutError;

    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(tx)
        .map_err(|e| format!("watcher init: {e}"))?;

    let watch_dir = |w: &mut RecommendedWatcher, p: &Path| -> Result<(), String> {
        if p.exists() {
            w.watch(p, RecursiveMode::Recursive)
                .map_err(|e| format!("watch {}: {e}", p.display()))?;
        }
        Ok(())
    };
    watch_dir(&mut watcher, &home.join("sessions"))?;
    watch_dir(&mut watcher, &home.join("archived_sessions"))?;

    let quiet = Duration::from_millis(quiet_ms.max(200));
    println!(
        "Surveillance de {} (Ctrl-C pour arreter)",
        home.display()
    );

    loop {
        match rx.recv_timeout(quiet) {
            Ok(_event) => {
                // Periode de quietude : on avale les evenements suivants.
                while rx.recv_timeout(quiet).is_ok() {}
                match imp::import(home, db_path, engine, false, false) {
                    Ok(rep) => {
                        if rep.calls_inserted > 0 {
                            println!(
                                "[{}] +{} appels ({} fichiers modifiés, {} ms)",
                                crate::util::iso_utc(crate::util::now_unix()),
                                rep.calls_inserted,
                                rep.files_changed,
                                rep.duration_ms
                            );
                        }
                    }
                    Err(e) => eprintln!("import error: {e}"),
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}
