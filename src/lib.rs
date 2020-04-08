use std::{time::Duration, pin::Pin, task::{Poll, Context}};
use futures::{prelude::*, channel::mpsc, stream::unfold};
use futures_timer::Delay;

/// Something that can produce stream of events.
pub trait IntoStream {
    type Guard: Guard;
    fn into_stream(self) -> Box<dyn Stream<Item=Self::Guard> + Unpin + Send>;
}

pub trait Guard {
    fn skip(&mut self) {
        // By default, noop
        // For test code, this can be used to control firing of events.`
    }
}

impl Guard for () { }

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
    skip: bool,
}

impl Drop for BackSignalGuard {
    fn drop(&mut self) {
        if !self.skip {
            let _ = self.sender.take()
                .expect("Drop cannot be called twice, and BackSignalGuard never created without sender")
                .unbounded_send(());
        }
    }
}

impl Guard for BackSignalGuard {
    fn skip(&mut self) {
        self.skip = true;
    }
}

/// Interval that produces stream of "guarded" events.
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
                    let back_signal_guard = BackSignalGuard { sender: Some(sender.clone()), skip: false };
                    Some((back_signal_guard, sender))
                })
            })
        )
    }
}

/// This serves to control how timer events are handled on the receiving side.
pub struct BackSignalControl(mpsc::UnboundedReceiver<()>);

impl BackSignalControl {
    /// Clear all accumulated haNdled timer events.
    pub fn clear(&mut self) {
        while self.0.next().now_or_never().is_some() { }
    }

    /// Next handled timer event.
    pub fn next<'a>(&'a mut self) -> impl Future<Output=Option<()>> + 'a {
        self.0.next()
    }

    /// Next handled timer event.
    pub fn next_blocking<'a>(&'a mut self) -> impl Future<Output=Option<()>> + 'a {
        self.clear();
        self.0.next()
    }
}

impl BackSignalInterval {
    /// New interval with hanldling notification.
    pub fn new(duration: Duration) -> (Self, BackSignalControl) {
        let (sender, receiver) = mpsc::unbounded();
        let back_signal = BackSignalInterval {
            value: duration,
            sender: sender,
        };
        (back_signal, BackSignalControl(receiver))
    }
}

pub struct ManualSignalInterval {
    event_receiver: mpsc::UnboundedReceiver<()>,
    handle_sender: mpsc::UnboundedSender<()>,
}

pub struct ManualIntervalControl {
    handle_receiver: mpsc::UnboundedReceiver<()>,
    event_sender: mpsc::UnboundedSender<()>,
}

impl ManualIntervalControl {
    pub fn next<'a>(&'a mut self) -> Pin<Box<dyn Future<Output=Option<()>> + 'a>> {
        if let Err(_) = self.event_sender.unbounded_send(()) {
            return futures::future::ready(None).boxed()
        }
        self.handle_receiver.next().boxed()
    }
}

impl ManualSignalInterval {
    /// New manual interval.
    ///
    /// Use this when you want to control how often your timer fires.
    pub fn new() -> (Self, ManualIntervalControl) {
        let (event_sender, event_receiver) = mpsc::unbounded();
        let (handle_sender, handle_receiver) = mpsc::unbounded();
        let back_signal = ManualSignalInterval {
            event_receiver, handle_sender,
        };
        let control = ManualIntervalControl {
            event_sender, handle_receiver,
        };
        (back_signal, control)
    }
}

impl Stream for ManualSignalInterval {
    type Item = BackSignalGuard;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);
        match Pin::new(&mut this.event_receiver).poll_next(cx) {
            Poll::Ready(Some(_)) => {
                let guard = BackSignalGuard { sender: Some(this.handle_sender.clone()), skip: false };
                Poll::Ready(Some(guard))
            },
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl IntoStream for ManualSignalInterval {
    type Guard = BackSignalGuard;

    fn into_stream(self) -> Box<dyn Stream<Item=Self::Guard> + Unpin + Send> {
        Box::new(self)
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use futures::channel::oneshot;
    use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering}};

    async fn run_test<R: IntoStream>(rythm: R, exit: oneshot::Receiver<()>, change_this: Arc<AtomicBool>) {
        let interval = rythm.into_stream().fuse();
        let exit = exit.fuse();
        futures::pin_mut!(interval, exit);
        loop {
            futures::select! {
                _ = interval.next() => {
                    futures_timer::Delay::new(Duration::from_millis(100)).await;
                    change_this.store(true, AtomicOrdering::SeqCst);
                },
                _ = exit => {
                    break;
                }
            }
        }
    }

    pub static TEST_CONDITION: AtomicUsize = AtomicUsize::new(0);

    async fn run_test_conditional<R: IntoStream>(rythm: R, exit: oneshot::Receiver<()>, change_this: Arc<AtomicBool>)
    {
        let interval = rythm.into_stream().fuse();
        let exit = exit.fuse();
        futures::pin_mut!(interval, exit);
        loop {
            futures::select! {
                guard = interval.next() => {
                    futures_timer::Delay::new(Duration::from_millis(100)).await;
                    change_this.store(true, AtomicOrdering::SeqCst);
                    if TEST_CONDITION.load(AtomicOrdering::SeqCst) == 0 {
                        guard.expect("guard can never be None").skip();
                    }
                },
                _ = exit => {
                    break;
                }
            }
        }
    }

    async fn set_10_after_some_time() {
        Delay::new(Duration::from_millis(200)).await;
        TEST_CONDITION.store(10, AtomicOrdering::SeqCst);
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

    #[test]
    fn test_back_signal_blocking() {
        let (rythm, mut rythm_receiver) = BackSignalInterval::new(std::time::Duration::from_millis(100));
        let change_this = Arc::new(AtomicBool::new(false));
        let (exit_sender, exit_receiver) = oneshot::channel();
        let background_thread = futures::executor::ThreadPool::new().unwrap();
        background_thread.spawn_ok(run_test(rythm, exit_receiver, change_this.clone()));

        futures::executor::block_on(async move {
            rythm_receiver.next_blocking().await;
            assert_eq!(change_this.load(AtomicOrdering::SeqCst), true);
            let _ = exit_sender.send(());
        });
    }

    #[test]
    fn test_back_signal_blocking_with_skip() {
        let (rythm, mut rythm_receiver) = BackSignalInterval::new(std::time::Duration::from_millis(100));
        let change_this = Arc::new(AtomicBool::new(false));
        let (exit_sender, exit_receiver) = oneshot::channel();
        let background_thread = futures::executor::ThreadPool::new().unwrap();

        background_thread.spawn_ok(run_test_conditional(rythm, exit_receiver, change_this.clone()));

        background_thread.spawn_ok(set_10_after_some_time());

        futures::executor::block_on(async move {
            rythm_receiver.next_blocking().await;
            assert_eq!(change_this.load(AtomicOrdering::SeqCst), true);
            let _ = exit_sender.send(());
        });

        assert_eq!(TEST_CONDITION.load(AtomicOrdering::SeqCst), 10);
    }

    #[test]
    fn test_manual_interval() {
        let (rythm, mut control) = ManualSignalInterval::new();
        let change_this = Arc::new(AtomicBool::new(false));
        let (_, exit_receiver) = oneshot::channel();
        let background_thread = futures::executor::ThreadPool::new().unwrap();
        background_thread.spawn_ok(run_test(rythm, exit_receiver, change_this.clone()));

        futures::executor::block_on(control.next());

        assert_eq!(change_this.load(AtomicOrdering::SeqCst), true);
    }
}