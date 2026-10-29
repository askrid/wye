#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use proptest::prelude::*;

use super::*;

fn prices(input: &str, output: &str) -> Prices {
    Prices {
        input: parse_price(input).unwrap(),
        output: parse_price(output).unwrap(),
        cache_read: Some(parse_price("0.20").unwrap()),
        cache_write_5m: Some(parse_price("5.00").unwrap()),
        cache_write_1h: Some(parse_price("8.00").unwrap()),
    }
}

fn table(entry_prices: Prices, tiers: Vec<Tier>) -> Pricing {
    Pricing {
        version: "test".into(),
        entries: vec![PricedModels {
            models: vec![ModelPattern::parse("claude-opus-*").unwrap()].into(),
            upstreams: Box::new([]),
            prices: entry_prices,
            tiers: tiers.into(),
        }]
        .into(),
    }
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        ..Usage::default()
    }
}

#[test]
fn parses_prices_per_million_into_exact_pico_per_token() {
    assert_eq!(parse_price("4.00"), Ok(4_000_000));
    assert_eq!(parse_price("0.10"), Ok(100_000));
    assert_eq!(parse_price("0.000001"), Ok(1));
    assert_eq!(parse_price("15"), Ok(15_000_000));
    for bad in ["", ".5", "1.", "-1", "1.0000001", "1e3", "1,5", " 1"] {
        assert_eq!(parse_price(bad), Err(PriceError::Format), "{bad:?}");
    }
    assert_eq!(
        parse_price("99999999999999999999"),
        Err(PriceError::Overflow)
    );
}

#[test]
fn usd_strings_round_trip() {
    for pico in [0, 1, 1_234_500_000, 2_000_000_000_000, u64::MAX] {
        assert_eq!(parse_usd(&Usd(pico).to_string()), Some(pico));
    }
    for bad in ["", "-1", "1.", "0.0000000000001", "x"] {
        assert_eq!(parse_usd(bad), None, "{bad:?}");
    }
}

#[test]
fn formats_usd_exactly() {
    assert_eq!(Usd(0).to_string(), "0");
    assert_eq!(Usd(1_234_500_000).to_string(), "0.0012345");
    assert_eq!(Usd(2_000_000_000_000).to_string(), "2");
    assert_eq!(Usd(1).to_string(), "0.000000000001");
}

#[test]
fn prices_tokens_cache_reads_and_writes_by_ttl() {
    let t = table(prices("4.00", "20.00"), vec![]);
    let u = Usage {
        cache_read_input_tokens: Some(1_000_000),
        cache_creation_input_tokens: Some(3_000_000),
        cache_creation_5m_tokens: Some(1_000_000),
        cache_creation_1h_tokens: Some(2_000_000),
        ..usage(1_000_000, 1_000_000)
    };
    // 4 + 20 + 0.20 + 5 + 2 x 8 USD.
    let cost = t.cost("claude-opus-5-5", None, &u, true);
    assert_eq!(cost.status, CostStatus::Complete);
    assert_eq!(Usd(cost.pico.unwrap()).to_string(), "45.2");
}

#[test]
fn an_unknown_ttl_split_prices_writes_high_and_is_partial() {
    let t = table(prices("4.00", "20.00"), vec![]);
    let u = Usage {
        cache_creation_input_tokens: Some(1_000_000),
        ..usage(0, 0)
    };
    let cost = t.cost("claude-opus-5-5", None, &u, true);
    assert_eq!(cost.status, CostStatus::Partial);
    assert_eq!(Usd(cost.pico.unwrap()).to_string(), "8");

    // A total the split does not cover (updated after it): the rest is of
    // unknown TTL too. 1M at 5 USD, 2M at 8 USD.
    let stale = Usage {
        cache_creation_input_tokens: Some(3_000_000),
        cache_creation_5m_tokens: Some(1_000_000),
        cache_creation_1h_tokens: Some(0),
        ..usage(0, 0)
    };
    let cost = t.cost("claude-opus-5-5", None, &stale, true);
    assert_eq!(cost.status, CostStatus::Partial);
    assert_eq!(Usd(cost.pico.unwrap()).to_string(), "21");
}

#[test]
fn missing_prices_and_extras_make_it_partial_never_zero() {
    let mut p = prices("0.10", "0.50");
    p.cache_read = None;
    let t = table(p, vec![]);
    let read = Usage {
        cache_read_input_tokens: Some(10),
        ..usage(10, 10)
    };
    assert_eq!(
        t.cost("claude-opus-5-5", None, &read, true).status,
        CostStatus::Partial
    );
    let tools = Usage {
        unpriced: true,
        ..usage(10, 10)
    };
    assert_eq!(
        t.cost("claude-opus-5-5", None, &tools, true).status,
        CostStatus::Partial
    );
    assert_eq!(
        t.cost("claude-opus-5-5", None, &usage(10, 10), false)
            .status,
        CostStatus::Partial
    );
    // No price for the model, or no usage: unknown, not $0.
    assert_eq!(
        t.cost("claude-haiku-5-5", None, &usage(10, 10), true),
        Cost::UNKNOWN
    );
    assert_eq!(
        t.cost("claude-opus-5-5", None, &Usage::default(), true),
        Cost::UNKNOWN
    );
}

#[test]
fn the_highest_tier_reached_prices_the_whole_request() {
    let tier = |above, input| Tier {
        above_prompt_tokens: above,
        input: Some(parse_price(input).unwrap()),
        output: None,
        cache_read: None,
        cache_write_5m: None,
        cache_write_1h: None,
    };
    let t = table(
        prices("1.00", "2.00"),
        vec![tier(100, "3.00"), tier(200, "5.00")],
    );
    let pico = |input| {
        t.cost("claude-opus-5-5", None, &usage(input, 0), true)
            .pico
            .unwrap()
    };
    assert_eq!(pico(100), 100 * 1_000_000);
    assert_eq!(pico(101), 101 * 3_000_000);
    assert_eq!(pico(201), 201 * 5_000_000);
}

#[test]
fn overflow_fails_closed() {
    let t = table(prices("1000000", "1000000"), vec![]);
    assert_eq!(
        t.cost("claude-opus-5-5", None, &usage(u64::MAX / 2, 1), true),
        Cost::UNKNOWN
    );
}

proptest! {
    /// The cost is the exact sum of tokens times prices, whenever it fits.
    #[test]
    fn cost_is_the_exact_checked_sum(
        input in 0u64..1 << 40,
        output in 0u64..1 << 40,
        read in 0u64..1 << 40,
        five in 0u64..1 << 40,
        one in 0u64..1 << 40,
        price_in in 0u64..1 << 30,
        price_out in 0u64..1 << 30,
    ) {
        let p = Prices {
            input: price_in,
            output: price_out,
            cache_read: Some(price_in / 10),
            cache_write_5m: Some(price_in + price_in / 4),
            cache_write_1h: Some(price_in * 2),
        };
        let t = table(p, vec![]);
        let u = Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_input_tokens: Some(read),
            cache_creation_input_tokens: Some(five + one),
            cache_creation_5m_tokens: Some(five),
            cache_creation_1h_tokens: Some(one),
            unpriced: false,
        };
        let want = [
            (input, p.input),
            (output, p.output),
            (read, p.cache_read.unwrap()),
            (five, p.cache_write_5m.unwrap()),
            (one, p.cache_write_1h.unwrap()),
        ]
        .iter()
        .map(|&(n, price)| u128::from(n) * u128::from(price))
        .sum::<u128>();
        let cost = t.cost("claude-opus-5-5", None, &u, true);
        match u64::try_from(want) {
            Ok(want) => {
                prop_assert_eq!(cost.pico, Some(want));
                prop_assert_eq!(cost.status, CostStatus::Complete);
            }
            Err(_) => prop_assert_eq!(cost, Cost::UNKNOWN),
        }
    }
}

#[test]
fn an_entry_scoped_to_upstreams_prices_only_their_requests() {
    let entry = |upstreams: &[&str], input: &str| PricedModels {
        models: vec![ModelPattern::parse("claude-opus-*").unwrap()].into(),
        upstreams: upstreams.iter().map(|u| Box::from(*u)).collect(),
        prices: prices(input, "0"),
        tiers: Box::new([]),
    };
    let table = Pricing {
        version: "test".into(),
        // The unscoped entry first: the scoped one still wins for its upstream.
        entries: vec![entry(&[], "4.00"), entry(&["aws-east"], "4.40")].into(),
    };
    let one_million = usage(1_000_000, 0);
    let pico = |upstream| {
        table
            .cost("claude-opus-5-5", upstream, &one_million, true)
            .pico
    };
    assert_eq!(pico(Some("aws-east")), Some(4_400_000_000_000));
    assert_eq!(pico(Some("team-a")), Some(4_000_000_000_000));
    assert_eq!(pico(None), Some(4_000_000_000_000));
}
