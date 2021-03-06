#![allow(dead_code)]

use async_std::{
    prelude::*,
    sync::{channel, Receiver, Sender},
    task,
};
use crossbeam_channel::{self, Receiver as CrossbeamReceiver, Sender as CrossbeamSender};
use std::collections::VecDeque;

/// # WorkerPool
///
/// This is a channels-oriented async worker pool.
/// It's intended to be used with relatively long-running futures that all write out to the
/// same output channel of type `Out`. The worker pool gathers all of that output in whatever
/// order it appears, and sends it to the output channel.
///
/// The number of workers in this implementation is intended as a best effort, not a fixed
/// count, with an eye towards being used in situations where we may want that number to go
/// up or down over time based on the environment conditions.
///
/// You could imagine that a system under load might decide to back off on the number of open
/// connections if it was experiencing resource contention, and conversely to add new workers
/// if the queue has grown and we aren't at our max worker count.
///
/// I'm not incredibly concerned about allocations in this model; `WorkerPool` is a higher level
/// abstraction than something like `crossbeam`. I built this for a client-side CLI use case to
/// put a load test target under variable load from long-running workers that just sit and loop
/// TCP connections against a server.
///
pub struct WorkerPool<In, Out, F> {
    /// How many workers we want
    num_workers: usize,
    /// How many workers we actually have
    cur_workers: usize,
    /// Outstanding tasks
    queue: VecDeque<In>,
    /// Output channel
    output: Sender<Out>,
    /// The async function that a worker performs
    task: fn(Job<In, Out>) -> F,
    /// Used to get completed work from workers
    results_channel: (Sender<Out>, Receiver<Out>),
    /// Used to stop workers before they self-terminate
    close_channel: (Sender<()>, Receiver<()>),
    /// Unbounded internal event and command bus, processed every tick.
    worker_events: (CrossbeamSender<WorkerEvent>, CrossbeamReceiver<WorkerEvent>),
    command_events: (CrossbeamSender<WorkerPoolCommand>, CrossbeamReceiver<WorkerPoolCommand>),

    outstanding_stops: usize,
}

#[derive(Debug, Copy, Clone)]
enum WorkerEvent {
    WorkerDone,
    WorkerStopped,
}

#[derive(Debug, Copy, Clone)]
pub enum WorkerPoolCommand {
    Stop,
    SetWorkerCount(usize),
}

// todo command channel

pub struct Job<In, Out> {
    pub task: In,
    pub close: Receiver<()>,
    pub results: Sender<Out>,
}

impl<In, Out> Job<In, Out> {
    pub fn new(task: In, close: Receiver<()>, results: Sender<Out>) -> Self {
        Self { task, close, results }
    }

    pub fn stop_requested(&self) -> bool {
        match self.close.try_recv() {
            Ok(_) => true,
            Err(_) => false,
        }
    }
}

pub enum JobStatus {
    Done,
    Stopped,
    Running,
}

impl<In, Out, F> WorkerPool<In, Out, F>
where
    In: Send + Sync + 'static,
    Out: Send + Sync + 'static,
    F: Future<Output = JobStatus> + Send + 'static,
{
    pub fn new(task: fn(Job<In, Out>) -> F, output: Sender<Out>, num_workers: usize) -> Self {
        Self {
            task,
            output,
            num_workers,
            cur_workers: 0,
            results_channel: channel(num_workers),
            close_channel: channel(num_workers),
            worker_events: crossbeam_channel::unbounded(),
            command_events: crossbeam_channel::unbounded(),
            queue: VecDeque::with_capacity(num_workers),
            outstanding_stops: 0,
        }
    }

    /// Number of workers currently working
    /// This is the number of workers we haven't tried to stop yet plus the workers that haven't
    /// noticed they were told to stop.
    pub fn cur_workers(&self) -> usize {
        self.cur_workers - self.outstanding_stops
    }

    /// Target number of workers
    pub fn target_workers(&self) -> usize {
        self.num_workers
    }

    /// Whether the current number of workers is the target number of workers
    /// Adjusted for the number of workers that we have TOLD to stop but have
    /// not actually gotten around to stopping yet.
    pub fn at_target_worker_count(&self) -> bool {
        self.cur_workers() == self.target_workers()
    }

    pub fn working(&self) -> bool {
        self.cur_workers() > 0
    }

    /// Sets the target number of workers.
    /// Does not stop in-progress workers.
    pub fn set_target_workers(&mut self, n: usize) {
        self.num_workers = n;
    }

    /// Add a new task to the back of the queue
    pub fn push(&mut self, task: In) {
        self.queue.push_back(task);
    }

    /// Attempts to grab any immediately available results from the workers
    /// todo: Eh, I'm not sure this is a good API.
    pub fn try_next(&mut self) -> Option<Out> {
        match self.results_channel.1.try_recv() {
            Ok(out) => Some(out),
            Err(_) => None,
        }
    }

    pub fn command_channel(&self) -> crossbeam_channel::Sender<WorkerPoolCommand> {
        self.command_events.0.clone()
    }

    pub async fn work(&mut self) {
        task::block_on(async {
            loop {
                self.flush_output().await;

                if !self.event_loop() {
                    break;
                }

                self.balance_workers().await;

                if !self.working() {
                    break;
                }
            }
        })
    }

    /// Processes outstanding command and worker events
    /// Returns whether or not to continue execution.
    fn event_loop(&mut self) -> bool {
        while let Ok(event) = self.worker_events.1.try_recv() {
            match event {
                WorkerEvent::WorkerDone => {
                    self.cur_workers -= 1;
                }
                WorkerEvent::WorkerStopped => {
                    self.cur_workers -= 1;
                    self.outstanding_stops -= 1;
                }
            }
        }

        while let Ok(command) = self.command_events.1.try_recv() {
            match command {
                WorkerPoolCommand::Stop => {
                    return false;
                }
                WorkerPoolCommand::SetWorkerCount(n) => {
                    let n = match n {
                        0 => 1,
                        n => n,
                    };

                    println!("{}, {}", n, self.num_workers);
                    self.num_workers = n;
                }
            }
        }

        true
    }

    /// Flush all outstanding work results to the output channel.
    ///
    /// This blocks on consumption, which gives us a nice property -- if a user only
    /// wants a limited number of messages they can just read a limited number of times.
    /// This ends up only updating the async state machine that number of times, which
    /// is the "lazy" property of async we wanted to achieve.
    async fn flush_output(&mut self) {
        while let Ok(out) = self.results_channel.1.try_recv() {
            self.output.send(out).await;
        }
    }

    /// Starts a new worker if there is work to do
    fn start_worker(&mut self) {
        if self.queue.is_empty() {
            return;
        }

        let task = self.queue.pop_front().unwrap();
        let work_send = self.results_channel.0.clone();
        let close_recv = self.close_channel.1.clone();
        let event_send = self.worker_events.0.clone();
        let job = Job::new(task, close_recv, work_send);
        let fut = (self.task)(job);

        // If a worker stops on its own without us telling it to stop then we want to know about
        // it so that we can spin up a replacement. This is done through an unbounded crossbeam
        // channnel that is processed every tick to update state.
        async_std::task::spawn(async move {
            let status = fut.await;
            let message = match status {
                JobStatus::Done => WorkerEvent::WorkerDone,
                JobStatus::Stopped => WorkerEvent::WorkerStopped,
                JobStatus::Running => panic!("this shouldn't happen"),
            };

            event_send.send(message).expect("failed to send WorkerEvent");
        });

        self.cur_workers += 1;
    }

    /// Find a listening worker and tell it to stop.
    /// Doesn't forcibly kill in-progress tasks.
    async fn send_stop_work_message(&mut self) {
        self.outstanding_stops += 1;
        self.close_channel.0.send(()).await;
    }

    /// Pops tasks from the queue if we have available worker capacity
    /// Sends out messages if any of our workers have delivered results
    pub async fn balance_workers(&mut self) {
        if self.cur_workers() < self.target_workers() {
            self.start_worker();
        } else if self.cur_workers() > self.target_workers() {
            self.send_stop_work_message().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::task;
    use futures_await_test::async_test;
    use std::time::Duration;

    /// Double the input some number of times or until we receive a close message
    async fn double(job: Job<(usize, usize), usize>) {
        let (mut i, n) = job.task;
        for _ in 0..n {
            // play nice with the pool by allowing it to stop this loop early
            if job.stop_requested() {
                break;
            }

            // do the actual work
            i *= 2;

            // send it to the pool for collection so it can be sent along to listeners
            job.results.send(i).await;

            // pretend this is hard
            task::sleep(Duration::from_millis(100)).await;
        }
    }

    #[async_test]
    async fn pool_test() {
        let num_workers = 2;
        let (send, recv) = channel(num_workers);
        let mut pool = WorkerPool::new(double, send, num_workers);

        pool.push((1, 10));
        pool.push((3, 10));
        pool.push((6, 2));

        // separate process to receive and analyze output from the worker queue
        task::spawn(async move {
            while let Ok(out) = recv.recv().await {
                dbg!(out);
            }
        });

        pool.work().await;
    }
}
