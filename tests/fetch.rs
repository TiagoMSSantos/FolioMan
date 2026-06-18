//! Tests for src/fetch.rs. The fetchers hit the network, so they're not unit-tested
//! offline; this pins the one ordering assumption the concurrent fan-out relies on
//! (`join_all` keeps input order — quotes line up with the tickers passed in).

use futures::future::join_all;

#[tokio::test]
async fn join_all_preserves_order() {
    let futs = (0..5).map(|i| async move { i });
    let out: Vec<i32> = join_all(futs).await;
    assert_eq!(out, vec![0, 1, 2, 3, 4]);
}
