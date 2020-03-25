use std::time::Duration;
use futures::{prelude::*, channel::mpsc, stream::unfold};
use futures_timer::Delay;

/// Something that can produce stream of events.
pub trait IntoStream {
    type Guard;
    fn into_stream(self) -> Box<dyn Stream<Item=Self::Guard> + Unpin + Send>;
}

/// Interval that produces stream of `()` events.
pub struct Interval {
    value: Duration,
}

impl Interval {
    /// New interval.
    pub fn new(duration: Duration) -> Interval {
        Interval { value: duration }
    }
}

impl IntoStream for Interval {
    type Guard = ();

    fn into_stream(self) -> Box<dyn Stream<Item=()> + Unpin + Send> {
        let value = self.value;
        Box::new(
            unfold((), move |_| {
                Delay::new(value).map(|_| Some(((), ())))
            }).map(drop)
        )
    }
}

/// Guard for interval event
///
/// This guard detects when it is dropped and notifies the party
/// that cretated `BackSignalInterval`.
pub struct BackSignalGuard {
    sender: Option<mpsc::UnboundedSender<()>>,
}

impl Drop for BackSignalGuard {
    fn drop(&mut self) {
        let _ = self.sender.take()
            .expect("Drop cannot be called twice, and BackSignalGuard never created without sender")
            .unbounded_send(());
    }
}

/// Interval that produces stream of "guarded" events.Duration
///
/// Each time when such guarded event handled (dropped), event for the
/// receiver is generated.
pub struct BackSignalInterval {
    value: Duration,
    sender: mpsc::UnboundedSender<()>,
}

impl IntoStream for BackSignalInterval {
    type Guard = BackSignalGuard;

    fn into_stream(self) -> Box<dyn Stream<Item=BackSignalGuard> + Unpin + Send> {
        let value = self.value;
        let sender = self.sender;
        Box::new(
            unfold(sender, move |sender| {
                Delay::new(value).map(|_| {
                    let back_signal_guard = BackSignalGuard { sender: Some(sender.clone()) };
                    Some((back_signal_guard, sender))
                })
            })
        )
    }
}

impl BackSignalInterval {
    /// New interval with hanldling notification.
    pub fn new(duration: Duration) -> (Self, mpsc::UnboundedReceiver<()>) {
        let (sender, receiver) = mpsc::unbounded();
        let back_signal = BackSignalInterval {
            value: duration,
            sender: sender,
        };
        (back_signal, receiver)
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use futures::channel::oneshot;
    use std::sync::{Arc, atomic::{AtomicBool, Ordering as AtomicOrdering}};

    async fn run_test<R: IntoStream>(rythm: R, exit: oneshot::Receiver<()>, change_this: Arc<AtomicBool>) {
        let interval = rythm.into_stream().fuse();
        let exit = exit.fuse();
        futures::pin_mut!(interval, exit);
        loop {
            futures::select! {
                _ = interval.next() => {
                    change_this.store(true, AtomicOrdering::SeqCst);
                },
                _ = exit => {
                    break;
                }
            }
        }
    }

    #[test]
    fn test_back_signal() {
        let (rythm, mut rythm_receiver) = BackSignalInterval::new(std::time::Duration::from_millis(100));
        let change_this = Arc::new(AtomicBool::new(false));
        let (exit_sender, exit_receiver) = oneshot::channel();
        let background_thread = futures::executor::ThreadPool::new().unwrap();
        background_thread.spawn_ok(run_test(rythm, exit_receiver, change_this.clone()));

        futures::executor::block_on(async move {
            rythm_receiver.next().await;
            assert_eq!(change_this.load(AtomicOrdering::SeqCst), true);
            let _ = exit_sender.send(());
        });
    }
}