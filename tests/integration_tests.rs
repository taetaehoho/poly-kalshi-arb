// tests/integration_tests.rs
// Holistic integration tests for the arbitrage bot
//
// These tests verify the full flow:
// 1. Arb detection (with fee awareness)
// 2. Position tracking after fills
// 3. Circuit breaker behavior
// 4. End-to-end scenarios

// Note: Arc and RwLock are used by various test modules below

// Note: Old helper function `make_market_state` and `detection_tests` module were removed
// as they tested the old non-atomic MarketArbState architecture that has been deleted.
// The atomic-equivalent tests are in the `integration_tests` module below.

// ============================================================================
// POSITION TRACKER TESTS - Verify fill recording and P&L calculation
// ============================================================================

mod position_tracker_tests {
    use arb_bot::position_tracker::*;
    
    /// Test: Recording fills updates position correctly
    #[test]
    fn test_record_fills_updates_position() {
        let mut tracker = PositionTracker::new();
        
        // Record a Kalshi NO fill
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Market",
            "kalshi",
            "no",
            10.0,   // 10 contracts
            0.50,   // at 50¢
            0.18,   // 18¢ fees (for 10 contracts)
            "order123",
        ));
        
        // Record a Polymarket YES fill
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Market",
            "polymarket",
            "yes",
            10.0,   // 10 contracts
            0.40,   // at 40¢
            0.0,    // no fees
            "order456",
        ));
        
        let summary = tracker.summary();
        
        assert_eq!(summary.open_positions, 1, "Should have 1 open position");
        assert!(summary.total_contracts > 0.0, "Should have contracts");
        
        // Cost basis: 10 * 0.50 + 10 * 0.40 = $9.00 + fees
        assert!(summary.total_cost_basis > 9.0, "Cost basis should be > $9");
    }
    
    /// Test: Matched arb calculates guaranteed profit
    #[test]
    fn test_matched_arb_guaranteed_profit() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");
        
        // Buy 10 YES on Poly at 40¢
        pos.poly_yes.add(10.0, 0.40);
        
        // Buy 10 NO on Kalshi at 50¢
        pos.kalshi_no.add(10.0, 0.50);
        
        // Add fees
        pos.total_fees = 0.18;  // 18¢ fees
        
        // Total cost: $4.00 + $5.00 + $0.18 = $9.18
        // Guaranteed payout: $10.00
        // Guaranteed profit: $0.82
        
        assert!((pos.total_cost() - 9.18).abs() < 0.01, "Cost should be $9.18");
        assert!((pos.matched_contracts() - 10.0).abs() < 0.01, "Should have 10 matched");
        assert!(pos.guaranteed_profit() > 0.0, "Should have positive guaranteed profit");
        assert!((pos.guaranteed_profit() - 0.82).abs() < 0.01, "Profit should be ~$0.82");
    }
    
    /// Test: Partial fills create exposure
    #[test]
    fn test_partial_fill_creates_exposure() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");
        
        // Full fill on Poly YES
        pos.poly_yes.add(10.0, 0.40);
        
        // Partial fill on Kalshi NO (only 7 contracts)
        pos.kalshi_no.add(7.0, 0.50);
        
        assert!((pos.matched_contracts() - 7.0).abs() < 0.01, "Should have 7 matched");
        assert!((pos.unmatched_exposure() - 3.0).abs() < 0.01, "Should have 3 unmatched");
    }
    
    /// Test: Position resolution calculates realized P&L
    #[test]
    fn test_position_resolution() {
        let mut pos = ArbPosition::new("TEST-MARKET", "Test");
        
        pos.poly_yes.add(10.0, 0.40);   // Cost: $4.00
        pos.kalshi_no.add(10.0, 0.50);  // Cost: $5.00
        pos.total_fees = 0.18;           // Fees: $0.18
        
        // YES wins → Poly YES pays $10
        pos.resolve(true);
        
        assert_eq!(pos.status, "resolved");
        let pnl = pos.realized_pnl.expect("Should have realized P&L");
        
        // Payout: $10 - Cost: $9.18 = $0.82 profit
        assert!((pnl - 0.82).abs() < 0.01, "P&L should be ~$0.82, got {}", pnl);
    }
    
    /// Test: Daily P&L resets
    #[test]
    fn test_daily_pnl_persistence() {
        let mut tracker = PositionTracker::new();
        
        // Simulate some activity
        tracker.all_time_pnl = 100.0;
        tracker.daily_realized_pnl = 10.0;
        
        // Reset daily
        tracker.reset_daily();
        
        assert_eq!(tracker.daily_realized_pnl, 0.0, "Daily should reset");
        assert_eq!(tracker.all_time_pnl, 100.0, "All-time should persist");
    }
}

// ============================================================================
// CIRCUIT BREAKER TESTS - Verify safety limits
// ============================================================================

mod circuit_breaker_tests {
    use arb_bot::circuit_breaker::*;
    
    fn test_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            max_position_per_market: 50,
            max_total_position: 200,
            max_daily_loss: 25.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
        }
    }
    
    /// Test: Allows trades within limits
    #[tokio::test]
    async fn test_allows_trades_within_limits() {
        let cb = CircuitBreaker::new(test_config());
        
        // First trade should be allowed
        let result = cb.can_execute("market1", 10).await;
        assert!(result.is_ok(), "Should allow first trade");
        
        // Record success
        cb.record_success("market1", 10, 10, 0.50).await;
        
        // Second trade on same market should still be allowed
        let result = cb.can_execute("market1", 10).await;
        assert!(result.is_ok(), "Should allow second trade within limit");
    }
    
    /// Test: Blocks trade exceeding per-market limit
    #[tokio::test]
    async fn test_blocks_per_market_limit() {
        let cb = CircuitBreaker::new(test_config());
        
        // Fill up the market
        cb.record_success("market1", 45, 45, 1.0).await;
        
        // Try to add 10 more (would exceed 50 limit)
        let result = cb.can_execute("market1", 10).await;
        
        assert!(matches!(result, Err(TripReason::MaxPositionPerMarket { .. })),
            "Should block trade exceeding per-market limit");
    }
    
    /// Test: Blocks trade exceeding total position limit
    #[tokio::test]
    async fn test_blocks_total_position_limit() {
        let cb = CircuitBreaker::new(test_config());
        
        // Fill up multiple markets
        cb.record_success("market1", 50, 50, 1.0).await;
        cb.record_success("market2", 50, 50, 1.0).await;
        cb.record_success("market3", 50, 50, 1.0).await;
        cb.record_success("market4", 45, 45, 1.0).await;  // Total: 195
        
        // Try to add 10 more (would exceed 200 total limit)
        let result = cb.can_execute("market5", 10).await;
        
        assert!(matches!(result, Err(TripReason::MaxTotalPosition { .. })),
            "Should block trade exceeding total position limit");
    }
    
    /// Test: Consecutive errors trip the breaker
    #[tokio::test]
    async fn test_consecutive_errors_trip() {
        let cb = CircuitBreaker::new(test_config());
        
        // Record errors up to limit
        cb.record_error().await;
        assert!(cb.is_trading_allowed(), "Should still allow after 1 error");
        
        cb.record_error().await;
        assert!(cb.is_trading_allowed(), "Should still allow after 2 errors");
        
        cb.record_error().await;
        assert!(!cb.is_trading_allowed(), "Should halt after 3 errors");
        
        // Verify trip reason
        let status = cb.status().await;
        assert!(status.halted);
        assert!(matches!(status.trip_reason, Some(TripReason::ConsecutiveErrors { .. })));
    }
    
    /// Test: Success resets error count
    #[tokio::test]
    async fn test_success_resets_errors() {
        let cb = CircuitBreaker::new(test_config());
        
        // Record 2 errors
        cb.record_error().await;
        cb.record_error().await;
        
        // Record success
        cb.record_success("market1", 10, 10, 0.50).await;
        
        // Error count should be reset
        let status = cb.status().await;
        assert_eq!(status.consecutive_errors, 0, "Success should reset error count");
        
        // Should need 3 more errors to trip
        cb.record_error().await;
        cb.record_error().await;
        assert!(cb.is_trading_allowed());
    }
    
    /// Test: Manual reset clears halt
    #[tokio::test]
    async fn test_manual_reset() {
        let cb = CircuitBreaker::new(test_config());
        
        // Trip the breaker
        cb.record_error().await;
        cb.record_error().await;
        cb.record_error().await;
        assert!(!cb.is_trading_allowed());
        
        // Reset
        cb.reset().await;
        assert!(cb.is_trading_allowed(), "Should allow trading after reset");
        
        let status = cb.status().await;
        assert!(!status.halted);
        assert!(status.trip_reason.is_none());
    }
    
    /// Test: Disabled circuit breaker allows everything
    #[tokio::test]
    async fn test_disabled_allows_all() {
        let mut config = test_config();
        config.enabled = false;
        let cb = CircuitBreaker::new(config);
        
        // Should allow even excessive trades
        let result = cb.can_execute("market1", 1000).await;
        assert!(result.is_ok(), "Disabled CB should allow all trades");
        
        // Errors shouldn't trip it
        cb.record_error().await;
        cb.record_error().await;
        cb.record_error().await;
        cb.record_error().await;
        assert!(cb.is_trading_allowed(), "Disabled CB should never halt");
    }
}

// ============================================================================
// END-TO-END SCENARIO TESTS - Full flow simulation
// ============================================================================

mod e2e_tests {
    use arb_bot::position_tracker::*;
    use arb_bot::circuit_breaker::*;

    // Note: test_full_arb_lifecycle was removed because it used the deleted
    // make_market_state helper and MarketArbState::check_arbs method.
    // See arb_detection_tests::test_complete_arb_flow for the equivalent.

    /// Scenario: Circuit breaker halts trading after losses
    #[tokio::test]
    async fn test_circuit_breaker_halts_on_losses() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 100,
            max_total_position: 500,
            max_daily_loss: 10.0,  // Low threshold for test
            max_consecutive_errors: 5,
            cooldown_secs: 60,
            enabled: true,
        };
        
        let cb = CircuitBreaker::new(config);
        
        // Simulate a series of losing trades
        // (In reality this would come from actual fill data)
        cb.record_success("market1", 10, 10, -3.0).await;  // -$3
        cb.record_success("market2", 10, 10, -4.0).await;  // -$7 cumulative
        
        // Should still be allowed
        assert!(cb.can_execute("market3", 10).await.is_ok());
        
        // One more loss pushes over the limit
        cb.record_success("market3", 10, 10, -5.0).await;  // -$12 cumulative
        
        // Now should be blocked due to max daily loss
        let result = cb.can_execute("market4", 10).await;
        assert!(matches!(result, Err(TripReason::MaxDailyLoss { .. })),
            "Should halt due to max daily loss");
    }
    
    /// Scenario: Partial fill creates exposure warning
    #[tokio::test]
    async fn test_partial_fill_exposure_tracking() {
        let mut tracker = PositionTracker::new();
        
        // Full fill on one side
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test",
            "polymarket",
            "yes",
            10.0,
            0.40,
            0.0,
            "order1",
        ));
        
        // Partial fill on other side (slippage/liquidity issue)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test",
            "kalshi",
            "no",
            7.0,  // Only got 7!
            0.50,
            0.13,
            "order2",
        ));
        
        let summary = tracker.summary();
        
        // Should show exposure
        assert!(
            summary.total_unmatched_exposure > 0.0,
            "Should show unmatched exposure: {}",
            summary.total_unmatched_exposure
        );
        
        // Matched should be limited to the smaller fill
        let position = tracker.get("TEST-MARKET").expect("Should have position");
        assert!((position.matched_contracts() - 7.0).abs() < 0.01);
        assert!((position.unmatched_exposure() - 3.0).abs() < 0.01);
    }

    // Note: test_fees_prevent_false_arb was removed because it used the deleted
    // make_market_state helper and MarketArbState::check_arbs method.
    // See arb_detection_tests::test_fees_eliminate_marginal_arb for the equivalent.
}

// ============================================================================
// FILL DATA ACCURACY TESTS - Verify actual vs expected prices
// ============================================================================

mod fill_accuracy_tests {
    use arb_bot::position_tracker::*;
    
    /// Test: Actual fill price different from expected
    #[test]
    fn test_fill_price_slippage() {
        let mut tracker = PositionTracker::new();
        
        // Expected: buy at 40¢, but actually filled at 42¢ (slippage)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test",
            "polymarket",
            "yes",
            10.0,
            0.42,  // Actual fill price (worse than expected 0.40)
            0.0,
            "order1",
        ));
        
        let pos = tracker.get("TEST-MARKET").expect("Should have position");
        
        // Should use actual price
        assert!((pos.poly_yes.avg_price - 0.42).abs() < 0.001);
        assert!((pos.poly_yes.cost_basis - 4.20).abs() < 0.01);
    }
    
    /// Test: Multiple fills at different prices calculates weighted average
    #[test]
    fn test_multiple_fills_weighted_average() {
        let mut pos = ArbPosition::new("TEST", "Test");
        
        // First fill: 5 contracts at 40¢
        pos.poly_yes.add(5.0, 0.40);
        
        // Second fill: 5 contracts at 44¢ (price moved)
        pos.poly_yes.add(5.0, 0.44);
        
        // Weighted average: (5*0.40 + 5*0.44) / 10 = 0.42
        assert!((pos.poly_yes.avg_price - 0.42).abs() < 0.001);
        assert!((pos.poly_yes.cost_basis - 4.20).abs() < 0.01);
        assert!((pos.poly_yes.contracts - 10.0).abs() < 0.01);
    }
    
    /// Test: Actual fees from API response
    #[test]
    fn test_actual_fees_recorded() {
        let mut tracker = PositionTracker::new();
        
        // Kalshi reports actual fees in response
        // Expected: 18¢ for 10 contracts at 50¢
        // Actual from API: 20¢ (maybe market-specific fee)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test",
            "kalshi",
            "no",
            10.0,
            0.50,
            0.20,  // Actual fees from API
            "order1",
        ));
        
        let pos = tracker.get("TEST-MARKET").expect("Should have position");
        assert!((pos.total_fees - 0.20).abs() < 0.001, "Should use actual fees");
    }
}

// ============================================================================
// INFRASTRUCTURE INTEGRATION TESTS
// ============================================================================

mod infra_integration_tests {
    use arb_bot::types::*;

    /// Helper to create market state with prices
    fn setup_market(
        kalshi_yes: PriceCents,
        kalshi_no: PriceCents,
        poly_yes: PriceCents,
        poly_no: PriceCents,
    ) -> (GlobalState, u16) {
        let mut state = GlobalState::new();

        let pair = MarketPair {
            pair_id: "arb-test-market".into(),
            league: "epl".into(),
            market_type: MarketType::Moneyline,
            description: "Test Market".into(),
            kalshi_event_ticker: "KXEPLGAME-25DEC27CFCARS".into(),
            kalshi_market_ticker: "KXEPLGAME-25DEC27CFCARS-CFC".into(),
            poly_slug: "arb-test".into(),
            poly_yes_token: "arb_yes_token".into(),
            poly_no_token: "arb_no_token".into(),
            line_value: None,
            team_suffix: Some("CFC".into()),
        };

        let market_id = state.add_pair(pair).unwrap();

        // Set prices
        let market = state.get_by_id(market_id).unwrap();
        market.kalshi.store(kalshi_yes, kalshi_no, 1000, 1000);
        market.poly.store(poly_yes, poly_no, 1000, 1000);

        (state, market_id)
    }

    // =========================================================================
    // Arb Detection Tests (check_arbs)
    // =========================================================================

    /// Test: detects clear cross-platform arb (Poly YES + Kalshi NO)
    #[test]
    fn test_detects_poly_yes_kalshi_no_arb() {
        // Poly YES 40¢ + Kalshi NO 50¢ = 90¢ raw
        // Kalshi fee on 50¢ = 2¢
        // Effective = 92¢ → ARB!
        let (state, market_id) = setup_market(55, 50, 40, 65);

        let market = state.get_by_id(market_id).unwrap();
        let arb_mask = market.check_arbs(100);  // 100¢ = $1.00 threshold

        assert!(arb_mask & 1 != 0, "Should detect Poly YES + Kalshi NO arb (bit 0)");
    }

    /// Test: detects clear cross-platform arb (Kalshi YES + Poly NO)
    #[test]
    fn test_detects_kalshi_yes_poly_no_arb() {
        // Kalshi YES 40¢ + Poly NO 50¢ = 90¢ raw
        // Kalshi fee on 40¢ = 2¢
        // Effective = 92¢ → ARB!
        let (state, market_id) = setup_market(40, 65, 55, 50);

        let market = state.get_by_id(market_id).unwrap();
        let arb_mask = market.check_arbs(100);

        assert!(arb_mask & 2 != 0, "Should detect Kalshi YES + Poly NO arb (bit 1)");
    }

    /// Test: correctly rejects marginal arb when fees eliminate profit
    #[test]
    fn test_fees_eliminate_marginal_arb() {
        // Poly YES 49¢ + Kalshi NO 50¢ = 99¢ raw
        // Kalshi fee on 50¢ = 2¢
        // Effective = 101¢ → NOT AN ARB (costs more than $1 payout!)
        let (state, market_id) = setup_market(55, 50, 49, 55);

        let market = state.get_by_id(market_id).unwrap();
        let arb_mask = market.check_arbs(100);

        assert!(arb_mask & 1 == 0, "Fees should eliminate marginal Poly YES + Kalshi NO arb");
    }

    /// Test: returns no arbs for efficient market
    #[test]
    fn test_no_arbs_in_efficient_market() {
        // All prices sum to > $1
        let (state, market_id) = setup_market(55, 55, 52, 52);

        let market = state.get_by_id(market_id).unwrap();
        let arb_mask = market.check_arbs(100);

        assert_eq!(arb_mask, 0, "Should detect no arbs in efficient market");
    }

    /// Test: handles missing prices correctly
    #[test]
    fn test_handles_missing_prices() {
        let (state, market_id) = setup_market(50, NO_PRICE, 50, 50);

        let market = state.get_by_id(market_id).unwrap();
        let arb_mask = market.check_arbs(100);

        assert_eq!(arb_mask, 0, "Should return 0 when any price is missing");
    }

    // =========================================================================
    // Fee Calculation Tests (integer arithmetic)
    // =========================================================================

    /// Test: Integer fee calculation matches expected values
    #[test]
    fn test_kalshi_fee_cents_accuracy() {
        // At 50¢: ceil(7 * 50 * 50 / 10000) = ceil(1.75) = 2
        assert_eq!(kalshi_fee_cents(50), 2, "Fee at 50¢ should be 2¢");

        // At 10¢: ceil(7 * 10 * 90 / 10000) = ceil(0.63) = 1
        assert_eq!(kalshi_fee_cents(10), 1, "Fee at 10¢ should be 1¢");

        // At 90¢: symmetric with 10¢
        assert_eq!(kalshi_fee_cents(90), 1, "Fee at 90¢ should be 1¢");
    }

    // Note: test_fee_matches_legacy_for_common_prices was removed because
    // it tested against the deleted MarketArbState::kalshi_fee method.
    // The fee calculation (kalshi_fee_cents) is tested independently above.

    // =========================================================================
    // FastExecutionRequest Tests
    // =========================================================================

    /// Test: FastExecutionRequest calculates profit correctly
    #[test]
    fn test_execution_request_profit_calculation() {
        // Poly YES 40¢ + Kalshi NO 50¢ = 90¢
        // Kalshi fee on 50¢ = 2¢
        // Profit = 100 - 90 - 2 = 8¢
        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        assert_eq!(req.profit_cents(), 8, "Profit should be 8¢");
    }

    /// Test: FastExecutionRequest handles negative profit
    #[test]
    fn test_execution_request_negative_profit() {
        // Prices too high - no profit
        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 52,
            no_price: 52,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        assert!(req.profit_cents() < 0, "Should calculate negative profit");
    }

    // =========================================================================
    // GlobalStateLookup Tests
    // =========================================================================

    /// Test: GlobalStatelookup by Kalshi ticker hash
    #[test]
    fn test_lookup_by_kalshi_hash() {
        let (state, market_id) = setup_market(50, 50, 50, 50);

        let kalshi_hash = fxhash_str("KXEPLGAME-25DEC27CFCARS-CFC");
        let found_id = state.id_by_kalshi_hash(kalshi_hash);

        assert_eq!(found_id, Some(market_id), "Should find market by Kalshi hash");
    }

    /// Test: GlobalStatelookup by Poly token hashes
    #[test]
    fn test_lookup_by_poly_hashes() {
        let (state, market_id) = setup_market(50, 50, 50, 50);

        let poly_yes_hash = fxhash_str("arb_yes_token");
        let poly_no_hash = fxhash_str("arb_no_token");

        assert_eq!(state.id_by_poly_yes_hash(poly_yes_hash), Some(market_id));
        assert_eq!(state.id_by_poly_no_hash(poly_no_hash), Some(market_id));
    }

    /// Test: GlobalStatehandles multiple markets
    #[test]
    fn test_multiple_markets() {
        let mut state = GlobalState::new();

        // Add 5 markets
        for i in 0..5 {
            let pair = MarketPair {
                pair_id: format!("market-{}", i).into(),
                league: "epl".into(),
                market_type: MarketType::Moneyline,
                description: format!("Market {}", i).into(),
                kalshi_event_ticker: format!("KXTEST-{}", i).into(),
                kalshi_market_ticker: format!("KXTEST-{}-YES", i).into(),
                poly_slug: format!("test-{}", i).into(),
                poly_yes_token: format!("yes_{}", i).into(),
                poly_no_token: format!("no_{}", i).into(),
                line_value: None,
                team_suffix: None,
            };

            let id = state.add_pair(pair).unwrap();
            assert_eq!(id, i as u16);
        }

        assert_eq!(state.market_count(), 5);

        // All should be findable
        for i in 0..5 {
            assert!(state.get_by_id(i as u16).is_some());
            assert_eq!(state.id_by_kalshi_hash(fxhash_str(&format!("KXTEST-{}-YES", i))), Some(i as u16));
        }
    }

    // =========================================================================
    // Price Conversion Tests
    // =========================================================================

    /// Test: Price conversion roundtrip
    #[test]
    fn test_price_conversion_roundtrip() {
        for cents in [1u16, 10, 25, 50, 75, 90, 99] {
            let price = cents_to_price(cents);
            let back = price_to_cents(price);
            assert_eq!(back, cents, "Roundtrip failed for {}¢", cents);
        }
    }

    /// Test: Fast price parsing
    #[test]
    fn test_parse_price_accuracy() {
        assert_eq!(parse_price("0.50"), 50);
        assert_eq!(parse_price("0.01"), 1);
        assert_eq!(parse_price("0.99"), 99);
        assert_eq!(parse_price("0.5"), 50);  // Short format
        assert_eq!(parse_price("invalid"), 0);  // Invalid
    }

    // =========================================================================
    // Full Flow Integration Test
    // =========================================================================

    /// Test: Complete arb detection → execution request flow
    #[test]
    fn test_complete_arb_flow() {
        // 1. Setup market with arb opportunity
        let (state, market_id) = setup_market(55, 50, 40, 65);

        // 2. Detect arb (threshold = 100 cents = $1.00)
        let market = state.get_by_id(market_id).unwrap();
        let arb_mask = market.check_arbs(100);

        assert!(arb_mask & 1 != 0, "Step 2: Should detect arb");

        // 3. Extract prices for execution
        let (p_yes, _, p_yes_sz, _) = market.poly.load();
        let (_, k_no, _, k_no_sz) = market.kalshi.load();

        // 4. Build execution request
        let req = FastExecutionRequest {
            market_id,
            yes_price: p_yes,
            no_price: k_no,
            yes_size: p_yes_sz,
            no_size: k_no_sz,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        // 5. Verify request is valid
        assert_eq!(req.yes_price, 40, "YES price should be 40¢");
        assert_eq!(req.no_price, 50, "NO price should be 50¢");
        assert!(req.profit_cents() > 0, "Should have positive profit");

        // 6. Verify we can access market pair for execution
        let pair = market.pair.as_ref().expect("Should have pair");
        assert!(!pair.kalshi_market_ticker.is_empty());
        assert!(!pair.poly_yes_token.is_empty());
        assert!(!pair.poly_no_token.is_empty());
    }

    // Note: test_vs_legacy_arb_detection_consistency was removed because
    // it tested against the deleted MarketArbState, PlatformState, and MarketSide types.
    // The arb detection (check_arbs) is tested comprehensively above.
}

// ============================================================================
// EXECUTION ENGINE TESTS - Test execution without real API calls
// ============================================================================

mod execution_tests {
    use arb_bot::types::*;
    use arb_bot::circuit_breaker::*;
    use arb_bot::position_tracker::*;

    /// Test: ExecutionEngine correctly filters low-profit opportunities
    #[tokio::test]
    async fn test_execution_profit_threshold() {
        // This tests the logic flow - actual execution would need mocked clients
        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 50,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        // 50 + 50 + 2 (fee) = 102 > 100 → negative profit
        assert!(req.profit_cents() < 0, "Should have negative profit");
    }

    /// Test: ExecutionEngine respects circuit breaker
    #[tokio::test]
    async fn test_circuit_breaker_integration() {
        let config = CircuitBreakerConfig {
            max_position_per_market: 50,
            max_total_position: 200,
            max_daily_loss: 25.0,
            max_consecutive_errors: 3,
            cooldown_secs: 60,
            enabled: true,
        };

        let cb = CircuitBreaker::new(config);

        // Fill up market position
        cb.record_success("market1", 45, 45, 1.0).await;

        // Should block when adding more
        let result = cb.can_execute("market1", 10).await;
        assert!(matches!(result, Err(TripReason::MaxPositionPerMarket { .. })));
    }

    /// Test: Position tracker records fills correctly
    #[tokio::test]
    async fn test_position_tracker_integration() {
        let mut tracker = PositionTracker::new();

        // Simulate fill recording (what ExecutionEngine does)
        tracker.record_fill(&FillRecord::new(
            "test-market-1",
            "Test Market",
            "kalshi",
            "no",
            10.0,
            0.50,
            0.18,  // fees
            "test_order_123",
        ));

        tracker.record_fill(&FillRecord::new(
            "test-market-1",
            "Test Market",
            "polymarket",
            "yes",
            10.0,
            0.40,
            0.0,
            "test_order_456",
        ));

        let summary = tracker.summary();
        assert_eq!(summary.open_positions, 1);
        assert!(summary.total_guaranteed_profit > 0.0);
    }

    /// Test: NanoClock provides monotonic timing
    #[test]
    fn test_nano_clock_monotonic() {
        use arb_bot::execution::NanoClock;

        let clock = NanoClock::new();

        let t1 = clock.now_ns();
        std::thread::sleep(std::time::Duration::from_micros(100));
        let t2 = clock.now_ns();

        assert!(t2 > t1, "Clock should be monotonic");
        assert!(t2 - t1 >= 100_000, "Should measure at least 100µs");
    }
}

// ============================================================================
// MISMATCHED FILL / AUTO-CLOSE EXPOSURE TESTS
// ============================================================================
// These tests verify that when Kalshi and Polymarket fill different quantities,
// the system correctly handles the unmatched exposure.

mod mismatched_fill_tests {
    use arb_bot::position_tracker::*;

    /// Test: When Poly fills more than Kalshi, we have excess Poly exposure
    /// that needs to be sold to close the position.
    #[test]
    fn test_poly_fills_more_than_kalshi_creates_exposure() {
        let mut tracker = PositionTracker::new();

        // Scenario: Requested 10 contracts
        // Kalshi filled: 7 contracts at 50¢ (NO side)
        // Poly filled: 10 contracts at 40¢ (YES side)
        // Excess: 3 Poly YES contracts that aren't hedged

        // Record Kalshi fill (only 7)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            7.0,      // Only 7 filled
            0.50,
            0.12,     // fees
            "kalshi_order_1",
        ));

        // Record Poly fill (full 10)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            10.0,     // Full 10 filled
            0.40,
            0.0,
            "poly_order_1",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // Matched position: 7 contracts
        // Unmatched Poly YES: 3 contracts
        assert!((pos.poly_yes.contracts - 10.0).abs() < 0.01, "Poly YES should have 10 contracts");
        assert!((pos.kalshi_no.contracts - 7.0).abs() < 0.01, "Kalshi NO should have 7 contracts");

        // This position has EXPOSURE because poly_yes (10) != kalshi_no (7)
        // The 7 matched contracts are hedged (guaranteed profit)
        // The 3 excess poly_yes contracts are unhedged exposure
    }

    /// Test: When Kalshi fills more than Poly, we have excess Kalshi exposure
    /// that needs to be sold to close the position.
    #[test]
    fn test_kalshi_fills_more_than_poly_creates_exposure() {
        let mut tracker = PositionTracker::new();

        // Scenario: Requested 10 contracts
        // Kalshi filled: 10 contracts at 50¢ (NO side)
        // Poly filled: 6 contracts at 40¢ (YES side)
        // Excess: 4 Kalshi NO contracts that aren't hedged

        // Record Kalshi fill (full 10)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            10.0,     // Full 10 filled
            0.50,
            0.18,     // fees
            "kalshi_order_1",
        ));

        // Record Poly fill (only 6)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            6.0,      // Only 6 filled
            0.40,
            0.0,
            "poly_order_1",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        assert!((pos.kalshi_no.contracts - 10.0).abs() < 0.01, "Kalshi NO should have 10 contracts");
        assert!((pos.poly_yes.contracts - 6.0).abs() < 0.01, "Poly YES should have 6 contracts");

        // The 6 matched contracts are hedged
        // The 4 excess kalshi_no contracts are unhedged exposure
    }

    /// Test: After auto-closing excess Poly, position should be balanced
    #[test]
    fn test_auto_close_poly_excess_balances_position() {
        let mut tracker = PositionTracker::new();

        // Initial mismatched fill
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            7.0,
            0.50,
            0.12,
            "kalshi_order_1",
        ));

        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            10.0,
            0.40,
            0.0,
            "poly_order_1",
        ));

        // Simulate auto-close: SELL 3 Poly YES to close exposure
        // (In real execution, this would be a market order to dump the excess)
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            -3.0,     // Negative = selling/reducing position
            0.38,     // Might get worse price on the close
            0.0,
            "poly_close_order",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // After auto-close, both sides should have 7 contracts
        assert!(
            (pos.poly_yes.contracts - 7.0).abs() < 0.01,
            "Poly YES should be reduced to 7 contracts, got {}",
            pos.poly_yes.contracts
        );
        assert!(
            (pos.kalshi_no.contracts - 7.0).abs() < 0.01,
            "Kalshi NO should still have 7 contracts, got {}",
            pos.kalshi_no.contracts
        );
    }

    /// Test: After auto-closing excess Kalshi, position should be balanced
    #[test]
    fn test_auto_close_kalshi_excess_balances_position() {
        let mut tracker = PositionTracker::new();

        // Initial mismatched fill
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            10.0,
            0.50,
            0.18,
            "kalshi_order_1",
        ));

        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            6.0,
            0.40,
            0.0,
            "poly_order_1",
        ));

        // Simulate auto-close: SELL 4 Kalshi NO to close exposure
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            -4.0,     // Negative = selling/reducing position
            0.48,     // Might get worse price on the close
            0.07,     // Still pay fees
            "kalshi_close_order",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // After auto-close, both sides should have 6 contracts
        assert!(
            (pos.kalshi_no.contracts - 6.0).abs() < 0.01,
            "Kalshi NO should be reduced to 6 contracts, got {}",
            pos.kalshi_no.contracts
        );
        assert!(
            (pos.poly_yes.contracts - 6.0).abs() < 0.01,
            "Poly YES should still have 6 contracts, got {}",
            pos.poly_yes.contracts
        );
    }

    /// Test: Complete failure on one side creates full exposure
    /// (e.g., Kalshi fills 10, Poly fills 0)
    #[test]
    fn test_complete_one_side_failure_full_exposure() {
        let mut tracker = PositionTracker::new();

        // Kalshi succeeds
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            10.0,
            0.50,
            0.18,
            "kalshi_order_1",
        ));

        // Poly completely fails - no fill recorded

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // Full Kalshi exposure - must be closed immediately
        assert!((pos.kalshi_no.contracts - 10.0).abs() < 0.01);
        assert!((pos.poly_yes.contracts - 0.0).abs() < 0.01);

        // This is a dangerous situation - 10 unhedged Kalshi NO contracts
        // Auto-close should sell all 10 Kalshi NO to eliminate exposure
    }

    /// Test: Auto-close after complete one-side failure
    #[test]
    fn test_auto_close_after_complete_failure() {
        let mut tracker = PositionTracker::new();

        // Kalshi fills, Poly fails completely
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            10.0,
            0.50,
            0.18,
            "kalshi_order_1",
        ));

        // Auto-close: Sell ALL 10 Kalshi NO contracts
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            -10.0,    // Sell everything
            0.45,     // Might get a bad price in emergency close
            0.18,     // More fees
            "kalshi_close_order",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // Position should be flat (0 contracts on both sides)
        assert!(
            (pos.kalshi_no.contracts - 0.0).abs() < 0.01,
            "Kalshi NO should be 0 after emergency close, got {}",
            pos.kalshi_no.contracts
        );
    }

    /// Test: Profit calculation with partial fill and auto-close
    #[test]
    fn test_profit_with_partial_fill_and_auto_close() {
        let mut tracker = PositionTracker::new();

        // Requested 10 contracts
        // Kalshi fills 8 @ 50¢ (cost: $4.00 + 0.14 fees)
        // Poly fills 10 @ 40¢ (cost: $4.00)
        // Need to close 2 excess Poly @ 38¢ (receive: $0.76)

        // Initial fills
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "kalshi",
            "no",
            8.0,
            0.50,
            0.14,
            "kalshi_order_1",
        ));

        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            10.0,
            0.40,
            0.0,
            "poly_order_1",
        ));

        // Auto-close 2 excess Poly
        tracker.record_fill(&FillRecord::new(
            "TEST-MARKET",
            "Test Match",
            "polymarket",
            "yes",
            -2.0,
            0.38,     // Sold at 38¢ (worse than 40¢ buy price)
            0.0,
            "poly_close_order",
        ));

        let pos = tracker.get("TEST-MARKET").expect("Should have position");

        // Net position: 8 matched contracts
        // Kalshi NO: 8 @ 50¢ = $4.00 cost + $0.14 fees
        // Poly YES: 8 @ ~40¢ = ~$3.20 cost (10*0.40 - 2*0.38 = 4.00 - 0.76 = 3.24 effective for 8)

        assert!(
            (pos.kalshi_no.contracts - 8.0).abs() < 0.01,
            "Should have 8 matched Kalshi NO"
        );
        assert!(
            (pos.poly_yes.contracts - 8.0).abs() < 0.01,
            "Should have 8 matched Poly YES"
        );

        // The matched 8 contracts have guaranteed profit:
        // $1.00 payout - $0.50 kalshi - ~$0.405 poly = ~$0.095 per contract
        // But we also lost $0.02 per contract on the 2 we had to close (40¢ - 38¢)
    }
}

// ============================================================================
// PROCESS MOCK TESTS - Simulate execution flow without real APIs
// ============================================================================
// These tests verify that process correctly:
// 1. Records fills to the position tracker
// 2. Handles mismatched fills
// 3. Updates circuit breaker state
// 4. Captures order IDs from both platforms

mod process_mock_tests {
    use arb_bot::types::*;
    use arb_bot::circuit_breaker::*;
    use arb_bot::position_tracker::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Simulates what process does after execute_both_legs_async returns
    /// This allows testing the position tracking logic without real API clients
    struct MockExecutionResult {
        kalshi_filled: i64,
        poly_filled: i64,
        kalshi_cost: i64,  // cents
        poly_cost: i64,    // cents
        kalshi_order_id: String,
        poly_order_id: String,
    }

    /// Simulates the position tracking logic from process
    async fn simulate_process_position_tracking(
        tracker: &Arc<RwLock<PositionTracker>>,
        circuit_breaker: &CircuitBreaker,
        pair: &MarketPair,
        req: &FastExecutionRequest,
        result: MockExecutionResult,
    ) -> (i64, i16) {  // Returns (matched, profit_cents)
        let matched = result.kalshi_filled.min(result.poly_filled);
        let actual_profit = matched as i16 * 100 - (result.kalshi_cost + result.poly_cost) as i16;

        // Determine sides for position tracking (cross-platform only)
        let (kalshi_side, poly_side) = match req.arb_type {
            ArbType::PolyYesKalshiNo => ("no", "yes"),
            ArbType::KalshiYesPolyNo => ("yes", "no"),
        };

        // Record success to circuit breaker
        if matched > 0 {
            circuit_breaker.record_success(&pair.pair_id, matched, matched, actual_profit as f64 / 100.0).await;
        }

        // === UPDATE POSITION TRACKER (mirrors process logic) ===
        if matched > 0 || result.kalshi_filled > 0 || result.poly_filled > 0 {
            let mut tracker_guard = tracker.write().await;

            // Record Kalshi fill
            if result.kalshi_filled > 0 {
                tracker_guard.record_fill(&FillRecord::new(
                    &pair.pair_id,
                    &pair.description,
                    "kalshi",
                    kalshi_side,
                    matched as f64,
                    result.kalshi_cost as f64 / 100.0 / result.kalshi_filled.max(1) as f64,
                    0.0,
                    &result.kalshi_order_id,
                ));
            }

            // Record Poly fill
            if result.poly_filled > 0 {
                tracker_guard.record_fill(&FillRecord::new(
                    &pair.pair_id,
                    &pair.description,
                    "polymarket",
                    poly_side,
                    matched as f64,
                    result.poly_cost as f64 / 100.0 / result.poly_filled.max(1) as f64,
                    0.0,
                    &result.poly_order_id,
                ));
            }

            tracker_guard.save_async();
        }

        (matched, actual_profit)
    }

    fn test_market_pair() -> MarketPair {
        MarketPair {
            pair_id: "process-fast-test".into(),
            league: "epl".into(),
            market_type: MarketType::Moneyline,
            description: "Process Fast Test Market".into(),
            kalshi_event_ticker: "KXTEST-PROCESS".into(),
            kalshi_market_ticker: "KXTEST-PROCESS-YES".into(),
            poly_slug: "process-fast-test".into(),
            poly_yes_token: "pf_yes_token".into(),
            poly_no_token: "pf_no_token".into(),
            line_value: None,
            team_suffix: None,
        }
    }

    fn test_circuit_breaker_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            max_position_per_market: 100,
            max_total_position: 500,
            max_daily_loss: 50.0,
            max_consecutive_errors: 5,
            cooldown_secs: 60,
            enabled: true,
        }
    }

    /// Test: process records both fills to position tracker with correct order IDs
    #[tokio::test]
    async fn test_process_records_fills_with_order_ids() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 500,  // 10 contracts × 50¢
            poly_cost: 400,    // 10 contracts × 40¢
            kalshi_order_id: "kalshi_order_abc123".to_string(),
            poly_order_id: "poly_order_xyz789".to_string(),
        };

        let (matched, profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // Verify matched contracts
        assert_eq!(matched, 10, "Should have 10 matched contracts");

        // Verify profit: 10 contracts × $1 payout - $5 Kalshi - $4 Poly = $1 = 100¢
        assert_eq!(profit, 100, "Should have 100¢ profit");

        // Verify position tracker was updated
        let tracker_guard = tracker.read().await;
        let summary = tracker_guard.summary();

        assert_eq!(summary.open_positions, 1, "Should have 1 open position");
        assert!(summary.total_contracts > 0.0, "Should have contracts recorded");

        // Verify the position has both legs recorded
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position");
        assert!(pos.kalshi_no.contracts > 0.0, "Should have Kalshi NO contracts");
        assert!(pos.poly_yes.contracts > 0.0, "Should have Poly YES contracts");
    }

    /// Test: process handles Poly YES + Kalshi NO correctly (sides)
    #[tokio::test]
    async fn test_process_poly_yes_kalshi_no_sides() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        // Poly YES + Kalshi NO configuration
        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 500,
            poly_cost: 400,
            kalshi_order_id: "k_order_1".to_string(),
            poly_order_id: "p_order_1".to_string(),
        };

        simulate_process_position_tracking(&tracker, &cb, &pair, &req, result).await;

        let tracker_guard = tracker.read().await;
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position");

        // With arb_type = PolyYesKalshiNo:
        // - Kalshi side = "no"
        // - Poly side = "yes"
        assert!((pos.kalshi_no.contracts - 10.0).abs() < 0.01, "Kalshi NO should have 10 contracts");
        assert!((pos.poly_yes.contracts - 10.0).abs() < 0.01, "Poly YES should have 10 contracts");
        assert!((pos.kalshi_yes.contracts - 0.0).abs() < 0.01, "Kalshi YES should be empty");
        assert!((pos.poly_no.contracts - 0.0).abs() < 0.01, "Poly NO should be empty");
    }

    /// Test: process handles Kalshi YES + Poly NO correctly (reversed sides)
    #[tokio::test]
    async fn test_process_kalshi_yes_poly_no_sides() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        // Kalshi YES + Poly NO configuration
        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiYesPolyNo,
            detected_ns: 0,
        };

        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 400,  // Kalshi YES at 40¢
            poly_cost: 500,    // Poly NO at 50¢
            kalshi_order_id: "k_order_2".to_string(),
            poly_order_id: "p_order_2".to_string(),
        };

        simulate_process_position_tracking(&tracker, &cb, &pair, &req, result).await;

        let tracker_guard = tracker.read().await;
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position");

        // With arb_type = KalshiYesPolyNo:
        // - Kalshi side = "yes"
        // - Poly side = "no"
        assert!((pos.kalshi_yes.contracts - 10.0).abs() < 0.01, "Kalshi YES should have 10 contracts");
        assert!((pos.poly_no.contracts - 10.0).abs() < 0.01, "Poly NO should have 10 contracts");
        assert!((pos.kalshi_no.contracts - 0.0).abs() < 0.01, "Kalshi NO should be empty");
        assert!((pos.poly_yes.contracts - 0.0).abs() < 0.01, "Poly YES should be empty");
    }

    /// Test: process updates circuit breaker on success
    #[tokio::test]
    async fn test_process_updates_circuit_breaker() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 500,
            poly_cost: 400,
            kalshi_order_id: "k_order_3".to_string(),
            poly_order_id: "p_order_3".to_string(),
        };

        simulate_process_position_tracking(&tracker, &cb, &pair, &req, result).await;

        // Verify circuit breaker was updated
        let status = cb.status().await;
        assert_eq!(status.consecutive_errors, 0, "Errors should be reset after success");
        assert!(status.total_position > 0, "Total position should be tracked");
    }

    /// Test: process handles partial Kalshi fill correctly
    #[tokio::test]
    async fn test_process_partial_kalshi_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        // Kalshi only fills 7 out of 10
        let result = MockExecutionResult {
            kalshi_filled: 7,
            poly_filled: 10,
            kalshi_cost: 350,  // 7 × 50¢
            poly_cost: 400,    // 10 × 40¢
            kalshi_order_id: "k_partial".to_string(),
            poly_order_id: "p_full".to_string(),
        };

        let (matched, _profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // Matched should be min(7, 10) = 7
        assert_eq!(matched, 7, "Matched should be min of both fills");

        let tracker_guard = tracker.read().await;
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position");

        // Position tracker records matched amounts (7), not total fills
        assert!((pos.kalshi_no.contracts - 7.0).abs() < 0.01, "Should record matched Kalshi contracts");
        assert!((pos.poly_yes.contracts - 7.0).abs() < 0.01, "Should record matched Poly contracts");
    }

    /// Test: process handles partial Poly fill correctly
    #[tokio::test]
    async fn test_process_partial_poly_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        // Poly only fills 6 out of 10
        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 6,
            kalshi_cost: 500,  // 10 × 50¢
            poly_cost: 240,    // 6 × 40¢
            kalshi_order_id: "k_full".to_string(),
            poly_order_id: "p_partial".to_string(),
        };

        let (matched, _profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // Matched should be min(10, 6) = 6
        assert_eq!(matched, 6, "Matched should be min of both fills");
    }

    /// Test: process handles zero Kalshi fill
    #[tokio::test]
    async fn test_process_zero_kalshi_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        // Kalshi fills 0, Poly fills 10 (complete failure on one side)
        let result = MockExecutionResult {
            kalshi_filled: 0,
            poly_filled: 10,
            kalshi_cost: 0,
            poly_cost: 400,
            kalshi_order_id: "".to_string(),
            poly_order_id: "p_only".to_string(),
        };

        let (matched, _profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // No matched contracts since one side is 0
        assert_eq!(matched, 0, "No matched contracts when one side is 0");

        // But position tracker still records the Poly fill (for exposure tracking)
        let tracker_guard = tracker.read().await;
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position even with partial fills");

        // Poly fill should still be recorded (matched=0 means 0 recorded as matched)
        // The position exists but has 0 matched contracts
    }

    /// Test: process handles zero Poly fill
    #[tokio::test]
    async fn test_process_zero_poly_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        // Kalshi fills 10, Poly fills 0
        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 0,
            kalshi_cost: 500,
            poly_cost: 0,
            kalshi_order_id: "k_only".to_string(),
            poly_order_id: "".to_string(),
        };

        let (matched, _profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        assert_eq!(matched, 0, "No matched contracts when Poly is 0");
    }

    /// Test: process correctly calculates profit with full fills
    #[tokio::test]
    async fn test_process_profit_calculation_full_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,  // Poly YES at 40¢
            no_price: 50,   // Kalshi NO at 50¢
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        let result = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 500,  // 10 × 50¢ = $5.00 = 500¢
            poly_cost: 400,    // 10 × 40¢ = $4.00 = 400¢
            kalshi_order_id: "k_profit".to_string(),
            poly_order_id: "p_profit".to_string(),
        };

        let (matched, profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // Profit = matched × $1 payout - costs
        // = 10 × 100¢ - 500¢ - 400¢
        // = 1000¢ - 900¢
        // = 100¢
        assert_eq!(matched, 10);
        assert_eq!(profit, 100, "Profit should be 100¢ ($1.00)");
    }

    /// Test: process correctly calculates profit with partial fill
    #[tokio::test]
    async fn test_process_profit_calculation_partial_fill() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        // Partial fill: Kalshi 7, Poly 10
        let result = MockExecutionResult {
            kalshi_filled: 7,
            poly_filled: 10,
            kalshi_cost: 350,  // 7 × 50¢
            poly_cost: 400,    // 10 × 40¢ (but only 7 are matched)
            kalshi_order_id: "k_partial_profit".to_string(),
            poly_order_id: "p_partial_profit".to_string(),
        };

        let (matched, profit) = simulate_process_position_tracking(
            &tracker, &cb, &pair, &req, result
        ).await;

        // Profit = matched × $1 payout - ALL costs (including unmatched)
        // = 7 × 100¢ - 350¢ - 400¢
        // = 700¢ - 750¢
        // = -50¢ (LOSS because we paid for 10 Poly but only matched 7!)
        assert_eq!(matched, 7);
        assert_eq!(profit, -50, "Should have -50¢ loss due to unmatched Poly contracts");
    }

    /// Test: Multiple executions accumulate in position tracker
    #[tokio::test]
    async fn test_process_multiple_executions_accumulate() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        // First execution: 10 contracts
        let result1 = MockExecutionResult {
            kalshi_filled: 10,
            poly_filled: 10,
            kalshi_cost: 500,
            poly_cost: 400,
            kalshi_order_id: "k_exec_1".to_string(),
            poly_order_id: "p_exec_1".to_string(),
        };

        simulate_process_position_tracking(&tracker, &cb, &pair, &req, result1).await;

        // Second execution: 5 more contracts
        let result2 = MockExecutionResult {
            kalshi_filled: 5,
            poly_filled: 5,
            kalshi_cost: 250,
            poly_cost: 200,
            kalshi_order_id: "k_exec_2".to_string(),
            poly_order_id: "p_exec_2".to_string(),
        };

        simulate_process_position_tracking(&tracker, &cb, &pair, &req, result2).await;

        // Verify accumulated position
        let tracker_guard = tracker.read().await;
        let pos = tracker_guard.get(&pair.pair_id).expect("Should have position");

        // Should have 15 total contracts (10 + 5)
        assert!(
            (pos.kalshi_no.contracts - 15.0).abs() < 0.01,
            "Should have accumulated 15 Kalshi NO contracts, got {}",
            pos.kalshi_no.contracts
        );
        assert!(
            (pos.poly_yes.contracts - 15.0).abs() < 0.01,
            "Should have accumulated 15 Poly YES contracts, got {}",
            pos.poly_yes.contracts
        );
    }

    /// Test: Circuit breaker tracks accumulated position per market
    #[tokio::test]
    async fn test_circuit_breaker_accumulates_position() {
        let tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let cb = CircuitBreaker::new(test_circuit_breaker_config());
        let pair = test_market_pair();

        let req = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        // Execute multiple times
        for i in 0..5 {
            let result = MockExecutionResult {
                kalshi_filled: 10,
                poly_filled: 10,
                kalshi_cost: 500,
                poly_cost: 400,
                kalshi_order_id: format!("k_cb_{}", i),
                poly_order_id: format!("p_cb_{}", i),
            };

            simulate_process_position_tracking(&tracker, &cb, &pair, &req, result).await;
        }

        // Circuit breaker tracks contracts on BOTH sides (kalshi + poly)
        // 5 executions × 10 matched × 2 sides = 100 total
        let status = cb.status().await;
        assert_eq!(status.total_position, 100, "Circuit breaker should track 100 contracts total (both sides)");
    }

    // =========================================================================
    // CROSS-PLATFORM FEE TESTS
    // =========================================================================

    /// Test: Cross-platform fee calculation (fee only on Kalshi side)
    #[test]
    fn test_cross_platform_single_fee() {
        use arb_bot::types::kalshi_fee_cents;

        // PolyYesKalshiNo: fee on Kalshi NO side
        let req1 = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };
        assert_eq!(req1.estimated_fee_cents(), kalshi_fee_cents(50),
            "PolyYesKalshiNo fee should be on NO side (50¢)");

        // KalshiYesPolyNo: fee on Kalshi YES side
        let req2 = FastExecutionRequest {
            market_id: 0,
            yes_price: 40,
            no_price: 50,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiYesPolyNo,
            detected_ns: 0,
        };
        assert_eq!(req2.estimated_fee_cents(), kalshi_fee_cents(40),
            "KalshiYesPolyNo fee should be on YES side (40¢)");
    }

    /// Test: Both cross-platform types have same fees (one Kalshi side each)
    #[test]
    fn test_cross_platform_profit_comparison() {
        let yes_price = 45u16;
        let no_price = 45u16;

        let cross1 = FastExecutionRequest {
            market_id: 0,
            yes_price,
            no_price,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::PolyYesKalshiNo,
            detected_ns: 0,
        };

        let cross2 = FastExecutionRequest {
            market_id: 0,
            yes_price,
            no_price,
            yes_size: 1000,
            no_size: 1000,
            arb_type: ArbType::KalshiYesPolyNo,
            detected_ns: 0,
        };

        // Both cross-platform types have same fees (one Kalshi side each)
        assert_eq!(cross1.profit_cents(), cross2.profit_cents(),
            "Both cross-platform types should have equal profit");
    }
}