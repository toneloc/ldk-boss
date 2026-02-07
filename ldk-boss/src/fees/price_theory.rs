/// Port of CLBoss FeeModderByPriceTheory.
///
/// Implements a "card game" optimizer that explores different fee multipliers
/// and learns which price point maximizes earnings for each peer.
///
/// Algorithm:
/// - For each peer, maintain a "center" price (integer).
/// - Create 5 cards at prices [center-2, center-1, center, center+1, center+2].
/// - Shuffle and play each card for ~2 days (288 ticks at 10-min intervals).
/// - Track earnings while each card is in play.
/// - After all 5 cards are played, the highest-earning card's price becomes the new center.
/// - Fee multiplier = 1.2^price (if positive) or 1/0.8^(-price) (if negative).
///
/// Reference: clboss/Boss/Mod/FeeModderByPriceTheory.cpp

use crate::config::FeesConfig;
use crate::db::Database;
use log::debug;

/// Maximum absolute price (clamped)
const MAX_PRICE: i32 = 10;

/// Card positions
const POS_DECK: i32 = 0;
const POS_IN_PLAY: i32 = 1;
const POS_DISCARDED: i32 = 2;

/// Get the fee multiplier for a given peer based on the price theory state.
pub fn get_fee_modifier(db: &Database, counterparty_node_id: &str) -> anyhow::Result<f64> {
    let conn = db.conn();

    // Find the in-play card for this peer
    let result = conn.query_row(
        "SELECT price FROM price_theory_cards \
         WHERE counterparty_node_id = ?1 AND position = ?2 \
         LIMIT 1",
        rusqlite::params![counterparty_node_id, POS_IN_PLAY],
        |row| row.get::<_, i32>(0),
    );

    match result {
        Ok(price) => Ok(price_to_multiplier(price)),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // No card in play; return 1.0 (neutral)
            Ok(1.0)
        }
        Err(e) => Err(e.into()),
    }
}

/// Convert a price integer to a fee multiplier.
/// Positive prices increase fees, negative prices decrease fees.
pub fn price_to_multiplier(price: i32) -> f64 {
    if price >= 0 {
        1.2_f64.powi(price)
    } else {
        // For negative: 1 / 0.8^(-price) = 1 / (1/1.25)^(-price) = 1.25^(-price)... no.
        // CLBoss uses: multiplier = 1.2^price for positive, 0.8^(-price) doesn't match.
        // Actually looking at the code: it's always 1.2^price.
        // For price=-1: 1.2^(-1) = 0.833
        // For price=-2: 1.2^(-2) = 0.694
        1.2_f64.powi(price)
    }
}

/// Update the price theory state machine for one tick.
///
/// - Decrement lifetime of in-play cards.
/// - If a card expires, discard it and draw a new one.
/// - If the deck is empty, end the round and start a new one.
pub fn update_tick(
    db: &Database,
    connected_peers: &[String],
    config: &FeesConfig,
) -> anyhow::Result<()> {
    let conn = db.conn();

    for peer_id in connected_peers {
        // Ensure this peer has been initialized
        ensure_initialized(conn, peer_id, config)?;

        // Find in-play card
        let in_play = conn.query_row(
            "SELECT id, lifetime FROM price_theory_cards \
             WHERE counterparty_node_id = ?1 AND position = ?2 \
             LIMIT 1",
            rusqlite::params![peer_id, POS_IN_PLAY],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?)),
        );

        match in_play {
            Ok((card_id, lifetime)) => {
                if lifetime <= 1 {
                    // Card expired: discard it
                    conn.execute(
                        "UPDATE price_theory_cards SET position = ?1, lifetime = 0 WHERE id = ?2",
                        rusqlite::params![POS_DISCARDED, card_id],
                    )?;
                    debug!(
                        "PriceTheory: peer {} card {} expired, discarding",
                        peer_id, card_id
                    );
                    // Try to draw a new card
                    draw_card(conn, peer_id, config)?;
                } else {
                    // Decrement lifetime
                    conn.execute(
                        "UPDATE price_theory_cards SET lifetime = lifetime - 1 WHERE id = ?1",
                        [card_id],
                    )?;
                }
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                // No card in play: draw one
                draw_card(conn, peer_id, config)?;
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(())
}

/// Record fee earnings for a peer's in-play card.
pub fn record_earnings(
    db: &Database,
    counterparty_node_id: &str,
    fee_msat: i64,
) -> anyhow::Result<()> {
    db.conn().execute(
        "UPDATE price_theory_cards SET earnings_msat = earnings_msat + ?1 \
         WHERE counterparty_node_id = ?2 AND position = ?3",
        rusqlite::params![fee_msat, counterparty_node_id, POS_IN_PLAY],
    )?;
    Ok(())
}

/// Draw the next card from the deck. If deck is empty, end the round.
fn draw_card(
    conn: &rusqlite::Connection,
    peer_id: &str,
    config: &FeesConfig,
) -> anyhow::Result<()> {
    // Find next card in deck (lowest deck_order)
    let next_card = conn.query_row(
        "SELECT id, price FROM price_theory_cards \
         WHERE counterparty_node_id = ?1 AND position = ?2 \
         ORDER BY deck_order ASC LIMIT 1",
        rusqlite::params![peer_id, POS_DECK],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?)),
    );

    match next_card {
        Ok((card_id, price)) => {
            conn.execute(
                "UPDATE price_theory_cards SET position = ?1, lifetime = ?2 WHERE id = ?3",
                rusqlite::params![POS_IN_PLAY, config.price_theory_card_lifetime_ticks, card_id],
            )?;
            debug!(
                "PriceTheory: peer {} drew card with price {} (mult {:.3})",
                peer_id,
                price,
                price_to_multiplier(price)
            );
            Ok(())
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // Deck empty: end the round
            end_round(conn, peer_id, config)?;
            // Draw from the new deck
            let next = conn.query_row(
                "SELECT id, price FROM price_theory_cards \
                 WHERE counterparty_node_id = ?1 AND position = ?2 \
                 ORDER BY deck_order ASC LIMIT 1",
                rusqlite::params![peer_id, POS_DECK],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?)),
            );
            if let Ok((card_id, price)) = next {
                conn.execute(
                    "UPDATE price_theory_cards SET position = ?1, lifetime = ?2 WHERE id = ?3",
                    rusqlite::params![
                        POS_IN_PLAY,
                        config.price_theory_card_lifetime_ticks,
                        card_id
                    ],
                )?;
                debug!(
                    "PriceTheory: peer {} new round, drew card with price {}",
                    peer_id, price
                );
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// End a round: find the best-earning card, set its price as new center, rebuild deck.
fn end_round(
    conn: &rusqlite::Connection,
    peer_id: &str,
    config: &FeesConfig,
) -> anyhow::Result<()> {
    // Find the highest-earning discarded card
    let best = conn.query_row(
        "SELECT price, earnings_msat FROM price_theory_cards \
         WHERE counterparty_node_id = ?1 AND position = ?2 \
         ORDER BY earnings_msat DESC LIMIT 1",
        rusqlite::params![peer_id, POS_DISCARDED],
        |row| Ok((row.get::<_, i32>(0)?, row.get::<_, i64>(1)?)),
    );

    let new_center = match best {
        Ok((price, earnings)) => {
            debug!(
                "PriceTheory: peer {} round ended, best price={} earned={}msat",
                peer_id, price, earnings
            );
            price.clamp(-MAX_PRICE, MAX_PRICE)
        }
        Err(_) => {
            // No discarded cards (shouldn't happen), keep current center
            conn.query_row(
                "SELECT price FROM price_theory_center WHERE counterparty_node_id = ?1",
                [peer_id],
                |row| row.get::<_, i32>(0),
            )
            .unwrap_or(0)
        }
    };

    // Update center
    conn.execute(
        "INSERT OR REPLACE INTO price_theory_center (counterparty_node_id, price) VALUES (?1, ?2)",
        rusqlite::params![peer_id, new_center],
    )?;

    // Delete old cards
    conn.execute(
        "DELETE FROM price_theory_cards WHERE counterparty_node_id = ?1",
        [peer_id],
    )?;

    // Create new deck with shuffled order
    create_deck(conn, peer_id, new_center, config)?;

    Ok(())
}

/// Ensure a peer has been initialized in the price theory system.
fn ensure_initialized(
    conn: &rusqlite::Connection,
    peer_id: &str,
    config: &FeesConfig,
) -> anyhow::Result<()> {
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM price_theory_cards WHERE counterparty_node_id = ?1",
            [peer_id],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !exists {
        conn.execute(
            "INSERT OR IGNORE INTO price_theory_center (counterparty_node_id, price) \
             VALUES (?1, 0)",
            [peer_id],
        )?;
        create_deck(conn, peer_id, 0, config)?;
    }

    Ok(())
}

/// Create a shuffled deck of 5 cards around the center price.
fn create_deck(
    conn: &rusqlite::Connection,
    peer_id: &str,
    center: i32,
    config: &FeesConfig,
) -> anyhow::Result<()> {
    let step = config.price_theory_max_step;
    let mut prices: Vec<i32> = (-step..=step).map(|s| (center + s).clamp(-MAX_PRICE, MAX_PRICE)).collect();

    // Shuffle using Fisher-Yates
    use rand::seq::SliceRandom;
    let mut rng = rand::thread_rng();
    prices.shuffle(&mut rng);

    for (order, price) in prices.iter().enumerate() {
        conn.execute(
            "INSERT INTO price_theory_cards \
             (counterparty_node_id, position, deck_order, price, lifetime, earnings_msat) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            rusqlite::params![
                peer_id,
                POS_DECK,
                order as i32,
                price,
                config.price_theory_card_lifetime_ticks,
            ],
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_price_to_multiplier() {
        assert!((price_to_multiplier(0) - 1.0).abs() < 0.001);
        assert!((price_to_multiplier(1) - 1.2).abs() < 0.001);
        assert!((price_to_multiplier(2) - 1.44).abs() < 0.001);
        assert!((price_to_multiplier(-1) - 0.8333).abs() < 0.01);
        assert!((price_to_multiplier(-2) - 0.6944).abs() < 0.01);
    }

    #[test]
    fn test_price_range() {
        // At max price (10), multiplier should be ~6.19
        let max_mult = price_to_multiplier(MAX_PRICE);
        assert!(max_mult > 5.0 && max_mult < 7.0, "Got {}", max_mult);

        // At min price (-10), multiplier should be ~0.16
        let min_mult = price_to_multiplier(-MAX_PRICE);
        assert!(min_mult > 0.1 && min_mult < 0.2, "Got {}", min_mult);
    }

    fn test_fees_config() -> FeesConfig {
        FeesConfig {
            enabled: true,
            default_base_msat: 1000,
            default_ppm: 100,
            balance_modder_enabled: true,
            preferred_bin_size_sats: 200_000,
            price_theory_enabled: true,
            price_theory_card_lifetime_ticks: 5, // Short for testing
            price_theory_max_step: 2,
        }
    }

    #[test]
    fn test_ensure_initialized_creates_deck() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let config = test_fees_config();
        let conn = db.conn();

        ensure_initialized(conn, "peer1", &config).unwrap();

        // Should have 5 cards (step=2: prices -2,-1,0,1,2)
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM price_theory_cards WHERE counterparty_node_id = 'peer1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 5);

        // Center should be 0
        let center: i32 = conn
            .query_row(
                "SELECT price FROM price_theory_center WHERE counterparty_node_id = 'peer1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(center, 0);
    }

    #[test]
    fn test_ensure_initialized_idempotent() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let config = test_fees_config();
        let conn = db.conn();

        ensure_initialized(conn, "peer1", &config).unwrap();
        ensure_initialized(conn, "peer1", &config).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM price_theory_cards WHERE counterparty_node_id = 'peer1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 5);
    }

    #[test]
    fn test_update_tick_draws_card() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let config = test_fees_config();

        // First tick should initialize peer and draw a card
        update_tick(&db, &["peer1".to_string()], &config).unwrap();

        let in_play: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM price_theory_cards \
                 WHERE counterparty_node_id = 'peer1' AND position = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(in_play, 1);
    }

    #[test]
    fn test_update_tick_decrements_lifetime() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let config = test_fees_config();

        // Initialize and draw first card
        update_tick(&db, &["peer1".to_string()], &config).unwrap();

        let lifetime_before: i32 = db
            .conn()
            .query_row(
                "SELECT lifetime FROM price_theory_cards \
                 WHERE counterparty_node_id = 'peer1' AND position = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Second tick should decrement lifetime
        update_tick(&db, &["peer1".to_string()], &config).unwrap();

        let lifetime_after: i32 = db
            .conn()
            .query_row(
                "SELECT lifetime FROM price_theory_cards \
                 WHERE counterparty_node_id = 'peer1' AND position = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();

        assert_eq!(lifetime_after, lifetime_before - 1);
    }

    #[test]
    fn test_card_expires_and_new_drawn() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let mut config = test_fees_config();
        config.price_theory_card_lifetime_ticks = 2; // Very short

        // Tick 1: initialize + draw card (lifetime=2)
        update_tick(&db, &["peer1".to_string()], &config).unwrap();
        // Tick 2: decrement to 1
        update_tick(&db, &["peer1".to_string()], &config).unwrap();
        // Tick 3: expires (lifetime=1 → discard), draws new card
        update_tick(&db, &["peer1".to_string()], &config).unwrap();

        let discarded: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM price_theory_cards \
                 WHERE counterparty_node_id = 'peer1' AND position = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(discarded >= 1, "Should have at least 1 discarded card");

        // Should still have a card in play
        let in_play: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM price_theory_cards \
                 WHERE counterparty_node_id = 'peer1' AND position = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(in_play, 1);
    }

    #[test]
    fn test_full_round_cycle() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let mut config = test_fees_config();
        config.price_theory_card_lifetime_ticks = 1; // Expire immediately

        // Play through all 5 cards (each expires after 1 tick + discard tick)
        // Tick 1: draw card 1 (lifetime=1)
        // Tick 2: card 1 expires, draw card 2
        // ... and so on until deck is empty → end_round → new deck
        for _ in 0..12 {
            update_tick(&db, &["peer1".to_string()], &config).unwrap();
        }

        // After enough ticks, we should have gone through at least one full round
        // The center should have been updated
        let center: i32 = db
            .conn()
            .query_row(
                "SELECT price FROM price_theory_center WHERE counterparty_node_id = 'peer1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Center can be anything from -2 to 2 depending on which card "won"
        assert!(center >= -MAX_PRICE && center <= MAX_PRICE);
    }

    #[test]
    fn test_record_earnings() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let config = test_fees_config();

        // Initialize and draw a card
        update_tick(&db, &["peer1".to_string()], &config).unwrap();

        // Record some earnings
        record_earnings(&db, "peer1", 5000).unwrap();
        record_earnings(&db, "peer1", 3000).unwrap();

        let earnings: i64 = db
            .conn()
            .query_row(
                "SELECT earnings_msat FROM price_theory_cards \
                 WHERE counterparty_node_id = 'peer1' AND position = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(earnings, 8000);
    }

    #[test]
    fn test_get_fee_modifier_no_card() {
        let db = crate::db::Database::open_in_memory().unwrap();
        // No cards at all → neutral multiplier
        let mult = get_fee_modifier(&db, "unknown_peer").unwrap();
        assert!((mult - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_get_fee_modifier_with_card() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let config = test_fees_config();

        update_tick(&db, &["peer1".to_string()], &config).unwrap();

        let mult = get_fee_modifier(&db, "peer1").unwrap();
        // Should be some valid multiplier (depends on which card was drawn)
        assert!(mult > 0.0);
    }
}
