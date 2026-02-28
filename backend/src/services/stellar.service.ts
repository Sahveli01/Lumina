/**
 * StellarService
 *
 * Thin orchestration layer over @stellar/stellar-sdk for Soroban contract
 * interactions.  Exposes three core operations:
 *
 *   invokeContract    — build → sign → submit → poll (write)
 *   buildAndSubmitTx  — lower-level: same flow from a pre-built operation
 *   getContractState  — direct ledger-entry read (persistent storage)
 *   queryContract     — simulation-based read (no fees, no signing)
 *
 * All ScVal encoding helpers are static so routes can call them without a
 * service instance.
 */

import * as StellarSdk from '@stellar/stellar-sdk';

// ── Public types ──────────────────────────────────────────────────────────────

export interface TxResult {
  txHash: string;
  returnValue: StellarSdk.xdr.ScVal;
}

// ── Service class ─────────────────────────────────────────────────────────────

export class StellarService {
  private readonly rpc: StellarSdk.rpc.Server;
  private readonly networkPassphrase: string;

  constructor() {
    const rpcUrl =
      process.env['STELLAR_RPC_URL'] ?? 'https://soroban-testnet.stellar.org';
    this.networkPassphrase =
      process.env['STELLAR_NETWORK_PASSPHRASE'] ?? StellarSdk.Networks.TESTNET;
    this.rpc = new StellarSdk.rpc.Server(rpcUrl, { allowHttp: false });
  }

  // ── Write operations ───────────────────────────────────────────────────────

  /**
   * Build a Soroban contract-call operation, sign it with `signerKeypair`,
   * submit it to the network, and poll until the transaction is confirmed.
   *
   * @returns tx hash + decoded return value from the contract.
   */
  async invokeContract(
    contractId: string,
    method: string,
    params: StellarSdk.xdr.ScVal[],
    signerKeypair: StellarSdk.Keypair,
  ): Promise<TxResult> {
    const contract = new StellarSdk.Contract(contractId);
    const operation = contract.call(method, ...params);
    return this.buildAndSubmitTx(operation, signerKeypair);
  }

  /**
   * Generic transaction builder/submitter.  Accepts any Soroban operation
   * (e.g. one built externally via `Contract.call`).
   *
   * Flow:
   *   1. Fetch the latest account sequence from RPC.
   *   2. Wrap the operation in a TransactionBuilder.
   *   3. Call `rpc.prepareTransaction` (simulation + fee augmentation).
   *   4. Sign with the provided keypair.
   *   5. `rpc.sendTransaction` — reject immediately on ERROR.
   *   6. Poll `rpc.getTransaction` every second until SUCCESS | FAILED.
   */
  async buildAndSubmitTx(
    operation: StellarSdk.xdr.Operation,
    keypair: StellarSdk.Keypair,
  ): Promise<TxResult> {
    const account = await this.rpc.getAccount(keypair.publicKey());

    const rawTx = new StellarSdk.TransactionBuilder(account, {
      fee: StellarSdk.BASE_FEE,
      networkPassphrase: this.networkPassphrase,
    })
      .addOperation(operation)
      .setTimeout(30)
      .build();

    // prepareTransaction simulates the tx and augments resource fees.
    const prepared = await this.rpc.prepareTransaction(rawTx);
    prepared.sign(keypair);

    const sendResp = await this.rpc.sendTransaction(prepared);
    if (sendResp.status === 'ERROR') {
      const detail = sendResp.errorResult
        ? JSON.stringify(sendResp.errorResult)
        : 'unknown error';
      throw new Error(`sendTransaction failed (ERROR): ${detail}`);
    }

    const txHash = sendResp.hash;

    // Poll up to 30 attempts × 1 s = 30 s ceiling.
    for (let attempt = 0; attempt < 30; attempt++) {
      await sleep(1_000);
      const resp = await this.rpc.getTransaction(txHash);

      if (resp.status === 'SUCCESS') {
        const ok = resp as StellarSdk.rpc.Api.GetSuccessfulTransactionResponse;
        return {
          txHash,
          returnValue: ok.returnValue ?? StellarSdk.nativeToScVal(null),
        };
      }

      if (resp.status === 'FAILED') {
        throw new Error(`Transaction FAILED on-chain: ${txHash}`);
      }

      // NOT_FOUND → ledger not closed yet; keep polling.
    }

    throw new Error(
      `Transaction ${txHash} was not confirmed within 30 seconds`
    );
  }

  // ── Read operations ────────────────────────────────────────────────────────

  /**
   * Simulate a read-only contract call without submitting a transaction.
   * Uses the admin account as the source account (required by the RPC server
   * to build a valid transaction envelope for simulation).
   *
   * @returns The contract's return ScVal, or `null` if nothing was returned.
   */
  async queryContract(
    contractId: string,
    method: string,
    params: StellarSdk.xdr.ScVal[],
  ): Promise<StellarSdk.xdr.ScVal | null> {
    const adminSecret = process.env['ADMIN_SECRET_KEY'];
    if (!adminSecret) {
      throw new Error(
        'ADMIN_SECRET_KEY is required for read-only contract queries ' +
          '(used as the simulation source account).'
      );
    }

    const adminKeypair = StellarSdk.Keypair.fromSecret(adminSecret);
    const account = await this.rpc.getAccount(adminKeypair.publicKey());

    const contract = new StellarSdk.Contract(contractId);
    const rawTx = new StellarSdk.TransactionBuilder(account, {
      fee: StellarSdk.BASE_FEE,
      networkPassphrase: this.networkPassphrase,
    })
      .addOperation(contract.call(method, ...params))
      .setTimeout(30)
      .build();

    const simResp = await this.rpc.simulateTransaction(rawTx);

    // Discriminate the union: error response has an `error` string field.
    if ('error' in simResp && typeof simResp.error === 'string') {
      throw new Error(
        `Contract query "${method}" on ${contractId} failed: ${simResp.error}`
      );
    }

    // Both success and restore responses expose `result?.retval`.
    const withResult =
      simResp as StellarSdk.rpc.Api.SimulateTransactionSuccessResponse;
    return withResult.result?.retval ?? null;
  }

  /**
   * Read a single persistent ledger entry from a Soroban contract's storage.
   * Returns `null` if the entry does not exist (instead of throwing).
   *
   * Prefer `queryContract` for data computed by a contract function.
   * Use this for simple key→value lookups in the contract's persistent map.
   */
  async getContractState(
    contractId: string,
    key: StellarSdk.xdr.ScVal,
  ): Promise<StellarSdk.xdr.ScVal | null> {
    try {
      const ledgerKey = StellarSdk.xdr.LedgerKey.contractData(
        new StellarSdk.xdr.LedgerKeyContractData({
          contract: new StellarSdk.Address(contractId).toScAddress(),
          key,
          durability: StellarSdk.xdr.ContractDataDurability.persistent(),
        })
      );
      const resp = await this.rpc.getLedgerEntries(ledgerKey);
      if (resp.entries.length === 0) return null;
      return resp.entries[0].val.contractData().val();
    } catch {
      return null;
    }
  }

  // ── ScVal encoding helpers (static) ───────────────────────────────────────

  /** Encode a hex string as a Soroban `Bytes` ScVal. */
  static bytes(hex: string): StellarSdk.xdr.ScVal {
    return StellarSdk.nativeToScVal(Buffer.from(hex, 'hex'), { type: 'bytes' });
  }

  /** Encode a number or bigint as a Soroban `u64` ScVal. */
  static u64(value: number | bigint): StellarSdk.xdr.ScVal {
    return StellarSdk.nativeToScVal(BigInt(value), { type: 'u64' });
  }

  /** Encode a number as a Soroban `u32` ScVal. */
  static u32(value: number): StellarSdk.xdr.ScVal {
    return StellarSdk.nativeToScVal(value, { type: 'u32' });
  }

  /** Encode a number or bigint as a Soroban `i128` ScVal. */
  static i128(value: number | bigint): StellarSdk.xdr.ScVal {
    return StellarSdk.nativeToScVal(BigInt(value), { type: 'i128' });
  }

  /** Encode a string as a Soroban `Symbol` ScVal (e.g. tranche names). */
  static symbol(s: string): StellarSdk.xdr.ScVal {
    return StellarSdk.xdr.ScVal.scvSymbol(s);
  }

  /** Encode a Stellar address string as a Soroban `Address` ScVal. */
  static address(addr: string): StellarSdk.xdr.ScVal {
    return new StellarSdk.Address(addr).toScVal();
  }

  /** Decode a ScVal to a native JS value via scValToNative. */
  static decode(val: StellarSdk.xdr.ScVal): unknown {
    return StellarSdk.scValToNative(val);
  }

  // ── Keypair helpers (static) ───────────────────────────────────────────────

  /** Derive a Keypair from a raw secret key string. */
  static keypairFromSecret(secret: string): StellarSdk.Keypair {
    return StellarSdk.Keypair.fromSecret(secret);
  }

  /**
   * Derive the admin Keypair from ADMIN_SECRET_KEY.
   * Throws a descriptive error if the variable is unset.
   */
  static adminKeypair(): StellarSdk.Keypair {
    const secret = process.env['ADMIN_SECRET_KEY'];
    if (!secret) {
      throw new Error(
        'ADMIN_SECRET_KEY is not configured. ' +
          'Generate one with: stellar keys generate --global admin'
      );
    }
    return StellarSdk.Keypair.fromSecret(secret);
  }

  // ── One-time initialization ────────────────────────────────────────────────

  /**
   * Calls `initialize` on all four Lumina contracts in the correct dependency
   * order.  Safe to call after `bash scripts/deploy.sh testnet` has updated
   * the .env file — each contract's initialize guards against double-init with
   * AlreadyInitialized (error code 1), which this function treats as a no-op.
   *
   * Order:
   *   1. risk-oracle        (no external deps)
   *   2. nullifier-registry (needs lumina-core address)
   *   3. liquidity-pools    (needs lumina-core + stablecoin)
   *   4. lumina-core        (needs all three above)
   *
   * @param ids    - the four deployed contract IDs
   * @param stablecoin - USDC contract ID on the target network
   */
  async initializeContracts(
    ids: {
      luminaCore:        string;
      nullifierRegistry: string;
      riskOracle:        string;
      liquidityPools:    string;
    },
    stablecoin: string,
  ): Promise<{
    riskOracle:        string | 'already_initialized';
    nullifierRegistry: string | 'already_initialized';
    liquidityPools:    string | 'already_initialized';
    luminaCore:        string | 'already_initialized';
  }> {
    const keypair  = StellarService.adminKeypair();
    const adminAddr = keypair.publicKey();

    const safeInvoke = async (
      contractId: string,
      method: string,
      params: StellarSdk.xdr.ScVal[],
    ): Promise<string | 'already_initialized'> => {
      try {
        const { txHash } = await this.invokeContract(contractId, method, params, keypair);
        return txHash;
      } catch (err) {
        const msg = (err as Error).message ?? '';
        // ContractError::AlreadyInitialized = error code 1
        if (msg.includes('AlreadyInitialized') || msg.includes('code: 1')) {
          return 'already_initialized';
        }
        throw err;
      }
    };

    // 1. risk-oracle: initialize(admin)
    const riskOracle = await safeInvoke(
      ids.riskOracle,
      'initialize',
      [StellarService.address(adminAddr)],
    );

    // 2. nullifier-registry: initialize(admin, lumina_core)
    const nullifierRegistry = await safeInvoke(
      ids.nullifierRegistry,
      'initialize',
      [
        StellarService.address(adminAddr),
        StellarService.address(ids.luminaCore),
      ],
    );

    // 3. liquidity-pools: initialize(admin, lumina_core, stablecoin)
    const liquidityPools = await safeInvoke(
      ids.liquidityPools,
      'initialize',
      [
        StellarService.address(adminAddr),
        StellarService.address(ids.luminaCore),
        StellarService.address(stablecoin),
      ],
    );

    // 4. lumina-core: initialize(admin, nullifier_registry, risk_oracle, liquidity_pools)
    const luminaCore = await safeInvoke(
      ids.luminaCore,
      'initialize',
      [
        StellarService.address(adminAddr),
        StellarService.address(ids.nullifierRegistry),
        StellarService.address(ids.riskOracle),
        StellarService.address(ids.liquidityPools),
      ],
    );

    return { riskOracle, nullifierRegistry, liquidityPools, luminaCore };
  }
}

// ── Utility ───────────────────────────────────────────────────────────────────

function sleep(ms: number): Promise<void> {
  return new Promise<void>((resolve) => setTimeout(resolve, ms));
}
