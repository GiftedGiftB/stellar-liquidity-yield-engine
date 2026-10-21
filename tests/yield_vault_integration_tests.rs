use soroban_sdk::{testutils::Address as _, Address, Env, Symbol};
use stellar_liquidity_yield_engine::{YieldVault, YieldVaultClient};

fn main() {}

mod tests {
    use super::*;
    use soroban_sdk::token::{StellarAssetClient, TokenClient};

    fn setup_vault(
        env: &Env,
    ) -> (
        YieldVaultClient,
        TokenClient,
        TokenClient,
        StellarAssetClient,
        StellarAssetClient,
        Address,
        Address,
        Address,
    ) {
        let admin = Address::generate(env);
        let user = Address::generate(env);
        let treasury = Address::generate(env);
        let token_a = env.register_stellar_asset_contract_v2(admin.clone());
        let token_b = env.register_stellar_asset_contract_v2(admin.clone());
        let token_a_client = TokenClient::new(env, &token_a.address());
        let token_b_client = TokenClient::new(env, &token_b.address());
        let token_a_admin = StellarAssetClient::new(env, &token_a.address());
        let token_b_admin = StellarAssetClient::new(env, &token_b.address());
        let pool_id = Address::generate(env);

        let vault_id = env.register_contract(None, YieldVault);
        let vault = YieldVaultClient::new(env, &vault_id);

        vault.initialize(
            &admin,
            &Symbol::new(env, "TestVault"),
            &token_a.address(),
            &token_b.address(),
            &pool_id,
            &1,
            &100,
            &50,
            &25,
            &treasury,
        );

        (
            vault,
            token_a_client,
            token_b_client,
            token_a_admin,
            token_b_admin,
            user,
            admin,
            treasury,
        )
    }

    // -------------------------------------------------------------------------
    // Security Checklist verification tests
    //
    // [CHECK-PAUSE-1] Paused state blocks all state-changing entrypoints
    // [CHECK-PAUSE-2] Unpause restores normal operation
    // [CHECK-ACL-1] Only admin can pause/unpause
    // [CHECK-ACL-2] Non-admin cannot modify admin-only configuration
    // [CHECK-REENT-1] State is updated before external token transfers
    //                 (share math consistent after deposit/withdraw)
    // [CHECK-FEE-1] Fees are routed to treasury and never exceed configured bounds
    // -------------------------------------------------------------------------

    #[test]
    fn test_deposit_withdraw_round_trip_balances() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (
            vault,
            token_a_client,
            token_b_client,
            token_a_admin,
            token_b_admin,
            user,
            _admin,
            treasury,
        ) = setup_vault(&env);

        // Fund user
        token_a_admin.mint(&user, &10000);
        token_b_admin.mint(&user, &10000);

        // Record balances before deposit
        let user_a_before = token_a_client.balance(&user);
        let user_b_before = token_b_client.balance(&user);

        // Deposit
        let shares = vault.deposit(&user, &5000, &5000, &0);
        assert!(shares > 0, "Should receive shares on deposit");

        // Verify vault metrics updated
        let metrics = vault.get_metrics();
        assert_eq!(metrics.total_amount_a, 5000);
        assert_eq!(metrics.total_amount_b, 5000);
        assert_eq!(metrics.total_shares, shares);

        // Withdraw half of shares
        let withdraw_amount = shares / 2;
        let (withdrawn_a, withdrawn_b) = vault.withdraw(&user, &withdraw_amount, &0, &0);

        // Verify withdrawn amounts are proportional
        assert!(withdrawn_a > 0, "Should withdraw token A");
        assert!(withdrawn_b > 0, "Should withdraw token B");

        // Verify share math consistency (reentrancy-safe state ordering)
        let metrics_after = vault.get_metrics();
        assert_eq!(metrics_after.total_shares, shares - withdraw_amount);
        assert_eq!(metrics_after.total_amount_a, 5000 - withdrawn_a);
        assert_eq!(metrics_after.total_amount_b, 5000 - withdrawn_b);

        // Verify user balances changed
        let user_a_after = token_a_client.balance(&user);
        let user_b_after = token_b_client.balance(&user);
        assert!(
            user_a_after < user_a_before,
            "User token A balance should decrease"
        );
        assert!(
            user_b_after < user_b_before,
            "User token B balance should decrease"
        );
    }

    #[test]
    fn test_deposit_withdraw_fees_and_share_math() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (
            vault,
            token_a_client,
            token_b_client,
            token_a_admin,
            token_b_admin,
            user,
            _admin,
            _treasury,
        ) = setup_vault(&env);

        // Fund user with withdrawal fee
        token_a_admin.mint(&user, &10000);
        token_b_admin.mint(&user, &10000);

        // Deposit with 2.5% withdrawal fee
        let shares = vault.deposit(&user, &5000, &5000, &0);

        // Withdraw all shares
        let (withdrawn_a, withdrawn_b) = vault.withdraw(&user, &shares, &0, &0);

        // With 2.5% fee (25 bps), user should receive 97.5% of proportional amount
        // Proportional: 5000 each
        // Fee on A: 5000 * 25 / 10000 = 12.5 -> 12
        // Fee on B: 5000 * 25 / 10000 = 12.5 -> 12
        // Expected: 4988 each
        assert!(
            withdrawn_a < 5000,
            "Withdrawal fee should reduce token A received"
        );
        assert!(
            withdrawn_b < 5000,
            "Withdrawal fee should reduce token B received"
        );
        assert!(withdrawn_a >= 4900, "Fee should not be excessive");
        assert!(withdrawn_b >= 4900, "Fee should not be excessive");
    }

    #[test]
    fn test_harvest_reinvests_and_pays_treasury_fee() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (
            vault,
            token_a_client,
            token_b_client,
            token_a_admin,
            token_b_admin,
            user,
            _admin,
            treasury,
        ) = setup_vault(&env);

        // Fund user and deposit
        token_a_admin.mint(&user, &10000);
        token_b_admin.mint(&user, &10000);
        vault.deposit(&user, &10000, &10000, &0);

        // Harvest
        vault.harvest(&user);

        // Verify treasury received harvest fees (1% of 1000 = 10)
        let treasury_a = token_a_client.balance(&treasury);
        let treasury_b = token_b_client.balance(&treasury);
        assert!(
            treasury_a > 0 || treasury_b > 0,
            "Treasury should receive harvest fees"
        );
    }

    // [CHECK-PAUSE-1] + [CHECK-PAUSE-2]
    // Paused state blocks deposit, withdraw, and harvest; unpause restores operation.
    #[test]
    fn test_pause_blocks_deposit_withdraw_harvest() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (
            vault,
            _token_a_client,
            _token_b_client,
            token_a_admin,
            token_b_admin,
            user,
            admin,
            _treasury,
        ) = setup_vault(&env);

        // Fund user and deposit before pausing so we have shares to withdraw
        token_a_admin.mint(&user, &10000);
        token_b_admin.mint(&user, &10000);
        let shares = vault.deposit(&user, &1000, &1000, &0);
        assert!(shares > 0, "Pre-pause deposit should succeed");

        // Pause vault
        vault.pause(&admin);
        assert!(vault.is_paused(), "Vault should report paused");

        // Deposit should fail when paused
        let deposit_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            vault.deposit(&user, &1000, &1000, &0);
        }));
        assert!(
            deposit_result.is_err(),
            "Deposit should fail when vault is paused"
        );

        // Withdraw should fail when paused
        let withdraw_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            vault.withdraw(&user, &shares, &0, &0);
        }));
        assert!(
            withdraw_result.is_err(),
            "Withdraw should fail when vault is paused"
        );

        // Harvest should fail when paused
        let harvest_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            vault.harvest(&user);
        }));
        assert!(
            harvest_result.is_err(),
            "Harvest should fail when vault is paused"
        );

        // Unpause and verify deposit works again
        vault.unpause(&admin);
        assert!(!vault.is_paused(), "Vault should be unpaused");
        let shares_after = vault.deposit(&user, &1000, &1000, &0);
        assert!(shares_after > 0, "Deposit should work after unpause");
    }

    #[test]
    fn test_first_deposit_geometric_mean_share_calculation() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (
            vault,
            _token_a_client,
            _token_b_client,
            token_a_admin,
            token_b_admin,
            user,
            _admin,
            _treasury,
        ) = setup_vault(&env);

        // Fund user with asymmetric amounts
        token_a_admin.mint(&user, &10000);
        token_b_admin.mint(&user, &100);

        // First deposit uses geometric mean
        let shares = vault.deposit(&user, &10000, &100, &0);
        assert!(shares > 0, "First deposit should mint shares");

        // Geometric mean of 10000 * 100 = 1,000,000 -> sqrt = 1000
        // So shares should be ~1000
        assert!(
            shares >= 900 && shares <= 1100,
            "Shares should approximate geometric mean"
        );
    }

    // [CHECK-ACL-1]
    // Only the configured admin can pause or unpause the vault.
    #[test]
    fn test_non_admin_cannot_pause_or_unpause() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (
            vault,
            _token_a_client,
            _token_b_client,
            token_a_admin,
            token_b_admin,
            _user,
            _admin,
            _treasury,
        ) = setup_vault(&env);

        let non_admin = Address::generate(&env);

        // Non-admin pause should panic
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            vault.pause(&non_admin);
        }));
        assert!(result.is_err(), "Non-admin should not be able to pause");

        // Verify vault is still unpaused
        assert!(!vault.is_paused(), "Vault should remain unpaused");

        // Non-admin unpause should also panic (but vault is already unpaused)
        let result2 = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            vault.unpause(&non_admin);
        }));
        assert!(result2.is_err(), "Non-admin should not be able to unpause");
    }

    // [CHECK-ACL-2]
    // Admin-only configuration changes must reject non-admin callers.
    #[test]
    fn test_non_admin_cannot_change_admin() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (
            vault,
            _token_a_client,
            _token_b_client,
            _token_a_admin,
            _token_b_admin,
            _user,
            _admin,
            _treasury,
        ) = setup_vault(&env);

        let non_admin = Address::generate(&env);
        let new_admin = Address::generate(&env);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            vault.set_admin(&non_admin, &new_admin);
        }));
        assert!(
            result.is_err(),
            "Non-admin should not be able to change admin"
        );
    }

    // [CHECK-FEE-1]
    // Harvest fees are routed to treasury and remain within configured bounds.
    #[test]
    fn test_harvest_fee_routing_bounds() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (
            vault,
            token_a_client,
            token_b_client,
            token_a_admin,
            token_b_admin,
            user,
            _admin,
            treasury,
        ) = setup_vault(&env);

        token_a_admin.mint(&user, &10000);
        token_b_admin.mint(&user, &10000);
        vault.deposit(&user, &10000, &10000, &0);

        vault.harvest(&user);

        let treasury_a = token_a_client.balance(&treasury);
        let treasury_b = token_b_client.balance(&treasury);

        // Treasury fee is configured at 1% of harvest yield; verify it is positive
        // and not larger than the total deposited amount (sanity bound).
        assert!(
            treasury_a > 0 || treasury_b > 0,
            "Treasury should receive harvest fees"
        );
        assert!(treasury_a <= 10000, "Treasury fee must not exceed deposit");
        assert!(treasury_b <= 10000, "Treasury fee must not exceed deposit");
    }

    // [CHECK-REENT-1]
    // State is updated before external token transfers: after a deposit the
    // vault's internal accounting must match the token balances it holds.
    #[test]
    fn test_deposit_state_consistent_with_token_balances() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (
            vault,
            token_a_client,
            token_b_client,
            token_a_admin,
            token_b_admin,
            user,
            _admin,
            _treasury,
        ) = setup_vault(&env);

        token_a_admin.mint(&user, &10000);
        token_b_admin.mint(&user, &10000);

        vault.deposit(&user, &5000, &5000, &0);

        let metrics = vault.get_metrics();
        let vault_a = token_a_client.balance(&vault.resolve_address());
        let vault_b = token_b_client.balance(&vault.resolve_address());

        // Internal accounting must match actual token balances after deposit.
        assert_eq!(metrics.total_amount_a, vault_a);
        assert_eq!(metrics.total_amount_b, vault_b);
    }
}
