use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use googletor_core::{run_job, Dest, JobProgress};

/// Тест из терминала: несколько источников через запятую качаются одновременно в одной папке.
#[tokio::main]
async fn main() -> Result<()> {
    let mut a = std::env::args().skip(1).peekable();
    // Проверка возобновления на тестовом сервере (CLOUTER_DRIVE_BASE): `drive <источник>` и `resume`.
    if matches!(a.peek().map(|s| s.as_str()), Some("drive") | Some("resume")) {
        let mode = a.next().unwrap();
        let progress = Arc::new(JobProgress::default());
        let auth = googletor_core::drive::DriveAuth::fake();
        let source = if mode == "drive" {
            let src = a.next().expect("источник");
            let st = googletor_core::jobstate::JobState { id: format!("test-{}", std::process::id()), source: src.clone(), provider: "fake".into(), dest_name: "fake".into(), window_gb: 1.0, ..Default::default() };
            googletor_core::jobstate::save(&st);
            *progress.state.lock().unwrap() = Some(st);
            src
        } else {
            let st = googletor_core::jobstate::load_all().into_iter().find(|s| s.provider == "fake").expect("нет сохранённой загрузки");
            println!("RESUME next_file={} session={:?} offset={} done={}", st.next_file, st.session.as_ref().map(|u| u.len()), st.offset, st.done_bytes);
            let src = st.source.clone();
            *progress.state.lock().unwrap() = Some(st);
            src
        };
        googletor_core::set_max_parallel(1);
        let p2 = progress.clone();
        tokio::spawn(async move { loop { println!("{}", p2.line()); tokio::time::sleep(Duration::from_secs(2)).await; } });
        let r = run_job(&source, Dest::Drive(auth, None), 1 << 30, progress.clone()).await;
        println!("RESULT {:?} {}", r.as_ref().map(|_| "ok").map_err(|e| e.to_string()), progress.line());
        return Ok(());
    }
    let sources = a.next().expect("usage: googletor <magnet|file.torrent>[,<magnet>…] <out-dir> [window-mb]");
    let out = PathBuf::from(a.next().expect("out dir"));
    let window_mb: u64 = a.next().map(|s| s.parse().unwrap()).unwrap_or(2048);
    googletor_core::set_max_parallel(4);
    let mut tasks = vec![];
    let mut all = vec![];
    for src in sources.split(',').map(str::to_string) {
        let p = Arc::new(JobProgress::default());
        all.push(p.clone());
        let out = out.clone();
        tasks.push(tokio::spawn(async move { run_job(&src, Dest::Local(out), window_mb << 20, p).await }));
    }
    let pr = all.clone();
    tokio::spawn(async move {
        loop {
            for p in &pr { println!("{}", p.line()); }
            println!("--");
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    });
    for t in tasks { if let Err(e) = t.await? { println!("JOB ERROR: {e:#}"); } }
    for p in &all { println!("DONE {}", p.line()); }
    Ok(())
}
