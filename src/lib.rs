#[cfg(test)]
#[macro_use]
extern crate doc_comment;

#[cfg(test)]
doctest!("../README.md");


use core::mem;
use core::pin::Pin;
use futures::stream::{Fuse, FusedStream, Stream};
use futures::Future;
use futures::task::{Context, Poll};
use futures::StreamExt;
#[cfg(feature = "sink")]
use futures_sink::Sink;
use pin_utils::{unsafe_pinned, unsafe_unpinned};

use std::time::{Duration, Instant};
use futures_timer::Delay;

pub trait ChunksTimeoutStreamExt: Stream {
    fn chunks_timeout(self, capacity: usize, duration: Duration) -> ChunksTimeout<Self, NoStats>
    where
        Self: Sized,
    {
        ChunksTimeout::new(self, capacity, duration)
    }
}
impl<T: ?Sized> ChunksTimeoutStreamExt for T where T: Stream {}

pub enum FlushEvent {
    Full,
    MinTimeoutTimer,
    MaxTimeoutTimer,
    Last,
}

pub trait StatsStrategy {
    fn add(&mut self, event: FlushEvent, batch_number: usize);
}

pub struct NoStats;

impl StatsStrategy for NoStats {
    fn add(&mut self, _event: FlushEvent, _batch_number: usize) {}
}

#[derive(Debug)]
#[must_use = "streams do nothing unless polled"]
pub struct ChunksTimeout<St: Stream, Stats: StatsStrategy> {
    stream: Fuse<St>,
    items: Vec<St::Item>,
    cap: usize,
    // https://github.com/rust-lang-nursery/futures-rs/issues/1475
    clock: Option<Delay>,
    min_duration: Option<Duration>,
    max_duration: Duration,
    last_flush_time: Instant,
    stats: Stats,
}

impl<St: Unpin + Stream, Stats: StatsStrategy> Unpin for ChunksTimeout<St, Stats> {}

impl<St: Stream> ChunksTimeout<St, NoStats>
    where
        St: Stream,
{
    pub fn new(stream: St, capacity: usize, duration: Duration) -> ChunksTimeout<St, NoStats> {
        Self::with_stats(stream, capacity, None, duration, NoStats{})
    }

    // Creates a ChunksTimeout with an additional passive timer with min_duration.
    //
    // Small timeout values, 20 microseconds e.g., could lead to heavy overhead
    // from conditional variables to awake this future inside the runtime.
    // To solve that, we add a light-weight passive timer with `min_duration` timeout
    // which can only be driven by latter items inside the inner stream
    // and use the real timer with `max_duration` timeout as the deadline timeout
    // for the case that there's no upcoming item.
    pub fn with_min_timeout(stream: St, capacity: usize, min_duration: Duration, max_duration: Duration) -> ChunksTimeout<St, NoStats> {
        Self::with_stats(stream, capacity, Some(min_duration), max_duration, NoStats{})
    }
}

impl<St: Stream, Stats: StatsStrategy> ChunksTimeout<St, Stats>
where
    St: Stream,
{
    unsafe_unpinned!(items: Vec<St::Item>);
    unsafe_pinned!(clock: Option<Delay>);
    unsafe_pinned!(stream: Fuse<St>);
    unsafe_unpinned!(last_flush_time: Instant);
    unsafe_unpinned!(stats: Stats);

    pub fn with_stats(stream: St, capacity: usize, min_duration: Option<Duration>, max_duration: Duration, stats: Stats) -> ChunksTimeout<St, Stats> {
        assert!(capacity > 0);

        ChunksTimeout {
            stream: stream.fuse(),
            items: Vec::with_capacity(capacity),
            cap: capacity,
            clock: None,
            min_duration,
            max_duration,
            last_flush_time: Instant::now(),
            stats,
        }
    }

    fn take(mut self: Pin<&mut Self>) -> Vec<St::Item> {
        let cap = self.cap;
        mem::replace(self.as_mut().items(), Vec::with_capacity(cap))
    }

    /// Acquires a reference to the underlying stream that this combinator is
    /// pulling from.
    pub fn get_ref(&self) -> &St {
        self.stream.get_ref()
    }

    /// Acquires a mutable reference to the underlying stream that this
    /// combinator is pulling from.
    ///
    /// Note that care must be taken to avoid tampering with the state of the
    /// stream which may otherwise confuse this combinator.
    pub fn get_mut(&mut self) -> &mut St {
        self.stream.get_mut()
    }

    /// Acquires a pinned mutable reference to the underlying stream that this
    /// combinator is pulling from.
    ///
    /// Note that care must be taken to avoid tampering with the state of the
    /// stream which may otherwise confuse this combinator.
    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut St> {
        self.stream().get_pin_mut()
    }

    /// Consumes this combinator, returning the underlying stream.
    ///
    /// Note that this may discard intermediate state of this combinator, so
    /// care should be taken to avoid losing resources when this is called.
    pub fn into_inner(self) -> St {
        self.stream.into_inner()
    }

    fn flush(mut self: Pin<&mut Self>, event: FlushEvent) -> Poll<Option<Vec<St::Item>>> {
        *self.as_mut().clock() = None;
        if self.min_duration.is_some() {
            *self.as_mut().last_flush_time() = Instant::now();
        }

        let items = self.as_mut().take();
        self.as_mut().stats().add(event, items.len());
        return Poll::Ready(Some(items));
    }
}

impl<St: Stream, Stats: StatsStrategy> Stream for ChunksTimeout<St, Stats> {
    type Item = Vec<St::Item>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.as_mut().stream().poll_next(cx) {
                Poll::Ready(item) => match item {
                    // Push the item into the buffer and check whether it is full.
                    // If so, replace our buffer with a new and empty one and return
                    // the full one.
                    Some(item) => {
                        if self.items.is_empty() {
                            *self.as_mut().clock() =
                                Some(Delay::new(self.max_duration));
                        }
                        self.as_mut().items().push(item);
                        if self.items.len() >= self.cap {
                            return self.flush(FlushEvent::Full)
                        } else {
                            // Continue the loop
                            continue;
                        }
                    }

                    // Since the underlying stream ran out of values, return what we
                    // have buffered, if we have anything.
                    None => {
                        let last = if self.items.is_empty() {
                            None
                        } else {
                            let full_buf = mem::replace(self.as_mut().items(), Vec::new());
                            self.as_mut().stats().add(FlushEvent::Last, full_buf.len());
                            Some(full_buf)
                        };

                        return Poll::Ready(last);
                    }
                },
                // Don't return here, as we need to need check the clock.
                Poll::Pending => {}
            }

            if self.items.is_empty() {
                return Poll::Pending;
            }

            if let Some(min_duration) = self.min_duration {
                let now = Instant::now();
                if now > self.last_flush_time
                    && now.duration_since(self.last_flush_time) >= min_duration {
                    return self.flush(FlushEvent::MinTimeoutTimer);
                }
            }

            match self.as_mut().clock().as_pin_mut().map(|clock| clock.poll(cx)) {
                Some(Poll::Ready(())) => {
                    return self.flush(FlushEvent::MaxTimeoutTimer);
                }
                Some(Poll::Pending) => {}
                None => {
                    debug_assert!(
                        self.as_mut().items().is_empty(),
                        "Inner buffer is empty, but clock is available."
                    );
                }
            }

            return Poll::Pending;
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let chunk_len = if self.items.is_empty() { 0 } else { 1 };
        let (lower, upper) = self.stream.size_hint();
        let lower = lower.saturating_add(chunk_len);
        let upper = match upper {
            Some(x) => x.checked_add(chunk_len),
            None => None,
        };
        (lower, upper)
    }
}

impl<St: FusedStream, Stats: StatsStrategy> FusedStream for ChunksTimeout<St, Stats> {
    fn is_terminated(&self) -> bool {
        self.stream.is_terminated() & self.items.is_empty()
    }
}

// Forwarding impl of Sink from the underlying stream
#[cfg(feature = "sink")]
impl<S, Stats: StatsStrategy, Item> Sink<Item> for ChunksTimeout<S, Stats>
where
    S: Stream + Sink<Item>,
{
    type Error = S::Error;

    delegate_sink!(stream, Item);
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use futures::{stream, FutureExt, StreamExt, TryFutureExt};
    use std::iter;
    use std::time::{Duration, Instant};

    #[test]
    fn messages_pass_through() {
        let v = stream::iter(iter::once(5))
            .chunks_timeout(5, Duration::new(1, 0))
            .collect::<Vec<_>>();
        tokio::run(
            v.then(|x| {
                assert_eq!(vec![vec![5]], x);
                future::ready(())
            })
            .unit_error()
            .boxed()
            .compat(),
        );
    }

    #[test]
    fn message_chunks() {
        let iter = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9].into_iter();
        let stream = stream::iter(iter);

        let chunk_stream = ChunksTimeout::new(stream, 5, Duration::new(1, 0));

        let v = chunk_stream.collect::<Vec<_>>();
        tokio::run(
            v.then(|res| {
                assert_eq!(vec![vec![0, 1, 2, 3, 4], vec![5, 6, 7, 8, 9]], res);
                future::ready(())
            })
            .unit_error()
            .boxed()
            .compat(),
        );
    }

    #[test]
    fn message_early_exit() {
        let iter = vec![1, 2, 3, 4].into_iter();
        let stream = stream::iter(iter);

        let chunk_stream = ChunksTimeout::new(stream, 5, Duration::new(1, 0));

        let v = chunk_stream.collect::<Vec<_>>();
        tokio::run(
            v.then(|res| {
                assert_eq!(vec![vec![1, 2, 3, 4]], res);
                future::ready(())
            })
            .unit_error()
            .boxed()
            .compat(),
        );
    }

    // TODO: use the `tokio-test` and `futures-test-preview` crates
    #[test]
    fn message_timeout() {
        let iter = vec![1, 2, 3, 4].into_iter();
        let stream0 = stream::iter(iter);

        let iter = vec![5].into_iter();
        let stream1 = stream::iter(iter).then(move |n| {
            Delay::new(Duration::from_millis(300))
                .map(move |_| n)
        });

        let iter = vec![6, 7, 8].into_iter();
        let stream2 = stream::iter(iter);

        let stream = stream0.chain(stream1).chain(stream2);
        let chunk_stream = ChunksTimeout::new(stream, 5, Duration::from_millis(100));

        let now = Instant::now();
        let min_times = [Duration::from_millis(80), Duration::from_millis(150)];
        let max_times = [Duration::from_millis(280), Duration::from_millis(350)];
        let results = vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]];
        let mut i = 0;

        let v = chunk_stream
            .map(move |s| {
                let now2 = Instant::now();
                println!("{}: {:?} {:?}", i, now2 - now, s);
                assert!((now2 - now) < max_times[i]);
                assert!((now2 - now) > min_times[i]);
                i += 1;
                s
            })
            .collect::<Vec<_>>();

        tokio::run(
            v.then(move |res| {
                assert_eq!(res, results);
                future::ready(())
            })
            .unit_error()
            .boxed()
            .compat(),
        );
    }
}
