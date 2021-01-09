use crossbeam_queue::ArrayQueue;

use crate::{
    sync::{shared, ReceiverShared, SenderShared},
    PollRecv, PollSend, Sink, Stream,
};

pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let (tx_shared, rx_shared) = shared(StateExtension::new(capacity));
    let sender = Sender { shared: tx_shared };

    let receiver = Receiver { shared: rx_shared };

    (sender, receiver)
}

pub struct Sender<T> {
    pub(in crate::channels::mpsc) shared: SenderShared<StateExtension<T>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Sink for Sender<T> {
    type Item = T;

    fn poll_send(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        value: Self::Item,
    ) -> crate::PollSend<Self::Item> {
        if self.shared.is_closed() {
            return PollSend::Rejected(value);
        }

        let queue = &self.shared.extension().queue;

        match queue.push(value) {
            Ok(_) => {
                self.shared.notify_receivers();
                PollSend::Ready
            }
            Err(v) => {
                self.shared.subscribe_recv(cx.waker().clone());
                PollSend::Pending(v)
            }
        }
    }
}

pub struct Receiver<T> {
    pub(in crate::channels::mpsc) shared: ReceiverShared<StateExtension<T>>,
}

impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_recv(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> crate::PollRecv<Self::Item> {
        match self.shared.extension().queue.pop() {
            Some(v) => {
                self.shared.notify_senders();
                PollRecv::Ready(v)
            }
            None => {
                if self.shared.is_closed() {
                    return PollRecv::Closed;
                }

                self.shared.subscribe_send(cx.waker().clone());
                PollRecv::Pending
            }
        }
    }
}

struct StateExtension<T> {
    queue: ArrayQueue<T>,
}

impl<T> StateExtension<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::Pin, task::Context};

    use crate::{PollRecv, PollSend, Sink, Stream};
    use futures_test::task::{new_count_waker, noop_context, panic_context};

    use super::{channel, Receiver, Sender};

    fn pin<'a, 'b>(
        chan: &mut (Sender<Message>, Receiver<Message>),
    ) -> (Pin<&mut Sender<Message>>, Pin<&mut Receiver<Message>>) {
        let tx = Pin::new(&mut chan.0);
        let rx = Pin::new(&mut chan.1);

        (tx, rx)
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Message(usize);

    #[test]
    fn send_accepted() {
        let mut cx = panic_context();
        let mut chan = channel(2);
        let (tx, _) = pin(&mut chan);

        assert_eq!(PollSend::Ready, tx.poll_send(&mut cx, Message(1)));
    }

    #[test]
    fn send_blocks() {
        let mut cx = panic_context();
        let (mut tx, rx) = channel(2);

        assert_eq!(
            PollSend::Ready,
            Pin::new(&mut tx).poll_send(&mut cx, Message(1))
        );
        assert_eq!(
            PollSend::Ready,
            Pin::new(&mut tx).poll_send(&mut cx, Message(1))
        );
    }

    #[test]
    fn send_recv() {
        let mut cx = panic_context();
        let (mut tx, mut rx) = channel(2);

        assert_eq!(
            PollSend::Ready,
            Pin::new(&mut tx).poll_send(&mut cx, Message(1))
        );
        assert_eq!(
            PollSend::Ready,
            Pin::new(&mut tx).poll_send(&mut cx, Message(2))
        );
        assert_eq!(
            PollSend::Pending(Message(3)),
            Pin::new(&mut tx).poll_send(&mut noop_context(), Message(3))
        );

        assert_eq!(
            PollRecv::Ready(Message(1)),
            Pin::new(&mut rx).poll_recv(&mut cx)
        );

        assert_eq!(
            PollRecv::Ready(Message(2)),
            Pin::new(&mut rx).poll_recv(&mut cx)
        );

        assert_eq!(PollRecv::Pending, Pin::new(&mut rx).poll_recv(&mut cx));
    }

    #[test]
    fn sender_disconnect() {
        let mut cx = panic_context();
        let (mut tx, mut rx) = channel(100);
        let mut tx2 = tx.clone();

        assert_eq!(
            PollSend::Ready,
            Pin::new(&mut tx).poll_send(&mut cx, Message(1))
        );

        assert_eq!(
            PollSend::Ready,
            Pin::new(&mut tx2).poll_send(&mut cx, Message(2))
        );

        drop(tx);
        drop(tx2);

        assert_eq!(
            PollRecv::Ready(Message(1)),
            Pin::new(&mut rx).poll_recv(&mut cx)
        );

        assert_eq!(
            PollRecv::Ready(Message(2)),
            Pin::new(&mut rx).poll_recv(&mut cx)
        );

        assert_eq!(PollRecv::Closed, Pin::new(&mut rx).poll_recv(&mut cx));
    }

    #[test]
    fn receiver_disconnect() {
        let mut cx = panic_context();
        let (mut tx, rx) = channel(100);
        let mut tx2 = tx.clone();

        assert_eq!(
            PollSend::Ready,
            Pin::new(&mut tx).poll_send(&mut cx, Message(1))
        );

        assert_eq!(
            PollSend::Ready,
            Pin::new(&mut tx2).poll_send(&mut cx, Message(2))
        );

        drop(rx);

        assert_eq!(
            PollSend::Rejected(Message(3)),
            Pin::new(&mut tx).poll_send(&mut cx, Message(3))
        );

        assert_eq!(
            PollSend::Rejected(Message(4)),
            Pin::new(&mut tx2).poll_send(&mut cx, Message(4))
        );
    }

    #[test]
    fn wake_sender() {
        let mut cx = panic_context();
        let (mut tx, mut rx) = channel(1);

        assert_eq!(
            PollSend::Ready,
            Pin::new(&mut tx).poll_send(&mut cx, Message(1))
        );

        let (w2, w2_count) = new_count_waker();
        let mut w2_context = Context::from_waker(&w2);
        assert_eq!(
            PollSend::Pending(Message(2)),
            Pin::new(&mut tx).poll_send(&mut w2_context, Message(2))
        );

        assert_eq!(0, w2_count.get());

        assert_eq!(
            PollRecv::Ready(Message(1)),
            Pin::new(&mut rx).poll_recv(&mut cx)
        );

        assert_eq!(1, w2_count.get());
        assert_eq!(PollRecv::Pending, Pin::new(&mut rx).poll_recv(&mut cx));

        assert_eq!(1, w2_count.get());
    }

    #[test]
    fn wake_receiver() {
        let mut cx = panic_context();
        let (mut tx, mut rx) = channel(100);

        let (w1, w1_count) = new_count_waker();
        let mut w1_context = Context::from_waker(&w1);

        assert_eq!(
            PollRecv::Pending,
            Pin::new(&mut rx).poll_recv(&mut w1_context)
        );

        assert_eq!(0, w1_count.get());

        assert_eq!(
            PollSend::Ready,
            Pin::new(&mut tx).poll_send(&mut cx, Message(1))
        );

        assert_eq!(1, w1_count.get());

        assert_eq!(
            PollSend::Ready,
            Pin::new(&mut tx).poll_send(&mut cx, Message(2))
        );

        assert_eq!(1, w1_count.get());
    }
}
