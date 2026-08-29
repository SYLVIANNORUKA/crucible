//! # Liquid Staking Protocol for Soroban
//!
//! Production requirement: Multi-Asset Liquidity Staking Derivative (LSD) Protocol
//!
//! This protocol implements:
//! - Validator stake pooling and reward distribution
//! - sXLM minting (liquid staking derivative)
//! - Unbonding queue with cooldown periods
//! - Multi-epoch reward compounding

use soroban_sdk::{contract, contractimpl, token, Address, Env, Symbol, Vec, Map, U256, I256, TryFromVal};
use std::cmp::Ordering;

/// Validator information
#[derive(Clone)]
pub struct Validator {
    pub address: Address,
    pub stake_amount: u64,
    pub total_rewards: u64,
    pub is_active: bool,
}

/// User stake position
#[derive(Clone)]
pub struct StakePosition {
    pub user: Address,
    pub amount: u64,              // Staked amount (XLM)
    pub shares: u64,              // sXLM shares owned
    pub last_claim_epoch: u32,
}

/// Unbonding request
#[derive(Clone)]
pub struct UnbondRequest {
    pub user: Address,
    pub amount: u64,              // Amount to unbond
    pub shares: u64,              // Shares being burned
    pub request_epoch: u32,
    pub available_epoch: u32,     // When funds become available
}

/// Global staking state
#[derive(Clone)]
pub struct StakingState {
    pub total_staked: u64,              // Total XLM staked
    pub total_shares: u64,              // Total sXLM shares outstanding
    pub exchange_rate_numerator: u64,   // XLM per share (scaled)
    pub exchange_rate_denominator: u64,
    pub current_epoch: u32,
    pub epoch_rewards: u64,
    pub unbonding_queue: Vec<UnbondRequest>,
}

/// Epoch configuration
#[derive(Clone)]
pub struct EpochConfig {
    pub duration_blocks: u32,
    pub reward_percentage: u64,   // Expected APR as percentage
    pub unbonding_period: u32,    // Epochs until withdrawal
}

#[contract]
pub struct LiquidStaking;

#[contractimpl]
impl LiquidStaking {
    /// Initialize the staking protocol
    #[soroban_sdk::export_fn]
    pub fn initialize(
        env: Env,
        underlying_token: Address,
        reward_token: Address,
        operator: Address,
        unbonding_period_epochs: u32,
    ) -> StakingState {
        let state = StakingState {
            total_staked: 0,
            total_shares: 0,
            exchange_rate_numerator: 1_000_000, // 1:1 initially (6 decimals)
            exchange_rate_denominator: 1_000_000,
            current_epoch: 0,
            epoch_rewards: 0,
            unbonding_queue: Vec::new(&env),
        };
        
        // Store state
        let state_key = Symbol::new(&env, "state");
        env.storage().set(&state_key, &state);
        
        // Store config
        let config = EpochConfig {
            duration_blocks: 17280, // ~24 hours at 5 sec blocks
            reward_percentage: 500, // 5% APR
            unbonding_period: unbonding_period_epochs,
        };
        let config_key = Symbol::new(&env, "config");
        env.storage().set(&config_key, &config);
        
        // Store addresses
        let underlying_key = Symbol::new(&env, "underlying");
        env.storage().set(&underlying_key, &underlying_token);
        
        let reward_key = Symbol::new(&env, "reward_token");
        env.storage().set(&reward_key, &reward_token);
        
        let operator_key = Symbol::new(&env, "operator");
        env.storage().set(&operator_key, &operator);
        
        // Initialize validators map
        let validators_key = Symbol::new(&env, "validators");
        env.storage().set(&validators_key, &Map::<Address, Validator>::new(&env));
        
        // Initialize stake positions
        let stakes_key = Symbol::new(&env, "stakes");
        env.storage().set(&stakes_key, &Map::<Address, StakePosition>::new(&env));
        
        // Initialize unbonding requests
        let unbond_key = Symbol::new(&env, "unbond_requests");
        env.storage().set(&unbond_key, &Map::<u64, UnbondRequest>::new(&env));
        
        let unbond_count_key = Symbol::new(&env, "unbond_count");
        env.storage().set(&unbond_count_key, &0u64);
        
        // Initialize total shares
        let shares_key = Symbol::new(&env, "total_shares");
        env.storage().set(&shares_key, &0u64);
        
        state
    }

    /// Get current state
    pub fn get_state(env: Env) -> StakingState {
        let state_key = Symbol::new(&env, "state");
        env.storage().get_unchecked::<Symbol, StakingState>(&state_key).unwrap()
    }

    /// Get configuration
    pub fn get_config(env: Env) -> EpochConfig {
        let config_key = Symbol::new(&env, "config");
        env.storage().get_unchecked::<Symbol, EpochConfig>(&config_key).unwrap()
    }

    /// Deposit XLM and mint sXLM
    #[soroban_sdk::export_fn]
    pub fn deposit(env: Env, user: Address, amount: u64) -> u64 {
        assert!(amount > 0, "Amount must be positive");
        
        let state = Self::get_state(env.clone());
        let config = Self::get_config(env.clone());
        
        // Calculate shares to mint: shares = amount * exchange_rate
        // shares = amount * (total_shares / total_staked)
        // New user gets: shares = amount * total_shares / total_staked
        // For first depositor: shares = amount
        let shares = if state.total_staked == 0 {
            amount
        } else {
            (amount as u128 * state.total_shares as u128 / state.total_staked as u128) as u64
        };
        
        assert!(shares > 0, "Would receive zero shares");
        
        // Update state
        let mut new_state = state.clone();
        new_state.total_staked += amount;
        new_state.total_shares += shares;
        
        let state_key = Symbol::new(&env, "state");
        env.storage().set(&state_key, &new_state);
        
        // Update stake position
        let stakes_key = Symbol::new(&env, "stakes");
        let mut stakes: Map<Address, StakePosition> = env.storage().get(&stakes_key).unwrap_or(Map::new(&env));
        
        let mut position = stakes.get(user.clone()).unwrap_or(StakePosition {
            user: user.clone(),
            amount: 0,
            shares: 0,
            last_claim_epoch: state.current_epoch,
        });
        
        position.amount += amount;
        position.shares += shares;
        stakes.set(user.clone(), position);
        env.storage().set(&stakes_key, &stakes);
        
        // Track total shares
        let shares_key = Symbol::new(&env, "total_shares");
        env.storage().set(&shares_key, &new_state.total_shares);
        
        shares
    }

    /// Request unbonding - burns sXLM and queues for withdrawal
    #[soroban_sdk::export_fn]
    pub fn request_unbond(env: Env, user: Address, shares: u64) -> u64 {
        assert!(shares > 0, "Shares must be positive");
        
        let config = Self::get_config(env.clone());
        let state = Self::get_state(env.clone());
        
        // Check user has enough shares
        let stakes_key = Symbol::new(&env, "stakes");
        let mut stakes: Map<Address, StakePosition> = env.storage().get(&stakes_key).unwrap_or(Map::new(&env));
        
        let mut position = stakes.get(user.clone()).expect("No stake position");
        assert!(position.shares >= shares, "Insufficient shares");
        
        // Calculate amount to receive: amount = shares * exchange_rate
        let amount = (shares as u128 * state.exchange_rate_numerator as u128 / state.exchange_rate_denominator as u128) as u64;
        
        // Create unbonding request
        let unbond_count_key = Symbol::new(&env, "unbond_count");
        let mut unbond_count: u64 = env.storage().get(&unbond_count_key).unwrap_or(0u64);
        unbond_count += 1;
        env.storage().set(&unbond_count_key, &unbond_count);
        
        let unbond_request = UnbondRequest {
            user: user.clone(),
            amount,
            shares,
            request_epoch: state.current_epoch,
            available_epoch: state.current_epoch + config.unbonding_period,
        };
        
        // Store unbond request
        let unbond_key = Symbol::new(&env, "unbond_requests");
        let mut unbond_requests: Map<u64, UnbondRequest> = env.storage().get(&unbond_key).unwrap_or(Map::new(&env));
        unbond_requests.set(unbond_count, unbond_request.clone());
        env.storage().set(&unbond_key, &unbond_requests);
        
        // Add to global queue
        let state_key = Symbol::new(&env, "state");
        let mut current_state = Self::get_state(env.clone());
        current_state.unbonding_queue.push_back(unbond_request);
        current_state.total_shares -= shares;
        current_state.total_staked -= amount;
        env.storage().set(&state_key, &current_state);
        
        // Update user position
        position.shares -= shares;
        position.amount -= amount;
        stakes.set(user.clone(), position);
        env.storage().set(&stakes_key, &stakes);
        
        // Update total shares
        let shares_key = Symbol::new(&env, "total_shares");
        env.storage().set(&shares_key, &current_state.total_shares);
        
        unbond_count
    }

    /// Process unbonding - claim available funds after cooldown
    #[soroban_sdk::export_fn]
    pub fn claim_unbonded(env: Env, user: Address, unbond_id: u64) -> u64 {
        let unbond_key = Symbol::new(&env, "unbond_requests");
        let mut unbond_requests: Map<u64, UnbondRequest> = env.storage().get(&unbond_key).unwrap_or(Map::new(&env));
        
        if let Some(request) = unbond_requests.get(unbond_id) {
            assert_eq!(request.user, user, "Not request owner");
            assert!(request.available_epoch <= Self::get_state(env.clone()).current_epoch, "Cooldown not complete");
            
            // Remove request
            unbond_requests.remove(&unbond_id);
            env.storage().set(&unbond_key, &unbond_requests);
            
            // Update queue
            let state_key = Symbol::new(&env, "state");
            let mut state = Self::get_state(env.clone());
            
            let mut new_queue = Vec::new(&env);
            for req in state.unbonding_queue.iter() {
                if req.user != user || req.amount != request.amount || req.shares != request.shares {
                    new_queue.push_back(req.clone());
                }
            }
            state.unbonding_queue = new_queue;
            env.storage().set(&state_key, &state);
            
            request.amount
        } else {
            0
        }
    }

    /// Distribute staking rewards to validators
    #[soroban_sdk::export_fn]
    pub fn distribute_rewards(env: Env, operator: Address, reward_amount: u64) -> u64 {
        let operator_key = Symbol::new(&env, "operator");
        let stored_operator: Address = env.storage().get(&operator_key).unwrap();
        assert_eq!(operator, stored_operator, "Not operator");
        
        let mut state = Self::get_state(env.clone());
        state.epoch_rewards = reward_amount;
        
        // Update exchange rate: rewards are distributed proportionally
        // New exchange rate = (total_staked + rewards) / total_shares
        // This increases the value of existing shares
        if state.total_shares > 0 {
            let total_value = state.total_staked + reward_amount;
            state.exchange_rate_numerator = total_value;
            state.exchange_rate_denominator = state.total_shares;
        }
        
        let state_key = Symbol::new(&env, "state");
        env.storage().set(&state_key, &state);
        
        // Update each stake position's last claim epoch
        let stakes_key = Symbol::new(&env, "stakes");
        let stakes: Map<Address, StakePosition> = env.storage().get(&stakes_key).unwrap_or(Map::new(&env));
        
        for (_, mut position) in stakes.iter() {
            position.last_claim_epoch = state.current_epoch;
        }
        env.storage().set(&stakes_key, &stakes);
        
        reward_amount
    }

    /// Advance epoch (called by operator after reward distribution)
    #[soroban_sdk::export_fn]
    pub fn advance_epoch(env: Env, operator: Address) -> u32 {
        let operator_key = Symbol::new(&env, "operator");
        let stored_operator: Address = env.storage().get(&operator_key).unwrap();
        assert_eq!(operator, stored_operator, "Not operator");
        
        let mut state = Self::get_state(env.clone());
        let config = Self::get_config(env.clone());
        
        state.current_epoch += 1;
        
        // Check unbonding requests that are now available
        let unbond_key = Symbol::new(&env, "unbond_requests");
        let mut unbond_requests: Map<u64, UnbondRequest> = env.storage().get(&unbond_key).unwrap_or(Map::new(&env));
        
        // Process matured unbonding requests
        let mut to_remove: Vec<u64> = Vec::new(&env);
        for (id, request) in unbond_requests.iter() {
            if request.available_epoch == state.current_epoch {
                // Request is now claimable
                to_remove.push_back(id);
            }
        }
        
        for id in to_remove.iter() {
            unbond_requests.remove(id);
        }
        env.storage().set(&unbond_key, &unbond_requests);
        
        let state_key = Symbol::new(&env, "state");
        env.storage().set(&state_key, &state);
        
        state.current_epoch
    }

    /// Get sXLM balance for user
    #[soroban_sdk::export_fn]
    pub fn get_shares(env: Env, user: Address) -> u64 {
        let stakes_key = Symbol::new(&env, "stakes");
        let stakes: Map<Address, StakePosition> = env.storage().get(&stakes_key).unwrap_or(Map::new(&env));
        
        if let Some(position) = stakes.get(user) {
            position.shares
        } else {
            0
        }
    }

    /// Get XLM value of shares
    #[soroban_sdk::export_fn]
    pub fn get_value(env: Env, user: Address) -> u64 {
        let state = Self::get_state(env.clone());
        let shares = Self::get_shares(env.clone(), user);
        
        (shares as u128 * state.exchange_rate_numerator as u128 / state.exchange_rate_denominator as u128) as u64
    }

    /// Get current exchange rate (sXLM to XLM)
    #[soroban_sdk::export_fn]
    pub fn get_exchange_rate(env: Env) -> (u64, u64) {
        let state = Self::get_state(env.clone());
        (state.exchange_rate_numerator, state.exchange_rate_denominator)
    }

    /// Get pending unbonding requests for user
    #[soroban_sdk::export_fn]
    pub fn get_unbond_requests(env: Env, user: Address) -> Vec<UnbondRequest> {
        let unbond_key = Symbol::new(&env, "unbond_requests");
        let unbond_requests: Map<u64, UnbondRequest> = env.storage().get(&unbond_key).unwrap_or(Map::new(&env));
        
        let mut result = Vec::new(&env);
        for (_, request) in unbond_requests.iter() {
            if request.user == user {
                result.push_back(request);
            }
        }
        result
    }

    /// Add validator to pool
    #[soroban_sdk::export_fn]
    pub fn add_validator(env: Env, operator: Address, validator: Address) -> bool {
        let operator_key = Symbol::new(&env, "operator");
        let stored_operator: Address = env.storage().get(&operator_key).unwrap();
        assert_eq!(operator, stored_operator, "Not operator");
        
        let validators_key = Symbol::new(&env, "validators");
        let mut validators: Map<Address, Validator> = env.storage().get(&validators_key).unwrap_or(Map::new(&env));
        
        if validators.contains_key(&validator) {
            return false;
        }
        
        let new_validator = Validator {
            address: validator.clone(),
            stake_amount: 0,
            total_rewards: 0,
            is_active: true,
        };
        
        validators.set(validator, new_validator);
        env.storage().set(&validators_key, &validators);
        
        true
    }

    /// Get total staked amount
    #[soroban_sdk::export_fn]
    pub fn get_total_staked(env: Env) -> u64 {
        let state = Self::get_state(env.clone());
        state.total_staked
    }

    /// Get total sXLM supply
    #[soroban_sdk::export_fn]
    pub fn get_total_shares(env: Env) -> u64 {
        let shares_key = Symbol::new(&env, "total_shares");
        env.storage().get(&shares_key).unwrap_or(0u64)
    }
}

/// Testing module
#[cfg(test)]
mod test {
    use super::*;
    
    #[test]
    fn test_initial_exchange_rate() {
        let env = Env::default();
        let user = Address::random(&env);
        let token = Address::random(&env);
        let reward = Address::random(&env);
        let operator = Address::random(&env);
        
        LiquidStaking::initialize(env.clone(), token.clone(), reward.clone(), operator.clone(), 5);
        
        // Initial exchange rate should be 1:1
        let (num, den) = LiquidStaking::get_exchange_rate(env.clone());
        assert_eq!(num, den);
    }
    
    #[test]
    fn test_deposit_mints_shares() {
        let env = Env::default();
        let user = Address::random(&env);
        let token = Address::random(&env);
        let reward = Address::random(&env);
        let operator = Address::random(&env);
        
        LiquidStaking::initialize(env.clone(), token.clone(), reward.clone(), operator.clone(), 5);
        
        // Deposit 1000 XLM
        let shares = LiquidStaking::deposit(env.clone(), user.clone(), 1000);
        assert_eq!(shares, 1000);
        
        // Check shares balance
        let balance = LiquidStaking::get_shares(env.clone(), user.clone());
        assert_eq!(balance, 1000);
    }
    
    #[test]
    fn test_unbonding_cooldown() {
        let env = Env::default();
        let user = Address::random(&env);
        let token = Address::random(&env);
        let reward = Address::random(&env);
        let operator = Address::random(&env);
        
        LiquidStaking::initialize(env.clone(), token.clone(), reward.clone(), operator.clone(), 3);
        
        // Deposit
        let shares = LiquidStaking::deposit(env.clone(), user.clone(), 1000);
        
        // Request unbond
        let unbond_id = LiquidStaking::request_unbond(env.clone(), user.clone(), shares);
        
        // Cannot claim immediately
        let claimable = LiquidStaking::claim_unbonded(env.clone(), user.clone(), unbond_id);
        assert_eq!(claimable, 0);
        
        // Advance epochs
        for _ in 0..3 {
            LiquidStaking::advance_epoch(env.clone(), operator.clone());
        }
        
        // Now can claim
        let claimable = LiquidStaking::claim_unbonded(env.clone(), user.clone(), unbond_id);
        assert!(claimable > 0);
    }
    
    #[test]
    fn test_reward_compounding() {
        let env = Env::default();
        let user = Address::random(&env);
        let token = Address::random(&env);
        let reward = Address::random(&env);
        let operator = Address::random(&env);
        
        LiquidStaking::initialize(env.clone(), token.clone(), reward.clone(), operator.clone(), 1);
        
        // Deposit
        LiquidStaking::deposit(env.clone(), user.clone(), 1000);
        
        // Get initial value
        let initial_value = LiquidStaking::get_value(env.clone(), user.clone());
        assert_eq!(initial_value, 1000);
        
        // Distribute rewards
        LiquidStaking::distribute_rewards(env.clone(), operator.clone(), 100);
        
        // Value should increase
        let new_value = LiquidStaking::get_value(env.clone(), user.clone());
        assert!(new_value > initial_value);
        
        // Exchange rate should change
        let (num, den) = LiquidStaking::get_exchange_rate(env.clone());
        assert_eq!(num, 1100); // 1000 + 100 rewards
        assert_eq!(den, 1000); // Still 1000 shares
    }
    
    #[test]
    fn test_multi_user_compounding() {
        let env = Env::default();
        let user1 = Address::random(&env);
        let user2 = Address::random(&env);
        let token = Address::random(&env);
        let reward = Address::random(&env);
        let operator = Address::random(&env);
        
        LiquidStaking::initialize(env.clone(), token.clone(), reward.clone(), operator.clone(), 1);
        
        // User1 deposits 1000, user2 deposits 500
        LiquidStaking::deposit(env.clone(), user1.clone(), 1000);
        LiquidStaking::deposit(env.clone(), user2.clone(), 500);
        
        // Both should have proportional shares
        let user1_shares = LiquidStaking::get_shares(env.clone(), user1.clone());
        let user2_shares = LiquidStaking::get_shares(env.clone(), user2.clone());
        assert_eq!(user1_shares * 2, user2_shares * 4); // 1000:500 = 2:1 ratio
        
        // Distribute 150 rewards (10% of total)
        LiquidStaking::distribute_rewards(env.clone(), operator.clone(), 150);
        
        // Total value should be 1500 + 150 = 1650
        let total_staked = LiquidStaking::get_total_staked(env.clone());
        assert_eq!(total_staked, 1650);
        
        // Each user's value should increase proportionally
        let user1_value = LiquidStaking::get_value(env.clone(), user1.clone());
        let user2_value = LiquidStaking::get_value(env.clone(), user2.clone());
        assert_eq!(user1_value * 2, user2_value * 4); // Same 2:1 ratio
    }
    
    #[test]
    fn test_validator_management() {
        let env = Env::default();
        let token = Address::random(&env);
        let reward = Address::random(&env);
        let operator = Address::random(&env);
        let validator = Address::random(&env);
        
        LiquidStaking::initialize(env.clone(), token.clone(), reward.clone(), operator.clone(), 5);
        
        // Add validator
        let result = LiquidStaking::add_validator(env.clone(), operator.clone(), validator.clone());
        assert!(result);
        
        // Duplicate should fail
        let result = LiquidStaking::add_validator(env.clone(), operator.clone(), validator.clone());
        assert!(!result);
    }
}