//! Tether host daemon. Connection layer lands in Module 2.

fn main() {
    println!(
        "tetherd scaffold (protocol v{})",
        tether_protocol::PROTOCOL_VERSION
    );
}
