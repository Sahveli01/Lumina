import { Router, Request, Response } from 'express';

const healthRouter = Router();

/**
 * GET /health
 *
 * Returns the current service status, deployed contract IDs, and
 * the active Stellar network passphrase.  Used by the E2E test suite
 * and the pre-flight health-check script.
 */
healthRouter.get('/', (_req: Request, res: Response) => {
  res.json({
    status:    'ok',
    timestamp: new Date().toISOString(),
    version:   '2.0.0',
    contracts: {
      lumina_core:        process.env.LUMINA_CORE_CONTRACT_ID         ?? null,
      nullifier_registry: process.env.NULLIFIER_REGISTRY_CONTRACT_ID  ?? null,
      risk_oracle:        process.env.RISK_ORACLE_CONTRACT_ID         ?? null,
      liquidity_pools:    process.env.LIQUIDITY_POOLS_CONTRACT_ID     ?? null,
    },
    stellar_network: process.env.STELLAR_NETWORK_PASSPHRASE
      ?? 'Test SDF Network ; September 2015',
  });
});

export default healthRouter;
