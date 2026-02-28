/**
 * Pool Routes  (/api/pool)
 *
 * POST /api/pool/deposit
 *   Body: { tranche, amount, depositor_secret_key }
 *   Calls liquidity-pools.deposit() signed by the depositor's own keypair.
 *
 * GET /api/pool/balance/:tranche/:depositor
 *   Simulates liquidity-pools.get_balance() for the given depositor address.
 */

import { Router, Request, Response } from 'express';
import * as StellarSdk from '@stellar/stellar-sdk';
import { StellarService } from '../services/stellar.service';
import { requireContractId } from '../stellar/contractIds';

const router = Router();
const stellar = new StellarService();

const VALID_TRANCHES = ['senior', 'junior'] as const;
type Tranche = (typeof VALID_TRANCHES)[number];

function isTranche(s: unknown): s is Tranche {
  return typeof s === 'string' && (VALID_TRANCHES as readonly string[]).includes(s);
}

// ── POST /api/pool/deposit ────────────────────────────────────────────────────

interface DepositBody {
  tranche: Tranche;
  amount: number;               // u64 — smallest currency unit (e.g. stroop / USDC micro)
  depositor_secret_key: string; // signing key of the depositor
}

router.post('/deposit', async (req: Request, res: Response): Promise<void> => {
  try {
    const b = req.body as Partial<DepositBody>;

    // Validate tranche
    if (!isTranche(b.tranche)) {
      res.status(400).json({
        success: false,
        error: `tranche must be one of: ${VALID_TRANCHES.join(', ')}`,
      });
      return;
    }

    // Validate amount
    if (typeof b.amount !== 'number' || b.amount <= 0) {
      res.status(400).json({ success: false, error: 'amount must be a positive number' });
      return;
    }

    // Validate depositor key
    if (!b.depositor_secret_key || typeof b.depositor_secret_key !== 'string') {
      res.status(400).json({ success: false, error: 'depositor_secret_key is required' });
      return;
    }

    const body = b as DepositBody;
    const liquidityPools = requireContractId('liquidityPools');

    let depositorKeypair: StellarSdk.Keypair;
    try {
      depositorKeypair = StellarService.keypairFromSecret(body.depositor_secret_key);
    } catch {
      res.status(400).json({ success: false, error: 'depositor_secret_key is not a valid Stellar secret key' });
      return;
    }

    const { txHash } = await stellar.invokeContract(
      liquidityPools,
      'deposit',
      [
        StellarService.symbol(body.tranche),   // "senior" | "junior"
        StellarService.u64(body.amount),
      ],
      depositorKeypair,
    );

    res.status(200).json({
      success: true,
      data: {
        tranche:   body.tranche,
        amount:    body.amount,
        depositor: depositorKeypair.publicKey(),
        tx_hash:   txHash,
      },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

// ── GET /api/pool/balance/:tranche/:depositor ─────────────────────────────────

router.get('/balance/:tranche/:depositor', async (req: Request, res: Response): Promise<void> => {
  try {
    // req.params values are string | string[] in @types/express@5; cast to string.
    const tranche = req.params['tranche'] as string;
    const depositor = req.params['depositor'] as string;

    if (!isTranche(tranche)) {
      res.status(400).json({
        success: false,
        error: `tranche must be one of: ${VALID_TRANCHES.join(', ')}`,
      });
      return;
    }

    const liquidityPools = requireContractId('liquidityPools');

    const raw = await stellar.queryContract(
      liquidityPools,
      'get_balance',
      [
        StellarService.symbol(tranche),
        StellarService.address(depositor),
      ],
    );

    // Contract returns u64; scValToNative gives us a bigint.
    const balance = raw
      ? (StellarSdk.scValToNative(raw) as bigint).toString()
      : '0';

    res.status(200).json({
      success: true,
      data: { tranche, depositor, balance },
    });
  } catch (err) {
    res.status(500).json({ success: false, error: (err as Error).message });
  }
});

export default router;
