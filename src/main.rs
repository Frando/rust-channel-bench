use futures_lite::{Stream, StreamExt};
use futures_util::{Sink, SinkExt};
use std::time::Instant;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::PollSender;

use tokio::task::JoinSet;

fn main() {
    let rt_multi_thread = tokio::runtime::Builder::new_multi_thread().build().unwrap();
    let rt_current_thread = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let total_msgs = 1024 * 1024;
    let total_cap = 1024;

    println!("running channel benchmark with {total_msgs} messages and capacity {total_cap}");

    for n_tasks in [2, 16, 64, 256, 512] {
        let msgs_per_task = total_msgs / n_tasks;
        let settings = format!(
            "tasks: {n_tasks}, messages per task: {msgs_per_task}, capacity per task: {}",
            total_cap / n_tasks
        );
        println!("\n# multi_thread,   {settings}\n");
        rt_multi_thread.block_on(run_all(n_tasks, msgs_per_task, total_cap));
        println!("\n# current_thread, {settings}\n");
        rt_current_thread.block_on(run_all(n_tasks, msgs_per_task, total_cap));
    }
}

async fn run_all(n_tasks: usize, n_msgs: usize, total_cap: usize) {
    tokio_merged_receiver(n_tasks, n_msgs, total_cap / n_tasks).await;
    flume_merged_receiver(n_tasks, n_msgs, total_cap / n_tasks).await;
    async_channel_merged_receiver(n_tasks, n_msgs, total_cap).await;
    println!(" ");
    tokio_cloned_sender(n_tasks, n_msgs, total_cap).await;
    flume_cloned_sender(n_tasks, n_msgs, total_cap).await;
    async_channel_cloned_sender(n_tasks, n_msgs, total_cap / n_tasks).await;
}

async fn flume_cloned_sender(n_tasks: usize, n_msgs: usize, cap: usize) {
    let total = n_tasks * n_msgs;
    let (tx, rx) = flume::bounded(cap);
    let mut join_set = JoinSet::new();
    for i in 0..n_tasks {
        let tx = tx.clone();
        join_set.spawn(send_all(tx.into_sink(), n_msgs, move |j| i * j));
    }
    drop(tx);
    join_set.spawn(recv_report(rx.into_stream(), total, "cloned-send flume"));
    join_all(join_set).await;
}

async fn flume_merged_receiver(n_tasks: usize, n_msgs: usize, cap: usize) {
    let total = n_tasks * n_msgs;
    let mut join_set = JoinSet::new();
    let mut rx_all = vec![];
    for i in 0..n_tasks {
        let (tx, rx) = flume::bounded(cap);
        join_set.spawn(send_all(tx.into_sink(), n_msgs, move |j| i * j));
        rx_all.push(rx.into_stream());
    }
    let rx = futures_buffered::Merge::from_iter(rx_all);
    join_set.spawn(recv_report(rx, total, "merged-recv flume"));
    join_all(join_set).await;
}

async fn tokio_cloned_sender(n_tasks: usize, n_msgs: usize, cap: usize) {
    let total = n_tasks * n_msgs;
    let (tx, rx) = tokio::sync::mpsc::channel(cap);
    let mut join_set = JoinSet::new();
    for i in 0..n_tasks {
        join_set.spawn(send_all(PollSender::new(tx.clone()), n_msgs, move |j| {
            i * j
        }));
    }
    drop(tx);
    let rx = ReceiverStream::new(rx);
    join_set.spawn(recv_report(rx, total, "cloned-send tokio"));
    join_all(join_set).await;
}

async fn tokio_merged_receiver(n_tasks: usize, n_msgs: usize, cap: usize) {
    let total = n_tasks * n_msgs;
    let mut join_set = JoinSet::new();
    let mut rx_all = vec![];
    for i in 0..n_tasks {
        let (tx, rx) = tokio::sync::mpsc::channel(cap);
        join_set.spawn(send_all(PollSender::new(tx), n_msgs, move |j| i * j));
        rx_all.push(ReceiverStream::new(rx));
    }
    let rx = futures_buffered::Merge::from_iter(rx_all);
    join_set.spawn(recv_report(rx, total, "merged-recv tokio"));
    join_all(join_set).await;
}

async fn async_channel_cloned_sender(n_tasks: usize, n_msgs: usize, cap: usize) {
    let total = n_tasks * n_msgs;
    let (tx, rx) = async_channel::bounded(cap);
    let mut join_set = JoinSet::new();
    for i in 0..n_tasks {
        let tx = tx.clone();
        join_set.spawn(async move {
            for j in 0..n_msgs {
                let value = i * j;
                tx.send(value).await.map_err(|_| ()).unwrap();
            }
        });
    }
    drop(tx);
    join_set.spawn(recv_report(rx, total, "cloned-send async-channel"));
    join_all(join_set).await;
}

async fn async_channel_merged_receiver(n_tasks: usize, n_msgs: usize, cap: usize) {
    let total = n_tasks * n_msgs;
    let mut join_set = JoinSet::new();
    let mut rx_all = vec![];
    for i in 0..n_tasks {
        let (tx, rx) = async_channel::bounded(cap);
        join_set.spawn(async move {
            for j in 0..n_msgs {
                let value = i * j;
                tx.send(value).await.map_err(|_| ()).unwrap();
            }
        });
        rx_all.push(rx);
    }
    let rx = futures_buffered::Merge::from_iter(rx_all);
    join_set.spawn(recv_report(rx, total, "merged-recv async-channel"));
    join_all(join_set).await;
}

async fn send_all<T: Send>(
    mut sink: impl Sink<T> + Unpin,
    count: usize,
    create_item: impl Fn(usize) -> T,
) {
    for i in 0..count {
        let value = create_item(i);
        sink.send(value).await.map_err(|_| ()).unwrap();
    }
}

async fn recv_report<T>(rx: impl Stream<Item = T>, total: usize, label: &str) {
    let start = Instant::now();
    tokio::pin!(rx);
    let count = rx.count().await;
    println!(
        "{label: <30} {: >6?} ms {: >6?} ns/message",
        start.elapsed().as_millis(),
        start.elapsed().as_nanos() / total as u128
    );
    assert_eq!(count, total);
}

async fn join_all<T: 'static>(mut join_set: JoinSet<T>) {
    while let Some(r) = join_set.join_next().await {
        r.unwrap();
    }
}
