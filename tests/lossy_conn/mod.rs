use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fmt::Debug;
use std::marker::Unpin;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use failure::Error;

use futures::channel::mpsc;
use futures::{ready, stream::Fuse, Future, Sink, Stream, StreamExt};

use tokio::time::{self, delay_for, Delay};

use log::{debug, trace};

use rand;
use rand::distributions::Distribution;
use rand_distr::Normal;

pub struct LossyConn<T> {
    sender: mpsc::Sender<T>,
    receiver: Fuse<mpsc::Receiver<T>>,

    loss_rate: f64,
    delay_avg: Duration,
    delay_stddev: Duration,

    delay_buffer: BinaryHeap<TTime<T>>,
    delay: Delay,
}

struct TTime<T> {
    data: T,
    time: Instant,
}

impl<T> Ord for TTime<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        other.time.cmp(&self.time)
    }
}

impl<T> PartialOrd for TTime<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> PartialEq for TTime<T> {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time
    }
}

impl<T> Eq for TTime<T> {}

// Have the queue on the Stream impl so that way flushing doesn't act strangely.
impl<T: Unpin + Debug> Stream for LossyConn<T> {
    type Item = Result<T, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        let _ = Pin::new(&mut pin.delay).poll(cx);

        if let Some(ttime) = pin.delay_buffer.peek() {
            if ttime.time <= Instant::now() {
                let val = pin.delay_buffer.pop().unwrap();

                // reset timer
                if let Some(i) = pin.delay_buffer.peek() {
                    pin.delay.reset(time::Instant::from_std(i.time));
                }

                trace!(
                    "Forwarding packet {:?}, queue.len={}",
                    val.data,
                    pin.delay_buffer.len()
                );
                return Poll::Ready(Some(Ok(val.data)));
            }
        }

        loop {
            let to_send = match ready!(Pin::new(&mut pin.receiver).poll_next(cx)) {
                None => {
                    // There can't be any more packets AND there are no packets remaining
                    if pin.delay_buffer.is_empty() {
                        return Poll::Ready(None);
                    } else {
                        return Poll::Pending;
                    }
                }
                Some(to_send) => to_send,
            };

            if rand::random::<f64>() < pin.loss_rate {
                debug!("Dropping packet: {:?}", to_send);

                // drop
                continue;
            }

            if pin.delay_avg == Duration::from_secs(0) {
                // return it
                return Poll::Ready(Some(Ok(to_send)));
            }
            // delay
            let center = pin.delay_avg.as_secs_f64();
            let stddev = pin.delay_stddev.as_secs_f64();
            let between = Normal::new(center, stddev).unwrap();
            let delay_secs = f64::abs(between.sample(&mut rand::thread_rng()));

            let delay = Duration::from_secs_f64(delay_secs);

            pin.delay_buffer.push(TTime {
                data: to_send,
                time: Instant::now() + delay,
            });

            // update the timer
            pin.delay.reset(time::Instant::from_std(
                pin.delay_buffer.peek().unwrap().time,
            ));
            let _ = Pin::new(&mut pin.delay).poll(cx);
        }
    }
}

impl<T: Sync + Send + Unpin + 'static> Sink<T> for LossyConn<T> {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Error>> {
        let _ = ready!(self.sender.poll_ready(cx));
        Poll::Ready(Ok(()))
    }

    fn start_send(mut self: Pin<&mut Self>, to_send: T) -> Result<(), Error> {
        // just discard it, like a real UDP connection
        let _ = self.sender.start_send(to_send);
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(ready!(Pin::new(&mut self.sender).poll_flush(cx))?))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(ready!(Pin::new(&mut self.sender).poll_close(cx))?))
    }
}

impl<T> LossyConn<T> {
    pub fn channel(
        loss_rate: f64,
        delay_avg: Duration,
        delay_stddev: Duration,
    ) -> (LossyConn<T>, LossyConn<T>) {
        let (a2b, bfroma) = mpsc::channel(10000);
        let (b2a, afromb) = mpsc::channel(10000);

        (
            LossyConn {
                sender: a2b,
                receiver: afromb.fuse(),
                loss_rate,
                delay_avg,
                delay_stddev,

                delay_buffer: BinaryHeap::new(),
                delay: delay_for(Duration::from_secs(0)),
            },
            LossyConn {
                sender: b2a,
                receiver: bfroma.fuse(),
                loss_rate,
                delay_avg,
                delay_stddev,

                delay_buffer: BinaryHeap::new(),
                delay: delay_for(Duration::from_secs(0)),
            },
        )
    }
}
