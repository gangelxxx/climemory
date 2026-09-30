//! One process-level liveness line per minute; never per-agent diagnostic paths.
use std::{
    io::{self, Write},
    sync::mpsc,
    thread,
    time::Duration,
};

pub(crate) struct Heartbeat {
    stop: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Heartbeat {
    pub(crate) fn start() -> Self {
        let label = crate::ui::tr("RUNNING", "РАБОТАЕТ", "运行中");
        Self::with_interval(Duration::from_secs(60), move || {
            let mut out = io::stderr().lock();
            let _ = writeln!(out, "{label}");
            let _ = out.flush();
        })
    }
    fn with_interval(interval: Duration, mut emit: impl FnMut() + Send + 'static) -> Self {
        let (stop, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            while matches!(
                receiver.recv_timeout(interval),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                emit();
            }
        });
        Self {
            stop: Some(stop),
            worker: Some(worker),
        }
    }
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn periodic_signal_stops_with_request() {
        let (send, recv) = mpsc::channel();
        let heartbeat = Heartbeat::with_interval(Duration::from_millis(20), move || {
            let _ = send.send(());
        });
        recv.recv_timeout(Duration::from_secs(2)).unwrap();
        recv.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(heartbeat);
        while recv.try_recv().is_ok() {}
        assert_eq!(
            recv.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
    }
    #[test]
    fn quick_completion_does_not_emit_or_wait_for_interval() {
        let (send, recv) = mpsc::channel();
        let heartbeat = Heartbeat::with_interval(Duration::from_secs(60), move || {
            let _ = send.send(());
        });
        drop(heartbeat);
        assert_eq!(recv.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }
}
