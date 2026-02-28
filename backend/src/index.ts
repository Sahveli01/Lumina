import dotenv from 'dotenv';

dotenv.config();

// BigInt → number for JSON serialization (Soroban SDK returns BigInt values)
(BigInt.prototype as unknown as { toJSON: () => number }).toJSON = function () {
  return Number(this);
};

import * as StellarSdk from '@stellar/stellar-sdk';
import app from './app';
import { StellarService }   from './services/stellar.service';
import { requireContractId } from './stellar/contractIds';

const PORT = process.env['PORT'] ?? 4000;

/**
 * One-time startup check: ensure the admin keypair is whitelisted in the
 * nullifier-registry so it can call register_nullifier directly.
 *
 * The registry initialises with only lumina-core on the whitelist.
 * We check first (free simulation) and only submit a transaction when needed.
 */
async function ensureAdminWhitelisted(): Promise<void> {
  const stellar      = new StellarService();
  const adminKeypair = StellarService.adminKeypair();
  const nullifierRegistry = requireContractId('nullifierRegistry');

  const raw = await stellar.queryContract(
    nullifierRegistry,
    'is_whitelisted',
    [StellarService.address(adminKeypair.publicKey())],
  );
  const isWhitelisted = raw ? (StellarSdk.scValToNative(raw) as boolean) : false;

  if (isWhitelisted) {
    console.log('[startup] Admin already whitelisted in nullifier-registry.');
    return;
  }

  console.log('[startup] Whitelisting admin in nullifier-registry (one-time)…');
  await stellar.invokeContract(
    nullifierRegistry,
    'add_to_whitelist',
    [StellarService.address(adminKeypair.publicKey())],
    adminKeypair,
  );
  console.log('[startup] Admin whitelisted successfully.');
}

async function main(): Promise<void> {
  try {
    await ensureAdminWhitelisted();
  } catch (err) {
    console.warn(
      '[startup] Whitelist setup skipped (contracts may not be deployed yet):',
      (err as Error).message,
    );
  }

  app.listen(PORT, () => {
    console.log(`Lumina API server running on http://localhost:${PORT}`);
  });
}

main();
