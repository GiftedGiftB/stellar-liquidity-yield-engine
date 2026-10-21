use soroban_sdk::{
    contract, contractimpl, contracttype, Address, Env, Symbol, Vec,
};

/// Risk management engine for liquidity arbitrage
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RiskAssessment {
    pub pool_id: Address,
    pub impermanent_loss_risk: u32, // Basis points
    pub estimated_slippage: u32, // Basis points
    pub volatility_score: u32, // 0-100
    pub circuit_breaker_triggered: bool,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolatilityMetrics {
    pub price_correlation: i128, // -10000 to 10000 (percentage)
    pub volatility_24h: u32, // Basis points
    pub volatility_7d: u32, // Basis points
}

/// Configurable circuit breaker thresholds per pool
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CircuitBreakerConfig {
    pub max_imbalance: u32,          // 0-100 scale, default 80
    pub max_consecutive_losses: u32, // default 3
    pub gas_surge_protection: bool,  // default true
}

#[contract]
pub struct RiskEngine;

#[contractimpl]
impl RiskEngine {
    /// Calculate impermanent loss given price changes using the standard formula
    /// `IL = 1 - 2 * sqrt(r) / (1 + r)`, where `r = current / entry`.
    /// Returns the loss as basis points, capped at 10000 (100%).
    ///
    /// The arithmetic is scaled and multiplies-before-dividing so intermediate
    /// integer division never truncates (previously the result was discontinuous).
    pub fn calculate_impermanent_loss(
        _env: Env,
        current_price_ratio: i128, // Scaled by 10000
        entry_price_ratio: i128, // Scaled by 10000
    ) -> u32 {
        if current_price_ratio <= 0 || entry_price_ratio <= 0 {
            return 0;
        }

        // r scaled by 10000: (current / entry) * 10000
        let r_scaled = (current_price_ratio * 10000) / entry_price_ratio;

        // sqrt(r) scaled by 10000 = sqrt(r_scaled * 10000)
        let sqrt_r_scaled = Self::isqrt((r_scaled as u128) * 10000);

        // term = (2 * sqrt(r) / (1 + r)) * 10000
        let denominator = r_scaled as u128 + 10000;
        let two_sqrt = sqrt_r_scaled * 2;
        let term = (two_sqrt * 10000) / denominator;

        // By AM-GM, 1 + r >= 2 * sqrt(r), so term <= 10000 and
        // `10000 - term` can never underflow. IL rises as price diverges.
        let il_basis_points = 10000_u128.saturating_sub(term);
        il_basis_points.min(10000) as u32
    }

    /// Integer square root (Babylonian), returns floor(sqrt(n)).
    fn isqrt(n: u128) -> u128 {
        if n <= 1 {
            return n;
        }
        let mut x = n;
        let mut y = x.div_ceil(2);
        while y < x {
            x = y;
            y = (x + n / x) / 2;
        }
        x
    }

    /// Estimate slippage based on pool depth and trade amount
    /// Uses constant product formula: slippage = amount / (2 * liquidity)
    pub fn estimate_slippage(
        _env: Env,
        _pool_id: Address,
        amount: i128,
        pool_liquidity: i128, // Total liquidity in pool
    ) -> u32 {
        if pool_liquidity == 0 {
            return 10000; // 100% slippage if no liquidity
        }

        // Conservative estimate: slippage = amount / (1.5 * liquidity)
        let slippage = if amount > 0 {
            (amount * 10000) / (pool_liquidity * 15 / 10)
        } else {
            0
        };

        // Cap at 100%
        if slippage > 10000 {
            10000
        } else {
            slippage as u32
        }
    }

    /// Calculate volatility adjustment factor
    /// Reduces position size if assets have high correlation volatility
    pub fn volatility_adjustment(
        _env: Env,
        volatility_metrics: VolatilityMetrics,
    ) -> u32 {
        // Adjustment factor: 10000 = full position, scales down with volatility
        // Incorporate both 24h and 7d volatility for a blended adjustment
        let volatility_average = (volatility_metrics.volatility_24h as i128 + volatility_metrics.volatility_7d as i128) / 2;
        
        // Position multiplier: 10000 * (1 - volatility_avg/50000)
        // If volatility is 50% (5000 bp), reduce position to 50%
        10000 - (volatility_average.min(5000) as u32)
    }

    /// Configure circuit breaker thresholds for a specific pool
    pub fn set_pool_circuit_breaker(
        env: Env,
        pool_id: Address,
        config: CircuitBreakerConfig,
    ) {
        env.storage()
            .persistent()
            .set(&(Symbol::new(&env, "breaker_cfg"), pool_id.clone()), &config);

        env.events().publish(
            (Symbol::new(&env, "pool_breaker_configured"), pool_id),
            (config.max_imbalance, config.max_consecutive_losses, config.gas_surge_protection),
        );
    }

    /// Get circuit breaker thresholds for a pool (falling back to standard defaults)
    pub fn get_pool_circuit_breaker(env: Env, pool_id: Address) -> CircuitBreakerConfig {
        env.storage()
            .persistent()
            .get(&(Symbol::new(&env, "breaker_cfg"), pool_id))
            .unwrap_or(CircuitBreakerConfig {
                max_imbalance: 80,
                max_consecutive_losses: 3,
                gas_surge_protection: true,
            })
    }

    /// Circuit breaker: pause rebalancing if conditions are critical according to per-pool config
    pub fn circuit_breaker_check(
        env: Env,
        pool_id: Address,
        gas_price_surge: bool, // true if gas is >2x normal
        pool_imbalance: u32, // 0-100 scale, compared against configured max_imbalance
        recent_losses: Vec<bool>, // Recent rebalance results
    ) -> bool {
        let config = Self::get_pool_circuit_breaker(env, pool_id);

        // 1. Gas prices are spiking (if enabled for this pool)
        if config.gas_surge_protection && gas_price_surge {
            return true;
        }

        // 2. Pool is imbalanced beyond pool's configured threshold
        if pool_imbalance > config.max_imbalance {
            return true;
        }

        // 3. Consecutive losses exceed pool's configured threshold
        let loss_window = config.max_consecutive_losses;
        if loss_window > 0 && recent_losses.len() >= loss_window {
            let start = recent_losses.len() - loss_window;
            let mut all_false = true;
            let mut all_true = true;
            for i in start..recent_losses.len() {
                let val = recent_losses.get(i).unwrap_or(false);
                if val {
                    all_false = false;
                } else {
                    all_true = false;
                }
            }
            if all_false || all_true {
                return true;
            }
        }

        false
    }

    /// Create a comprehensive risk assessment for an arbitrage opportunity
    pub fn assess_arbitrage_risk(
        env: Env,
        pool_id: Address,
        current_price_ratio: i128,
        entry_price_ratio: i128,
        pool_liquidity: i128,
        amount: i128,
        volatility_metrics: VolatilityMetrics,
        gas_price_surge: bool,
        pool_imbalance: u32,
        recent_losses: Vec<bool>,
    ) -> RiskAssessment {
        let il_risk = Self::calculate_impermanent_loss(env.clone(), current_price_ratio, entry_price_ratio);
        let slippage = Self::estimate_slippage(env.clone(), pool_id.clone(), amount, pool_liquidity);
        let _volatility_adjustment = Self::volatility_adjustment(env.clone(), volatility_metrics.clone());
        let circuit_breaker = Self::circuit_breaker_check(env.clone(), pool_id.clone(), gas_price_surge, pool_imbalance, recent_losses);

        // Overall volatility score: average of IL and slippage adjusted by volatility
        let combined_risk = ((il_risk as u64 + slippage as u64) / 2) as u32;
        let volatility_score = ((volatility_metrics.volatility_24h as u64 + volatility_metrics.volatility_7d as u64) / 2 / 50) as u32;
        let final_score = ((combined_risk as u64 + volatility_score as u64) / 2).min(100) as u32;

        RiskAssessment {
            pool_id,
            impermanent_loss_risk: il_risk,
            estimated_slippage: slippage,
            volatility_score: final_score,
            circuit_breaker_triggered: circuit_breaker,
            timestamp: env.ledger().timestamp(),
        }
    }

    /// Calculate total cost of rebalancing
    pub fn calculate_total_rebalance_cost(
        _env: Env,
        gas_cost: i128,
        il_cost: u32, // Basis points
        slippage_cost: u32, // Basis points
        entry_fees: i128,
        amount: i128,
    ) -> (i128, u32) {
        // Total fee basis points
        let total_fee_bp = il_cost + slippage_cost + 50; // +50bp operational overhead

        // Calculate total cost in absolute terms
        let fee_amount = (amount * total_fee_bp as i128) / 10000;
        let total_cost = gas_cost + fee_amount + entry_fees;

        (total_cost, total_fee_bp)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::Env;

    fn il(env: &Env, current: i128, entry: i128) -> u32 {
        RiskEngine::calculate_impermanent_loss(env.clone(), current, entry)
    }

    #[test]
    fn test_il_no_change_is_zero() {
        let env = Env::default();
        // r = 1 => IL = 0
        assert_eq!(il(&env, 10000, 10000), 0);
        assert_eq!(il(&env, 20000, 20000), 0);
    }

    #[test]
    fn test_il_standard_values() {
        let env = Env::default();
        // r = 2 => IL = 1 - 2*sqrt(2)/3 = 5.72% => ~572 bp (allow +-15 due to integer sqrt)
        let il_2x = il(&env, 20000, 10000);
        assert!(il_2x >= 560 && il_2x <= 590, "r=2x got {} expected ~572", il_2x);

        // r = 0.5 => symmetric to r=2 => ~572 bp
        let il_half = il(&env, 5000, 10000);
        assert!(il_half >= 560 && il_half <= 590, "r=0.5 got {} expected ~572", il_half);

        // r = 4 => IL = 1 - 2*2/5 = 20% => 2000 bp
        let il_4x = il(&env, 40000, 10000);
        assert!(il_4x >= 1985 && il_4x <= 2015, "r=4x got {} expected 2000", il_4x);

        // r = 1.5 => IL = 1 - 2*sqrt(1.5)/2.5
        // sqrt1.5=1.2247 => 2*1.2247/2.5=0.9798 => IL 2.02% => ~202 bp
        let il_1_5 = il(&env, 15000, 10000);
        assert!(il_1_5 >= 190 && il_1_5 <= 220, "r=1.5 got {} expected ~202", il_1_5);
    }

    #[test]
    fn test_il_zero_or_negative_returns_zero() {
        let env = Env::default();
        assert_eq!(il(&env, 0, 10000), 0);
        assert_eq!(il(&env, 10000, 0), 0);
        assert_eq!(il(&env, -1000, 10000), 0);
    }

    #[test]
    fn test_il_is_monotonic_with_divergence() {
        let env = Env::default();
        let il_small = il(&env, 11000, 10000); // r=1.1
        let il_large = il(&env, 20000, 10000); // r=2
        assert!(il_large > il_small, "IL should increase with price divergence");
    }

    #[test]
    fn test_default_circuit_breaker_thresholds() {
        let env = Env::default();
        let contract_id = env.register(RiskEngine, ());
        let client = RiskEngineClient::new(&env, &contract_id);
        let pool = Address::generate(&env);

        let default_cfg = client.get_pool_circuit_breaker(&pool);
        assert_eq!(default_cfg.max_imbalance, 80);
        assert_eq!(default_cfg.max_consecutive_losses, 3);
        assert!(default_cfg.gas_surge_protection);

        // Normal conditions
        assert!(!client.circuit_breaker_check(&pool, &false, &30, &Vec::new(&env)));

        // Gas surge triggers under default config
        assert!(client.circuit_breaker_check(&pool, &true, &30, &Vec::new(&env)));

        // Imbalance > 80 triggers under default config
        assert!(client.circuit_breaker_check(&pool, &false, &85, &Vec::new(&env)));
        assert!(!client.circuit_breaker_check(&pool, &false, &75, &Vec::new(&env)));

        // 3 consecutive losses trigger under default config
        let mut losses = Vec::new(&env);
        losses.push_back(false);
        losses.push_back(false);
        assert!(!client.circuit_breaker_check(&pool, &false, &30, &losses));
        losses.push_back(false);
        assert!(client.circuit_breaker_check(&pool, &false, &30, &losses));
    }

    #[test]
    fn test_configurable_pool_circuit_breaker_sensitivity() {
        let env = Env::default();
        let contract_id = env.register(RiskEngine, ());
        let client = RiskEngineClient::new(&env, &contract_id);

        let volatile_pool = Address::generate(&env);
        let stable_pool = Address::generate(&env);

        // Configure volatile pool with high sensitivity (strict limits)
        client.set_pool_circuit_breaker(
            &volatile_pool,
            &CircuitBreakerConfig {
                max_imbalance: 60,
                max_consecutive_losses: 2,
                gas_surge_protection: true,
            },
        );

        // Configure stable pool with low sensitivity (relaxed limits)
        client.set_pool_circuit_breaker(
            &stable_pool,
            &CircuitBreakerConfig {
                max_imbalance: 95,
                max_consecutive_losses: 5,
                gas_surge_protection: false,
            },
        );

        // Imbalance of 70:
        // - Triggers volatile pool (threshold 60)
        // - Does NOT trigger stable pool (threshold 95)
        assert!(
            client.circuit_breaker_check(&volatile_pool, &false, &70, &Vec::new(&env)),
            "Volatile pool must trip at 70 imbalance"
        );
        assert!(
            !client.circuit_breaker_check(&stable_pool, &false, &70, &Vec::new(&env)),
            "Stable pool must NOT trip at 70 imbalance"
        );

        // Gas price surge:
        // - Triggers volatile pool (gas protection enabled)
        // - Does NOT trigger stable pool (gas protection disabled)
        assert!(client.circuit_breaker_check(&volatile_pool, &true, &30, &Vec::new(&env)));
        assert!(!client.circuit_breaker_check(&stable_pool, &true, &30, &Vec::new(&env)));

        // 2 consecutive losses:
        // - Triggers volatile pool (threshold 2)
        // - Does NOT trigger stable pool (threshold 5)
        let mut two_losses = Vec::new(&env);
        two_losses.push_back(false);
        two_losses.push_back(false);
        assert!(client.circuit_breaker_check(&volatile_pool, &false, &30, &two_losses));
        assert!(!client.circuit_breaker_check(&stable_pool, &false, &30, &two_losses));

        // 5 consecutive losses triggers stable pool
        let mut five_losses = Vec::new(&env);
        for _ in 0..5 {
            five_losses.push_back(false);
        }
        assert!(client.circuit_breaker_check(&stable_pool, &false, &30, &five_losses));
    }

    #[test]
    fn test_circuit_breaker_in_assess_arbitrage_risk() {
        let env = Env::default();
        let contract_id = env.register(RiskEngine, ());
        let client = RiskEngineClient::new(&env, &contract_id);
        let pool = Address::generate(&env);

        let metrics = VolatilityMetrics {
            price_correlation: 5000,
            volatility_24h: 500,
            volatility_7d: 500,
        };

        // Strict pool config: max imbalance 50
        client.set_pool_circuit_breaker(
            &pool,
            &CircuitBreakerConfig {
                max_imbalance: 50,
                max_consecutive_losses: 3,
                gas_surge_protection: true,
            },
        );

        // Assessment with pool_imbalance = 60: circuit breaker must be triggered
        let assessment = client.assess_arbitrage_risk(
            &pool,
            &11000,
            &10000,
            &1000000,
            &10000,
            &metrics,
            &false,
            &60,
            &Vec::new(&env),
        );
        assert!(assessment.circuit_breaker_triggered);

        // Assessment with pool_imbalance = 40: circuit breaker not triggered
        let assessment_normal = client.assess_arbitrage_risk(
            &pool,
            &11000,
            &10000,
            &1000000,
            &10000,
            &metrics,
            &false,
            &40,
            &Vec::new(&env),
        );
        assert!(!assessment_normal.circuit_breaker_triggered);
    }
}
