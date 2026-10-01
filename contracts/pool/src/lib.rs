    pub fn deposit(env: Env, lp: Address, usdc_amount: u128) -> u128 {
        Self::require_initialized(&env);
        lp.require_auth();
        if usdc_amount == 0 {
            panic_with_error!(&env, PoolError::InvalidAmount);
        }

        let totals = Self::totals(&env);
        let total_shares = totals.shares;
        let total_deposits = totals.deposits;

        if (total_shares == 0 || total_deposits == 0) && usdc_amount < MIN_INITIAL_DEPOSIT {
            panic_with_error!(&env, PoolError::InvalidAmount);
        }

        let shares_to_issue = if total_shares == 0 || total_deposits == 0 {
            usdc_amount
        } else {
            let scaled = usdc_amount
                .checked_mul(total_shares)
                .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
            scaled / total_deposits
        };

        // Dust-attack guard: once the pool accrues yield, the share price
        // (total_deposits / total_shares) rises above 1.0, so a sufficiently
        // small deposit can round down to 0 shares while its USDC is still
        // pulled into total_deposits, silently donating the deposit to existing
        // LPs. Reject any deposit that would mint 0 shares so the caller keeps
        // their funds. This check runs before the token transfer, so no USDC
        // leaves the depositor on the rejection path.
        if shares_to_issue == 0 {
            panic_with_error!(&env, PoolError::MinimumDeposit);
        }

        let usdc_id = Self::usdc(&env);
        let usdc = token::Client::new(&env, &usdc_id);
        usdc.transfer(&lp, &env.current_contract_address(), &(usdc_amount as i128));

        Self::mint(&env, &lp, shares_to_issue);
        let new_total_deposits = total_deposits
            .checked_add(usdc_amount)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        env.storage()
            .instance()
            .set(&DataKey::TotalDeposits, &new_total_deposits);

        let lp_deposit_count_key = DataKey::LPDepositCount(lp.clone());
        let count: u32 = env
            .storage()
            .persistent()
            .get(&lp_deposit_count_key)
            .unwrap_or(0);
        let new_count = count
            .checked_add(1)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        env.storage().persistent().set(&lp_deposit_count_key, &new_count);
        env.storage()
            .persistent()
            .extend_ttl(&lp_deposit_count_key, TTL_THRESHOLD, TTL_EXTEND_TO);

        let lp_init_key = DataKey::LPInitialDeposit(lp.clone());
        let init_dep: u128 = env.storage().persistent().get(&lp_init_key).unwrap_or(0);
        let new_init_dep = init_dep
            .checked_add(usdc_amount)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        env.storage()
            .persistent()
            .set(&lp_init_key, &new_init_dep);
        env.storage()
            .persistent()
            .extend_ttl(&lp_init_key, TTL_THRESHOLD, TTL_EXTEND_TO);

        events::lp_deposited(&env, &lp, usdc_amount, shares_to_issue);
        Self::extend_instance_ttl(&env);
        shares_to_issue
    }

    /// Withdraws shares from the pool and transfers USDC to the LP.
    ///
    /// # Arguments
    /// * `env` - The Soroban environment.
    /// * `lp` - The liquidity provider address.
    /// * `shares` - The number of shares to withdraw.
    ///
    /// # Auth
    /// Requires self-authorization from `lp` (via `lp.require_auth()`).
    ///
    /// # Panics
    /// * `InvalidAmount` if `shares` is zero.
    /// * `NoShares` if the LP has no shares.
    /// * `InsufficientShares` if the LP does not own enough shares.
    /// * `MinimumDeposit` if the computed USDC redemption rounds down to zero
    ///   (dust-guard: prevents burning shares for nothing when the share price
    ///   is very high relative to the number of shares redeemed).
    /// * `InsufficientLiquidity` if the pool lacks enough available USDC.
    /// * `Overflow` if `shares * total_deposits` (or `shares * lp_initial_deposit`)
    ///   would overflow `u128` while computing the redemption amount.
    ///
    /// # Notes
    /// On full withdrawal (remaining shares reach zero), `LPInitialDeposit`
    /// and `LPDepositCount` are removed from storage. This ensures a
    /// subsequent re-deposit starts with a fresh initial-deposit basis
    /// and an accurate deposit count.
    ///
    /// # Returns
    /// * `u128` - The amount of USDC returned.
    ///
    /// # Example
    /// ```ignore
    /// let returned = client.withdraw(&lp, 500);
    /// ```
    pub fn withdraw(env: Env, lp: Address, shares: u128) -> u128 {
        Self::require_initialized(&env);
        lp.require_auth();
        if shares == 0 {
            panic_with_error!(&env, PoolError::InvalidAmount);
        }

        let lp_shares_key = DataKey::LPShares(lp.clone());
        let lp_shares: u128 = env
            .storage()
            .persistent()
            .get(&lp_shares_key)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::NoShares));
        if shares > lp_shares {
            panic_with_error!(&env, PoolError::InsufficientShares);
        }

        let totals = Self::totals(&env);
        let total_shares = totals.shares;
        let total_deposits = totals.deposits;
        let total_funded = totals.funded;
        let available = total_deposits - total_funded;

        let scaled = shares
            .checked_mul(total_deposits)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        let usdc_to_return = scaled / total_shares;
        if usdc_to_return == 0 {
            panic_with_error!(&env, PoolError::MinimumDeposit);
        }
        if usdc_to_return > available {
            panic_with_error!(&env, PoolError::InsufficientLiquidity);
        }

        let usdc_id = Self::usdc(&env);
        let usdc = token::Client::new(&env, &usdc_id);
        usdc.transfer(
            &env.current_contract_address(),
            &lp,
            &(usdc_to_return as i128),
        );

        let remaining_shares = Self::burn(&env, &lp, shares);
        let new_total_deposits = total_deposits
            .checked_sub(usdc_to_return)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        env.storage()
            .instance()
            .set(&DataKey::TotalDeposits, &new_total_deposits);

        if remaining_shares == 0 {
            // Full withdrawal: reset LP-scoped storage to prevent stale state
            // on re-deposit. LPInitialDeposit is zeroed below via the
            // principal_portion calculation; LPDepositCount must be removed too.
            let dep_count_key = DataKey::LPDepositCount(lp.clone());
            env.storage().persistent().remove(&dep_count_key);
        }

        let init_dep_key = DataKey::LPInitialDeposit(lp.clone());
        let init_dep: u128 = env.storage().persistent().get(&init_dep_key).unwrap_or(0);
        let principal_scaled = shares
            .checked_mul(init_dep)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        let principal_portion = principal_scaled / (lp_shares);
        let yield_earned = usdc_to_return.saturating_sub(principal_portion);

        let new_init_dep = init_dep.saturating_sub(principal_portion);
        if new_init_dep > 0 {
            env.storage().persistent().set(&init_dep_key, &new_init_dep);
            env.storage()
                .persistent()
                .extend_ttl(&init_dep_key, TTL_THRESHOLD, TTL_EXTEND_TO);
        } else {
            env.storage().persistent().remove(&init_dep_key);
        }

        let yield_key = DataKey::LPYieldEarned(lp.clone());
        let prev_yield: u128 = env.storage().persistent().get(&yield_key).unwrap_or(0);
        let new_yield = prev_yield
            .checked_add(yield_earned)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        env.storage().persistent().set(&yield_key, &new_yield);
        env.storage()
            .persistent()
            .extend_ttl(&yield_key, TTL_THRESHOLD, TTL_EXTEND_TO);

        events::lp_withdrawn(&env, &lp, usdc_to_return, shares);
        Self::extend_instance_ttl(&env);
        usdc_to_return
    }

    pub fn fund_invoice(env: Env, invoice_id: BytesN<32>) -> bool {
        Self::require_initialized(&env);
        let invoice_contract = Self::invoice_contract(&env)
            .expect("pool is not initialized: invoice contract missing");

        let mut args = Vec::new(&env);
        args.push_back(invoice_id.clone().into_val(&env));
        let (invoice_status, face_value, discount_bps): (u32, u128, u32) = env.invoke_contract(
            &invoice_contract,
            &Symbol::new(&env, "get_funding_terms"),
            args,
        );
        if invoice_status != 1 {
            panic_with_error!(&env, PoolError::InvoiceNotListed);
        }

        let funded_key = DataKey::FundedInvoice(invoice_id.clone());
        if env.storage().persistent().has(&funded_key) {
            panic_with_error!(&env, PoolError::AlreadyFunded);
        }

        let registry_id = Self::registry_contract(&env);
        let mut args = Vec::new(&env);
        args.push_back(invoice_id.clone().into_val(&env));
        let issuer: Address =
            env.invoke_contract(&invoice_contract, &Symbol::new(&env, "get_issuer"), args);
        let mut args = Vec::new(&env);
        args.push_back(issuer.into_val(&env));
        let issuer_verified: bool =
            env.invoke_contract(&registry_id, &Symbol::new(&env, "is_verified"), args);
        if !issuer_verified {
            panic_with_error!(&env, PoolError::IssuerNotVerified);
        }

        let mut args = Vec::new(&env);
        args.push_back(invoice_id.clone().into_val(&env));
        let buyer: Address =
            env.invoke_contract(&invoice_contract, &Symbol::new(&env, "get_buyer"), args);
        let mut args = Vec::new(&env);
        args.push_back(buyer.into_val(&env));
        let buyer_verified: bool =
            env.invoke_contract(&registry_id, &Symbol::new(&env, "is_verified"), args);
        if !buyer_verified {
            panic_with_error!(&env, PoolError::BuyerNotVerified);
        }

        let mut args = Vec::new(&env);
        args.push_back(invoice_id.clone().into_val(&env));
        let invoice_asset: Address = env.invoke_contract(
            &invoice_contract,
            &Symbol::new(&env, "get_funding_asset"),
            args,
        );
        let usdc_id = Self::usdc(&env);
        if invoice_asset != usdc_id {
            panic_with_error!(&env, PoolError::AssetMismatch);
        }

        // `face_value` is read from the invoice contract via a cross-contract
        // call and is not bounded by this pool, so the scaling multiplication
        // must be guarded just like the utilization check below (#585).
        let funded_amount = face_value
            .checked_mul(10000 - discount_bps as u128)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow))
            / 10000;
        if funded_amount == 0 {
            panic_with_error!(&env, PoolError::InvalidAmount);
        }

        let totals = Self::totals(&env);
        let total_deposits = totals.deposits;
        let total_funded = totals.funded;
        let available = total_deposits - total_funded;
        if funded_amount > available {
            panic_with_error!(&env, PoolError::InsufficientLiquidity);
        }

        let max_utilization_bps = totals.max_utilization_bps;
        let new_total_funded = total_funded
            .checked_add(funded_amount)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        let utilization_after =
            Self::utilization_bps_or_panic(&env, new_total_funded, total_deposits);
        if utilization_after > max_utilization_bps {
            panic_with_error!(&env, PoolError::UtilizationCapExceeded);
        }

        // --- Checks-effects-interactions: commit pool state BEFORE any
        // cross-contract calls so a reentrant callback into this contract
        // always sees the updated TotalFunded / ActiveInvoiceCount /
        // FundedInvoice, preventing double-funding via stale state.
        env.storage()
            .instance()
            .set(&DataKey::TotalFunded, &new_total_funded);
        let active_count = totals.active_invoices;
        let new_active_count = active_count
            .checked_add(1)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        env.storage()
            .instance()
            .set(&DataKey::ActiveInvoiceCount, &new_active_count);

        env.storage().persistent().set(&funded_key, &funded_amount);
        env.storage()
            .persistent()
            .extend_ttl(&funded_key, TTL_THRESHOLD, TTL_EXTEND_TO);

        // --- Interactions: cross-contract calls after pool state is committed.
        let escrow_contract =
            Self::escrow_contract(&env).expect("pool is not initialized: escrow contract missing");

        let mut args = Vec::new(&env);
        args.push_back(invoice_id.clone().into_val(&env));
        args.push_back(funded_amount.into_val(&env));
        args.push_back(issuer.into_val(&env));
        let _: bool = env.invoke_contract(&escrow_contract, &Symbol::new(&env, "lock"), args);

        let pool_address = env.current_contract_address();
        let mut args = Vec::new(&env);
        args.push_back(invoice_id.clone().into_val(&env));
        args.push_back(pool_address.into_val(&env));
        args.push_back(usdc_id.into_val(&env));
        args.push_back(funded_amount.into_val(&env));
        let _: bool =
            env.invoke_contract(&invoice_contract, &Symbol::new(&env, "mark_funded"), args);

        events::invoice_funded(&env, &invoice_id, funded_amount);
        Self::extend_instance_ttl(&env);
        true
    }

    pub fn handle_default(env: Env, invoice_id: BytesN<32>) -> bool {
        let invoice_contract = Self::invoice_contract(&env)
            .expect("pool is not initialized: invoice contract missing");
        invoice_contract.require_auth();

        let funded_key = DataKey::FundedInvoice(invoice_id.clone());
        if !env.storage().persistent().has(&funded_key) {
            panic_with_error!(&env, PoolError::InvoiceNotFound);
        }
        let funded_amount: u128 = env.storage().persistent().get(&funded_key).unwrap();

        let escrow_contract =
            Self::escrow_contract(&env).expect("pool is not initialized: escrow contract missing");
        let pool_address = env.current_contract_address();
        let mut args = Vec::new(&env);
        args.push_back(invoice_id.clone().into_val(&env));
        args.push_back(pool_address.into_val(&env));
        let escrow_released: bool =
            env.invoke_contract(&escrow_contract, &Symbol::new(&env, "handle_default"), args);
        if !escrow_released {
            panic_with_error!(&env, PoolError::EscrowDefaultNotReleased);
        }

        let totals = Self::totals(&env);
        let total_funded = totals.funded;
        let total_deposits = totals.deposits;
        let total_loss_realised = totals.loss_realised;

        let new_total_funded = total_funded
            .checked_sub(funded_amount)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        let new_total_deposits = total_deposits
            .checked_sub(funded_amount)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        let new_total_loss = total_loss_realised
            .checked_add(funded_amount)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::Overflow));
        env.storage()
            .instance()
            .set(&DataKey::TotalFunded, &new_total_funded);
        env.storage()
            .instance()
            .set(&DataKey::TotalDeposits, &new_total_deposits);
        env.storage().instance().set(
            &DataKey::TotalLossRealised,
            &new_total_loss,
        );

        let active_count = totals.active_invoices;
        let new_active_count = active_count
            .checked_sub(1)
            .unwrap_or_else(|| panic_with_error!(&env, PoolError::ActiveCountUnderflow));
        env.storage()
            .instance()
            .set(&DataKey::ActiveInvoiceCount, &new_active_count);

        // Persist the Defaulted status on the invoice contract (step 2 of the
        // documented cross-contract sequence). This runs after the active-count
        // underflow check so a mismatched default still surfaces
        // ActiveCountUnderflow (#17) rather than an invoice lookup error from
        // mark_defaulted. mark_defaulted is idempotent: when
        // invoice.trigger_default already transitioned the status to Defaulted
        // before invoking this pool entry point, the call is a no-op.
        let mut args = Vec::new(&env);
        args.push_back(invoice_id.clone().into_val(&env));
        let _: bool = env.invoke_contract(
            &invoice_contract,
            &Symbol::new(&env, "mark_defaulted"),
            args,
        );

        env.storage().persistent().remove(&funded_key);

        events::invoice_defaulted(&env, &invoice_id, funded_amount);
        Self::extend_instance_ttl(&env);
        true
    }

    fn settle_repayment(env: &Env, invoice_id: &BytesN<32>, amount: u128, refund: u128) {
        let funded_key = DataKey::FundedInvoice(invoice_id.clone());
        let funded_amount: u128 = env
            .storage()
            .persistent()
            .get(&funded_key)
            .unwrap_or_else(|| panic_with_error!(env, PoolError::InvoiceNotFound));
        if amount < funded_amount {
            panic_with_error!(env, PoolError::InvalidAmount);
        }
        if refund > amount - funded_amount {
            panic_with_error!(env, PoolError::InvalidAmount);
        }

        let yield_amount = amount - funded_amount - refund;
        let totals = Self::totals(env);
        let total_deposits = totals.deposits;
        let total_funded = totals.funded;
        let total_yield = totals.yield_distributed;

        let new_total_funded = total_funded
            .checked_sub(funded_amount)
            .unwrap_or_else(|| panic_with_error!(env, PoolError::Overflow));
        let new_total_deposits = total_deposits
            .checked_add(yield_amount)
            .unwrap_or_else(|| panic_with_error!(env, PoolError::Overflow));
        let new_total_yield = total_yield
            .checked_add(yield_amount)
            .unwrap_or_else(|| panic_with_error!(env, PoolError::Overflow));
        env.storage()
            .instance()
            .set(&DataKey::TotalDeposits, &new_total_deposits);
        env.storage().instance().set(
            &DataKey::TotalYieldDistributed,
            &new_total_yield,
        );
        env.storage()
            .instance()
            .set(&DataKey::TotalFunded, &new_total_funded);

        let active_count = totals.active_invoices;
        let new_active_count = active_count
            .checked_sub(1)
            .unwrap_or_else(|| panic_with_error!(env, PoolError::ActiveCountUnderflow));
        env.storage()
            .instance()
            .set(&DataKey::ActiveInvoiceCount, &new_active_count);

        env.storage().persistent().remove(&funded_key);

        events::repayment_received(env, invoice_id, amount, yield_amount);
        Self::extend_instance_ttl(env);
    }

    /// Internal helper to mint LP shares (scoped for SEP-41 share issuance).
    fn mint(env: &Env, to: &Address, amount: u128) {
        let total_shares = Self::totals(env).shares;
        let new_total_shares = total_shares
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(env, PoolError::Overflow));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &new_total_shares);

        let lp_shares_key = DataKey::LPShares(to.clone());
        let lp_shares: u128 = env.storage().persistent().get(&lp_shares_key).unwrap_or(0);
        let new_lp_shares = lp_shares
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(env, PoolError::Overflow));
        env.storage()
            .persistent()
            .set(&lp_shares_key, &new_lp_shares);
        env.storage()
            .persistent()
            .extend_ttl(&lp_shares_key, TTL_THRESHOLD, TTL_EXTEND_TO);
    }

    /// Internal helper to burn LP shares (scoped for SEP-41 share redemption).
    fn burn(env: &Env, from: &Address, amount: u128) -> u128 {
        let total_shares = Self::totals(env).shares;
        let new_total_shares = total_shares
            .checked_sub(amount)
            .unwrap_or_else(|| panic_with_error!(env, PoolError::Overflow));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &new_total_shares);

        let lp_shares_key = DataKey::LPShares(from.clone());
        let lp_shares: u128 = env.storage().persistent().get(&lp_shares_key).unwrap_or(0);
        let remaining_shares = lp_shares
            .checked_sub(amount)
            .unwrap_or_else(|| panic_with_error!(env, PoolError::Overflow));
        env.storage()
            .persistent()
            .set(&lp_shares_key, &remaining_shares);
        env.storage()
            .persistent()
            .extend_ttl(&lp_shares_key, TTL_THRESHOLD, TTL_EXTEND_TO);
        remaining_shares
    }
