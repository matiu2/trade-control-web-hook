//! Keep broker session tasks running while completion waits for child commands.
use color_eyre::Result;
use tokio::runtime::Runtime;

pub(super) fn build() -> Result<Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn session_tasks_continue_while_the_completion_thread_waits_for_a_child() {
        crate::test_logging();
        let runtime = build().unwrap();
        let (finished, completion) = mpsc::channel();
        runtime.block_on(async {
            let (started, ready) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                started.send(()).unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
                finished.send(()).unwrap();
            });
            ready.await.unwrap();
        });

        // Child processes block this thread outside block_on, just like this
        // wait. Session readers and heartbeats must still make progress.
        assert!(
            completion.recv_timeout(Duration::from_secs(1)).is_ok(),
            "broker session tasks stopped while completion waited for its child"
        );
    }
}
