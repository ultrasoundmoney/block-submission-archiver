use std::time::Duration;

use memory_stats::memory_stats;
use tracing::trace;

pub async fn report_memory_periodically() {
    let mut interval = tokio::time::interval(Duration::from_secs(3));

    loop {
        interval.tick().await;

        if let Some(usage) = memory_stats() {
            trace!(
                mem_mb = usage.physical_mem / 1_000_000,
                "current physical memory usage",
            );
        } else {
            println!("couldn't get the current memory usage");
        }
    }
}
