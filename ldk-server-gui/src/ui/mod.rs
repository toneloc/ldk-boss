pub mod balances;
pub mod channels;
pub mod connection;
pub mod forwarded_payments;
pub mod lightning;
pub mod network_graph;
pub mod node_info;
pub mod onchain;
pub mod payments;
pub mod peers;
pub mod tools;

pub fn truncate_id(s: &str, _start: usize, _end: usize) -> String {
	s.to_string()
}

/// Format sats as spaced BTC: "0.00 000 000 BTC"
pub fn format_sats(sats: u64) -> String {
	format_btc_spaced(sats)
}

/// Format msats as spaced BTC: "0.00 000 000 BTC"
pub fn format_msat(msat: u64) -> String {
	let sats = msat / 1000;
	format_btc_spaced(sats)
}

/// Format sats as "X.XX XXX XXX BTC" with thin-space grouping
fn format_btc_spaced(sats: u64) -> String {
	let btc = sats as f64 / 100_000_000.0;
	let raw = format!("{:.8}", btc);
	// Split at decimal point
	if let Some(dot) = raw.find('.') {
		let whole = &raw[..dot];
		let decimals: Vec<char> = raw[dot + 1..].chars().collect();
		// Group as: XX XXX XXX (2-3-3)
		let grouped = format!(
			"{}{}\u{2009}{}{}{}\u{2009}{}{}{}",
			decimals[0], decimals[1],
			decimals[2], decimals[3], decimals[4],
			decimals[5], decimals[6], decimals[7]
		);
		format!("{}.{} BTC", whole, grouped)
	} else {
		format!("{} BTC", raw)
	}
}
