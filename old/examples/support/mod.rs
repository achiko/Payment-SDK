use std::{
    future::Future,
    task::{Context, Poll, Waker},
};

/// Polls a mock future that is contractually ready on its first poll.
pub fn block_on_ready<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = std::pin::pin!(future);

    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("mock connection future unexpectedly returned Pending"),
    }
}
