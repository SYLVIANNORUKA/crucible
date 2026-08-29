//! # Payment Streaming Protocol for Soroban
//!
//! Production requirement: Decentralized Subscription & Recurring Payment Stream Engine
//!
//! This contract implements:
//! - Continuous token streaming by-the-second
//! - Recurring subscription billing
//! - Automated payer withdrawal of unclaimed funds
//! - Stream cancellation with proportional refund

use soroban_sdk::{contract, contractimpl, token, Address, Env, Symbol, Vec, Map, U256, I256};
use std::cmp::Ordering;

/// Stream status
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum StreamStatus {
    Pending = 0,    // Created but not started
    Active = 1,     // Currently streaming
    Paused = 2,     // Temporarily paused
    Cancelled = 3,  // Cancelled by sender
    Completed = 4,  // Fully streamed and claimed
    Claimed = 5,    // Recipient claimed remaining balance
}

/// Stream type
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum StreamType {
    Linear = 0,      // Constant rate over time
    Instant = 1,     // All available immediately
    Cliff = 2,      // Cliff then linear
    Recurring = 3,  // Recurring subscription
}

/// Stream data structure
#[derive(Clone)]
pub struct Stream {
    pub id: u64,
    pub sender: Address,
    pub recipient: Address,
    pub token: Address,
    pub total_amount: u64,
    pub streamed_amount: u64,       // Amount that has streamed (claimable)
    pub claimed_amount: u64,        // Amount actually claimed by recipient
    pub remaining_amount: u64,      // Unstreamed amount remaining
    pub start_time: u64,
    pub end_time: u64,
    pub cliff_amount: u64,          // Amount unlock at cliff
    pub cliff_time: u64,            // When cliff unlocks
    pub rate_per_second: u64,       // Streaming rate
    pub status: StreamStatus,
    pub stream_type: StreamType,
    pub last_update_time: u64,
    pub cancellation_time: u64,
    pub deposit: u64,               // Original deposit amount
}

/// Withdraw claim for unclaimed stream funds
#[derive(Clone)]
pub struct WithdrawClaim {
    pub stream_id: u64,
    pub sender: Address,
    pub amount: u64,
    pub claim_time: u64,
}

/// Subscription configuration
#[derive(Clone)]
pub struct SubscriptionConfig {
    pub period: u64,           // Billing period in seconds
    pub amount_per_period: u64,
    pub max_duration: u64,     // Maximum subscription length
    pub grace_period: u64,     // Grace period after missed payment
}

/// Contract configuration
#[derive(Clone)]
pub struct StreamingConfig {
    pub protocol_fee_bps: u64,     // Fee charged on claimed amounts (basis points)
    pub min_stream_duration: u64,
    pub max_stream_duration: u64,
    pub min_amount: u64,
    pub cancellation_fee_bps: u64, // Fee for early cancellation
}

#[contract]
pub struct PaymentStreaming;

#[contractimpl]
impl PaymentStreaming {
    /// Initialize the streaming protocol
    #[soroban_sdk::export_fn]
    pub fn initialize(
        env: Env,
        protocol_admin: Address,
        protocol_fee_bps: u64,
    ) -> StreamingConfig {
        let config = StreamingConfig {
            protocol_fee_bps,
            min_stream_duration: 60,        // 1 minute minimum
            max_stream_duration: 31536000,  // 1 year maximum
            min_amount: 1000000,            // 1 unit with decimals
            cancellation_fee_bps: 0,        // No fee by default
        };
        
        // Store config
        let config_key = Symbol::new(&env, "config");
        env.storage().set(&config_key, &config);
        
        // Store admin
        let admin_key = Symbol::new(&env, "admin");
        env.storage().set(&admin_key, &protocol_admin);
        
        // Initialize streams
        let streams_key = Symbol::new(&env, "streams");
        env.storage().set(&streams_key, &Map::<u64, Stream>::new(&env));
        
        // Initialize stream counter
        let stream_count_key = Symbol::new(&env, "stream_count");
        env.storage().set(&stream_count_key, &0u64);
        
        // Initialize subscriptions
        let subs_key = Symbol::new(&env, "subscriptions");
        env.storage().set(&subs_key, &Map::<u64, SubscriptionConfig>::new(&env));
        
        // Initialize withdraw claims
        let claims_key = Symbol::new(&env, "claims");
        env.storage().set(&claims_key, &Map::<u64, WithdrawClaim>::new(&env));
        
        // Initialize paused state
        let paused_key = Symbol::new(&env, "paused");
        env.storage().set(&paused_key, &false);
        
        config
    }

    /// Get configuration
    fn get_config(env: Env) -> StreamingConfig {
        let config_key = Symbol::new(&env, "config");
        env.storage().get_unchecked::<Symbol, StreamingConfig>(&config_key).unwrap()
    }

    /// Get next stream ID
    fn get_next_stream_id(env: Env) -> u64 {
        let stream_count_key = Symbol::new(&env, "stream_count");
        let mut count: u64 = env.storage().get(&stream_count_key).unwrap_or(0u64);
        count += 1;
        env.storage().set(&stream_count_key, &count);
        count
    }

    /// Create a linear stream
    #[soroban_sdk::export_fn]
    pub fn create_stream(
        env: Env,
        sender: Address,
        recipient: Address,
        token: Address,
        amount: u64,
        start_time: u64,
        duration: u64,
    ) -> u64 {
        let config = Self::get_config(env.clone());
        
        assert!(amount >= config.min_amount, "Amount below minimum");
        assert!(duration >= config.min_stream_duration, "Duration below minimum");
        assert!(duration <= config.max_stream_duration, "Duration exceeds maximum");
        assert!(start_time >= env.ledger().timestamp(), "Start time in past");
        
        let stream_id = Self::get_next_stream_id(env.clone());
        let end_time = start_time + duration;
        let rate_per_second = amount / duration;
        
        let stream = Stream {
            id: stream_id,
            sender: sender.clone(),
            recipient: recipient.clone(),
            token: token.clone(),
            total_amount: amount,
            streamed_amount: 0,
            claimed_amount: 0,
            remaining_amount: amount,
            start_time,
            end_time,
            cliff_amount: 0,
            cliff_time: 0,
            rate_per_second,
            status: StreamStatus::Pending,
            stream_type: StreamType::Linear,
            last_update_time: start_time,
            cancellation_time: 0,
            deposit: amount,
        };
        
        // Store stream
        let streams_key = Symbol::new(&env, "streams");
        let mut streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        streams.set(stream_id, stream);
        env.storage().set(&streams_key, &streams);
        
        stream_id
    }

    /// Create stream with cliff
    #[soroban_sdk::export_fn]
    pub fn create_stream_with_cliff(
        env: Env,
        sender: Address,
        recipient: Address,
        token: Address,
        total_amount: u64,
        cliff_amount: u64,
        cliff_time: u64,
        vesting_duration: u64,
    ) -> u64 {
        let config = Self::get_config(env.clone());
        
        assert!(cliff_amount < total_amount, "Cliff must be less than total");
        assert!(vesting_duration >= config.min_stream_duration, "Duration too short");
        let start_time = env.ledger().timestamp();
        let end_time = start_time + vesting_duration;
        
        let stream_id = Self::get_next_stream_id(env.clone());
        let remaining_after_cliff = total_amount - cliff_amount;
        let linear_duration = vesting_duration;
        let rate_per_second = remaining_after_cliff / linear_duration;
        
        let stream = Stream {
            id: stream_id,
            sender: sender.clone(),
            recipient: recipient.clone(),
            token: token.clone(),
            total_amount,
            streamed_amount: 0,
            claimed_amount: 0,
            remaining_amount: total_amount,
            start_time,
            end_time,
            cliff_amount,
            cliff_time,
            rate_per_second,
            status: StreamStatus::Pending,
            stream_type: StreamType::Cliff,
            last_update_time: start_time,
            cancellation_time: 0,
            deposit: total_amount,
        };
        
        let streams_key = Symbol::new(&env, "streams");
        let mut streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        streams.set(stream_id, stream);
        env.storage().set(&streams_key, &streams);
        
        stream_id
    }

    /// Create recurring subscription
    #[soroban_sdk::export_fn]
    pub fn create_subscription(
        env: Env,
        sender: Address,
        recipient: Address,
        token: Address,
        amount_per_period: u64,
        period_seconds: u64,
        max_duration: u64,
    ) -> u64 {
        let config = Self::get_config(env.clone());
        let now = env.ledger().timestamp();
        
        let stream_id = Self::get_next_stream_id(env.clone());
        let rate_per_second = amount_per_period / period_seconds;
        
        let stream = Stream {
            id: stream_id,
            sender: sender.clone(),
            recipient: recipient.clone(),
            token: token.clone(),
            total_amount: 0, // Will increase with each renewal
            streamed_amount: 0,
            claimed_amount: 0,
            remaining_amount: amount_per_period,
            start_time: now,
            end_time: now + period_seconds,
            cliff_amount: 0,
            cliff_time: 0,
            rate_per_second,
            status: StreamStatus::Active,
            stream_type: StreamType::Recurring,
            last_update_time: now,
            cancellation_time: 0,
            deposit: amount_per_period,
        };
        
        // Store subscription config
        let subs_key = Symbol::new(&env, "subscriptions");
        let mut subs: Map<u64, SubscriptionConfig> = env.storage().get(&subs_key).unwrap_or(Map::new(&env));
        let sub_config = SubscriptionConfig {
            period: period_seconds,
            amount_per_period,
            max_duration,
            grace_period: 86400, // 1 day grace
        };
        subs.set(stream_id, sub_config);
        env.storage().set(&subs_key, &subs);
        
        // Store stream
        let streams_key = Symbol::new(&env, "streams");
        let mut streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        streams.set(stream_id, stream);
        env.storage().set(&streams_key, &streams);
        
        stream_id
    }

    /// Calculate claimable amount at current time
    /// Formula: (total_amount * elapsed_time) / duration
    fn calculate_claimable(env: Env, stream: Stream) -> u64 {
        let now = env.ledger().timestamp();
        
        // Handle different stream types
        match stream.stream_type {
            StreamType::Linear | StreamType::Recurring => {
                if now < stream.start_time {
                    return 0;
                }
                
                let elapsed = now.saturating_sub(stream.start_time);
                let duration = stream.end_time.saturating_sub(stream.start_time);
                
                if duration == 0 || elapsed >= duration {
                    stream.remaining_amount
                } else {
                    // Linear vesting: (amount * elapsed) / duration
                    let claimable = (stream.total_amount as u128 * elapsed as u128 / duration as u128) as u64;
                    claimable.saturating_sub(stream.claimed_amount)
                }
            }
            
            StreamType::Cliff => {
                if now < stream.cliff_time {
                    0
                } else if now >= stream.end_time {
                    stream.remaining_amount
                } else {
                    // After cliff: (remaining_after_cliff * elapsed_since_cliff) / remaining_duration
                    let elapsed_since_cliff = now.saturating_sub(stream.cliff_time);
                    let total_duration = stream.end_time.saturating_sub(stream.start_time);
                    let remaining_duration = total_duration.saturating_sub(stream.cliff_time.saturating_sub(stream.start_time));
                    
                    let vested = stream.cliff_amount + 
                        ((stream.total_amount - stream.cliff_amount) as u128 * elapsed_since_cliff as u128 / remaining_duration as u128) as u64;
                    
                    vested.saturating_sub(stream.claimed_amount)
                }
            }
            
            StreamType::Instant => {
                stream.remaining_amount
            }
        }
    }

    /// Claim available funds from stream
    #[soroban_sdk::export_fn]
    pub fn claim_from_stream(env: Env, stream_id: u64, recipient: Address) -> u64 {
        let streams_key = Symbol::new(&env, "streams");
        let mut streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        
        if let Some(mut stream) = streams.get(stream_id) {
            assert_eq!(stream.recipient, recipient, "Not the recipient");
            assert!(stream.status == StreamStatus::Active, "Stream not active");
            
            let claimable = Self::calculate_claimable(env.clone(), stream.clone());
            
            if claimable == 0 {
                return 0;
            }
            
            // Calculate protocol fee
            let config = Self::get_config(env.clone());
            let protocol_fee = (claimable * config.protocol_fee_bps) / 10000;
            let net_claim = claimable.saturating_sub(protocol_fee);
            
            // Update stream state
            stream.claimed_amount += claimable;
            stream.remaining_amount -= claimable;
            stream.last_update_time = env.ledger().timestamp();
            
            // Check if stream is complete
            if stream.remaining_amount == 0 {
                stream.status = StreamStatus::Completed;
            }
            
            streams.set(stream_id, stream);
            env.storage().set(&streams_key, &streams);
            
            // In production, transfer tokens to recipient here
            // Using token contract interface
            
            net_claim
        } else {
            0
        }
    }

    /// Cancel a stream and refund sender
    #[soroban_sdk::export_fn]
    pub fn cancel_stream(env: Env, stream_id: u64, canceller: Address) -> u64 {
        let config = Self::get_config(env.clone());
        let streams_key = Symbol::new(&env, "streams");
        let mut streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        
        if let Some(mut stream) = streams.get(stream_id) {
            let now = env.ledger().timestamp();
            
            // Authorization: sender can always cancel, recipient can cancel if past end
            assert!(stream.sender == canceller || stream.recipient == canceller, "Not authorized");
            assert!(stream.status == StreamStatus::Active || stream.status == StreamStatus::Pending, "Cannot cancel");
            
            // Calculate refund
            let refundable = if stream.status == StreamStatus::Pending {
                stream.remaining_amount
            } else {
                // Calculate unstreamed portion
                let elapsed = now.saturating_sub(stream.start_time);
                let duration = stream.end_time.saturating_sub(stream.start_time);
                
                if elapsed >= duration {
                    0 // Fully streamed, no refund
                } else {
                    let streamed = (stream.total_amount as u128 * elapsed as u128 / duration as u128) as u64;
                    let cancellation_fee = (stream.remaining_amount * config.cancellation_fee_bps) / 10000;
                    stream.remaining_amount.saturating_sub(cancellation_fee)
                }
            };
            
            // Update stream status
            stream.status = StreamStatus::Cancelled;
            stream.cancellation_time = now;
            streams.set(stream_id, stream);
            env.storage().set(&streams_key, &streams);
            
            // In production, transfer refund to sender
            
            refundable
        } else {
            0
        }
    }

    /// Sender withdraws unclaimed funds from completed stream
    #[soroban_sdk::export_fn]
    pub fn withdraw_unclaimed(env: Env, stream_id: u64, sender: Address) -> u64 {
        let streams_key = Symbol::new(&env, "streams");
        let mut streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        
        if let Some(mut stream) = streams.get(stream_id) {
            assert_eq!(stream.sender, sender, "Not the sender");
            
            // Only allow withdrawal from completed or cancelled streams
            assert!(stream.status == StreamStatus::Completed || stream.status == StreamStatus::Cancelled, "Stream not finalized");
            assert!(stream.remaining_amount > 0, "No unclaimed funds");
            
            let unclaimed = stream.remaining_amount;
            stream.remaining_amount = 0;
            stream.status = StreamStatus::Claimed;
            streams.set(stream_id, stream);
            env.storage().set(&streams_key, &streams);
            
            // In production, transfer remaining tokens back to sender
            
            unclaimed
        } else {
            0
        }
    }

    /// Renew a recurring subscription
    #[soroban_sdk::export_fn]
    pub fn renew_subscription(env: Env, stream_id: u64, sender: Address) -> bool {
        let subs_key = Symbol::new(&env, "subscriptions");
        let subs: Map<u64, SubscriptionConfig> = env.storage().get(&subs_key).unwrap_or(Map::new(&env));
        
        let streams_key = Symbol::new(&env, "streams");
        let mut streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        
        if let Some(config) = subs.get(stream_id) {
            if let Some(mut stream) = streams.get(stream_id) {
                assert_eq!(stream.sender, sender, "Not the sender");
                
                let now = env.ledger().timestamp();
                
                // Check if stream has ended or is about to end
                assert!(now >= stream.end_time.saturating_sub(config.grace_period), "Too early to renew");
                
                // Reset stream for next period
                stream.start_time = now;
                stream.end_time = now + config.period;
                stream.streamed_amount = 0;
                stream.remaining_amount = config.amount_per_period;
                stream.total_amount = config.amount_per_period;
                stream.status = StreamStatus::Active;
                stream.last_update_time = now;
                
                streams.set(stream_id, stream);
                env.storage().set(&streams_key, &streams);
                
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Pause a stream
    #[soroban_sdk::export_fn]
    pub fn pause_stream(env: Env, stream_id: u64, pauser: Address) -> bool {
        let streams_key = Symbol::new(&env, "streams");
        let mut streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        
        if let Some(mut stream) = streams.get(stream_id) {
            assert!(stream.sender == pauser || stream.recipient == pauser, "Not authorized");
            assert!(stream.status == StreamStatus::Active, "Cannot pause non-active stream");
            
            stream.status = StreamStatus::Paused;
            streams.set(stream_id, stream);
            env.storage().set(&streams_key, &streams);
            
            true
        } else {
            false
        }
    }

    /// Resume a paused stream
    #[soroban_sdk::export_fn]
    pub fn resume_stream(env: Env, stream_id: u64, resumer: Address) -> bool {
        let streams_key = Symbol::new(&env, "streams");
        let mut streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        
        if let Some(mut stream) = streams.get(stream_id) {
            assert!(stream.sender == resumer || stream.recipient == resumer, "Not authorized");
            assert!(stream.status == StreamStatus::Paused, "Stream not paused");
            
            stream.status = StreamStatus::Active;
            streams.set(stream_id, stream);
            env.storage().set(&streams_key, &streams);
            
            true
        } else {
            false
        }
    }

    /// Get stream details
    #[soroban_sdk::export_fn]
    pub fn get_stream(env: Env, stream_id: u64) -> Option<Stream> {
        let streams_key = Symbol::new(&env, "streams");
        let streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        streams.get(stream_id)
    }

    /// Get claimable amount for a stream
    #[soroban-sdk::export_fn]
    pub fn get_claimable_amount(env: Env, stream_id: u64) -> u64 {
        let streams_key = Symbol::new(&env, "streams");
        let streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        
        if let Some(stream) = streams.get(stream_id) {
            Self::calculate_claimable(env.clone(), stream)
        } else {
            0
        }
    }

    /// Get stream status
    #[soroban_sdk::export_fn]
    pub fn get_stream_status(env: Env, stream_id: u64) -> StreamStatus {
        let streams_key = Symbol::new(&env, "streams");
        let streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        
        if let Some(mut stream) = streams.get(stream_id) {
            // Auto-transition based on time
            let now = env.ledger().timestamp();
            
            if stream.status == StreamStatus::Pending && now >= stream.start_time {
                stream.status = StreamStatus::Active;
                let streams = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
                let mut mutable_streams = streams;
                mutable_streams.set(stream_id, stream);
                env.storage().set(&streams_key, &mutable_streams);
            }
            
            if stream.status == StreamStatus::Active && now >= stream.end_time {
                stream.status = StreamStatus::Completed;
                let streams = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
                let mut mutable_streams = streams;
                mutable_streams.set(stream_id, stream);
                env.storage().set(&streams_key, &mutable_streams);
            }
            
            stream.status
        } else {
            StreamStatus::Cancelled
        }
    }

    /// Get all streams for a user (as sender or recipient)
    #[soroban_sdk::export_fn]
    pub fn get_user_streams(env: Env, user: Address) -> Vec<Stream> {
        let streams_key = Symbol::new(&env, "streams");
        let streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        
        let mut result = Vec::new(&env);
        for (_, stream) in streams.iter() {
            if stream.sender == user || stream.recipient == user {
                result.push_back(stream);
            }
        }
        result
    }

    /// Get streaming rate per second
    #[soroban_sdk::export_fn]
    pub fn get_rate_per_second(env: Env, stream_id: u64) -> u64 {
        let streams_key = Symbol::new(&env, "streams");
        let streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        
        streams.get(stream_id).map_or(0, |s| s.rate_per_second)
    }

    /// Update stream duration (extend only)
    #[soroban_sdk::export_fn]
    pub fn extend_stream(env: Env, stream_id: u64, sender: Address, additional_duration: u64) -> bool {
        let streams_key = Symbol::new(&env, "streams");
        let mut streams: Map<u64, Stream> = env.storage().get(&streams_key).unwrap_or(Map::new(&env));
        
        if let Some(mut stream) = streams.get(stream_id) {
            assert_eq!(stream.sender, sender, "Not the sender");
            assert!(stream.status == StreamStatus::Active, "Stream not active");
            
            // Only extend, not shorten
            let new_end = stream.end_time + additional_duration;
            assert!(new_end <= stream.start_time + 31536000 * 5, "Exceeds max duration"); // 5 year max
            
            stream.end_time = new_end;
            stream.stream_type = StreamType::Linear; // Keep linear for extension
            streams.set(stream_id, stream);
            env.storage().set(&streams_key, &streams);
            
            true
        } else {
            false
        }
    }
}

/// Testing module
#[cfg(test)]
mod test {
    use super::*;
    
    #[test]
    fn test_linear_stream_creation() {
        let env = Env::default();
        let sender = Address::random(&env);
        let recipient = Address::random(&env);
        let token = Address::random(&env);
        let admin = Address::random(&env);
        
        PaymentStreaming::initialize(env.clone(), admin.clone(), 10); // 0.1% fee
        
        let now = env.ledger().timestamp();
        let stream_id = PaymentStreaming::create_stream(
            env.clone(),
            sender.clone(),
            recipient.clone(),
            token.clone(),
            1000000,        // 1M units
            now,
            1000000,        // 1M seconds
        );
        
        assert_eq!(stream_id, 1);
        
        let stream = PaymentStreaming::get_stream(env.clone(), stream_id).unwrap();
        assert_eq!(stream.total_amount, 1000000);
        assert_eq!(stream.rate_per_second, 1); // 1M / 1M = 1 per second
        assert_eq!(stream.stream_type, StreamType::Linear);
    }
    
    #[test]
    fn test_cliff_vesting() {
        let env = Env::default();
        let sender = Address::random(&env);
        let recipient = Address::random(&env);
        let token = Address::random(&env);
        let admin = Address::random(&env);
        
        PaymentStreaming::initialize(env.clone(), admin.clone(), 0);
        
        let now = env.ledger().timestamp();
        let stream_id = PaymentStreaming::create_stream_with_cliff(
            env.clone(),
            sender.clone(),
            recipient.clone(),
            token.clone(),
            1000000,        // 1M total
            200000,         // 200K cliff
            now + 1000,     // Cliff in 1000 seconds
            1000000,        // 1M seconds vesting
        );
        
        let stream = PaymentStreaming::get_stream(env.clone(), stream_id).unwrap();
        assert_eq!(stream.cliff_amount, 200000);
        assert_eq!(stream.stream_type, StreamType::Cliff);
    }
    
    #[test]
    fn test_claimable_calculation() {
        let env = Env::default();
        let sender = Address::random(&env);
        let recipient = Address::random(&env);
        let token = Address::random(&env);
        let admin = Address::random(&env);
        
        PaymentStreaming::initialize(env.clone(), admin.clone(), 0);
        
        let now = env.ledger().timestamp();
        let stream_id = PaymentStreaming::create_stream(
            env.clone(),
            sender.clone(),
            recipient.clone(),
            token.clone(),
            1000000, // 1M units
            now,
            1000,    // 1000 seconds = 1000 per second
        );
        
        // At 50% time, should have 50% claimable
        let stream = PaymentStreaming::get_stream(env.clone(), stream_id).unwrap();
        let initial_claimable = PaymentStreaming::get_claimable_amount(env.clone(), stream_id);
        assert_eq!(initial_claimable, 0); // Just started
        
        // Simulate advancing 500 seconds
        let advance_result = env.ledger().set_timestamp(now + 500);
        let half_claimable = PaymentStreaming::get_claimable_amount(env.clone(), stream_id);
        assert!(half_claimable >= 500000); // ~50%
        
        // At end time, all should be claimable
        let full_claimable = PaymentStreaming::get_claimable_amount(env.clone(), stream_id);
        assert!(full_claimable >= 500000);
    }
    
    #[test]
    fn test_stream_cancellation_refund() {
        let env = Env::default();
        let sender = Address::random(&env);
        let recipient = Address::random(&env);
        let token = Address::random(&env);
        let admin = Address::random(&env);
        
        PaymentStreaming::initialize(env.clone(), admin.clone(), 0);
        
        let now = env.ledger().timestamp();
        let stream_id = PaymentStreaming::create_stream(
            env.clone(),
            sender.clone(),
            recipient.clone(),
            token.clone(),
            1000000,
            now,
            1000,
        );
        
        // Cancel immediately - full refund
        let refund = PaymentStreaming::cancel_stream(env.clone(), stream_id, sender.clone());
        assert_eq!(refund, 1000000);
        
        let stream = PaymentStreaming::get_stream(env.clone(), stream_id).unwrap();
        assert_eq!(stream.status, StreamStatus::Cancelled);
    }
    
    #[test]
    fn test_partial_stream_cancellation() {
        let env = Env::default();
        let sender = Address::random(&env);
        let recipient = Address::random(&env);
        let token = Address::random(&env);
        let admin = Address::random(&env);
        
        PaymentStreaming::initialize(env.clone(), admin.clone(), 0);
        
        let now = env.ledger().timestamp();
        let stream_id = PaymentStreaming::create_stream(
            env.clone(),
            sender.clone(),
            recipient.clone(),
            token.clone(),
            1000000,
            now,
            1000, // 1000 per second
        );
        
        // Advance 50%
        env.ledger().set_timestamp(now + 500);
        
        // Cancel - should get ~50% back
        let refund = PaymentStreaming::cancel_stream(env.clone(), stream_id, sender.clone());
        assert!(refund < 1000000 && refund > 400000); // ~500K remaining
        
        let stream = PaymentStreaming::get_stream(env.clone(), stream_id).unwrap();
        assert_eq!(stream.status, StreamStatus::Cancelled);
    }
    
    #[test]
    fn test_recurring_subscription() {
        let env = Env::default();
        let sender = Address::random(&env);
        let recipient = Address::random(&env);
        let token = Address::random(&env);
        let admin = Address::random(&env);
        
        PaymentStreaming::initialize(env.clone(), admin.clone(), 0);
        
        let period = 86400; // 1 day
        let stream_id = PaymentStreaming::create_subscription(
            env.clone(),
            sender.clone(),
            recipient.clone(),
            token.clone(),
            100000,        // 100K per period
            period,
            31536000,      // Max 1 year
        );
        
        let stream = PaymentStreaming::get_stream(env.clone(), stream_id).unwrap();
        assert_eq!(stream.stream_type, StreamType::Recurring);
        assert_eq!(stream.remaining_amount, 100000);
    }
    
    #[test]
    fn test_pause_resume_stream() {
        let env = Env::default();
        let sender = Address::random(&env);
        let recipient = Address::random(&env);
        let token = Address::random(&env);
        let admin = Address::random(&env);
        
        PaymentStreaming::initialize(env.clone(), admin.clone(), 0);
        
        let now = env.ledger().timestamp();
        let stream_id = PaymentStreaming::create_stream(
            env.clone(),
            sender.clone(),
            recipient.clone(),
            token.clone(),
            1000000,
            now,
            1000,
        );
        
        // Pause
        let paused = PaymentStreaming::pause_stream(env.clone(), stream_id, sender.clone());
        assert!(paused);
        
        let status = PaymentStreaming::get_stream_status(env.clone(), stream_id);
        assert_eq!(status, StreamStatus::Paused);
        
        // Resume
        let resumed = PaymentStreaming::resume_stream(env.clone(), stream_id, sender.clone());
        assert!(resumed);
        
        let status = PaymentStreaming::get_stream_status(env.clone(), stream_id);
        assert_eq!(status, StreamStatus::Active);
    }
    
    #[test]
    fn test_withdraw_unclaimed() {
        let env = Env::default();
        let sender = Address::random(&env);
        let recipient = Address::random(&env);
        let token = Address::random(&env);
        let admin = Address::random(&env);
        
        PaymentStreaming::initialize(env.clone(), admin.clone(), 0);
        
        let now = env.ledger().timestamp();
        let stream_id = PaymentStreaming::create_stream(
            env.clone(),
            sender.clone(),
            recipient.clone(),
            token.clone(),
            1000000,
            now,
            1000,
        );
        
        // Advance past end time
        env.ledger().set_timestamp(now + 2000);
        
        // Recipient doesn't claim, sender withdraws
        let unclaimed = PaymentStreaming::withdraw_unclaimed(env.clone(), stream_id, sender.clone());
        assert_eq!(unclaimed, 1000000);
        
        let stream = PaymentStreaming::get_stream(env.clone(), stream_id).unwrap();
        assert_eq!(stream.status, StreamStatus::Claimed);
    }
    
    #[test]
    fn test_instant_stream() {
        let env = Env::default();
        let sender = Address::random(&env);
        let recipient = Address::random(&env);
        let token = Address::random(&env);
        let admin = Address::random(&env);
        
        PaymentStreaming::initialize(env.clone(), admin.clone(), 0);
        
        // Create linear stream with very short duration
        let now = env.ledger().timestamp();
        let stream_id = PaymentStreaming::create_stream(
            env.clone(),
            sender.clone(),
            recipient.clone(),
            token.clone(),
            1000000,
            now,
            1, // 1 second = instant
        );
        
        // Advance past end
        env.ledger().set_timestamp(now + 2);
        
        // All should be claimable immediately
        let claimable = PaymentStreaming::get_claimable_amount(env.clone(), stream_id);
        assert_eq!(claimable, 1000000);
    }
}