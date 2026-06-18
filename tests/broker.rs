//! Tests for src/broker.rs (live order clients). Network/order calls aren't tested; the
//! only pure logic is the Binance HMAC-SHA256 signing, asserted in `broker::selftest()`.

#[test]
fn broker_signing() {
    folioman::broker::selftest();
}
